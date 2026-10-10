//! Symphony provider accounts, OAuth, credential slots, and quota probes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentAuthKind, AgentLoginMode, AgentLoginPoll,
    AgentLoginStart, AgentLoginStatus, AgentUsageWindow, HarnessId,
};

use crate::repos::home_dir;
use crate::{EngineError, new_id, now_ms};

mod oauth;
mod stores;
mod symphony;
mod usage;

// Claude Code's public OAuth client (the one the CLI itself uses for the manual
// "paste the code" flow — no secret involved, PKCE carries the proof).
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_REDIRECT: &str = "https://console.anthropic.com/oauth/code/callback";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference";

const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// A forced list re-probes an account only when its last attempt is older
/// than this — the page's paint-then-refresh pair and Refresh mashing must
/// not multiply provider calls (Anthropic's usage endpoint 429s eagerly).
const FORCED_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// An abandoned login flow (dialog dismissed without Cancel) is reaped past this.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// A failed identity lookup is retried no sooner than this — an offline
/// device must not call the profile endpoint on every list.
const IDENTITY_RETRY: Duration = Duration::from_secs(5 * 60);

/// A remembered identity lookup (see `Inner::identities`).
#[derive(Clone)]
enum IdentityLookup {
    Known(String, SlotProfile),
    Failed(Instant),
}

/// Filesystem knobs — env-resolved in production ([`AgentAccountsConfig::detect`]),
/// explicit in tests.
#[derive(Debug, Clone)]
pub struct AgentAccountsConfig {
    /// Engine data dir; credential snapshots live under agent-accounts/.
    pub data_dir: PathBuf,
    /// Historical generated-image import root; no Codex runtime is launched.
    pub codex_home: PathBuf,
    /// Native Symphony credentials and provider environment.
    pub symphony_home: PathBuf,
}

impl AgentAccountsConfig {
    /// Resolve Symphony credentials and the historical image import root.
    pub fn detect(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            codex_home: std::env::var_os("CODEX_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home_dir().join(".codex")),
            symphony_home: zeron_harness::SymphonyHarness::credential_home(),
        }
    }

    /// Test configuration isolated from all real provider credentials.
    #[doc(hidden)]
    pub fn isolated(root: &Path) -> Self {
        Self {
            data_dir: root.join("data"),
            codex_home: root.join("codex"),
            symphony_home: root.join("symphony"),
        }
    }

    fn root_dir(&self) -> PathBuf {
        self.data_dir.join("agent-accounts")
    }

    /// [`Self::root_dir`], created (or tightened) owner-only — it holds slot
    /// files and every throwaway sign-in home.
    fn private_root(&self) -> std::io::Result<PathBuf> {
        let root = self.root_dir();
        private_dir(&root)?;
        Ok(root)
    }

    fn usage_cache_file(&self) -> PathBuf {
        self.root_dir().join("usage-cache.json")
    }
}

/// `dir` created (parents as needed) and itself made owner-only — 0700 on
/// Unix, an existing looser dir included. Sign-in homes hold whatever a CLI
/// writes there (fresh tokens, in modes the CLI picks), so the directory is
/// the boundary that keeps other local users out.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => {}
            Err(err) => return Err(err),
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

// ── slot storage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlotProfile {
    email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    auth_kind: AgentAuthKind,
}

/// One saved login (`{slotId}.json`), same field surface as zeron's slot files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    id: String,
    harness: HarnessId,
    /// The provider-side identity the slot is keyed by (account uuid/email).
    account_key: String,
    profile: SlotProfile,
    /// Claude: the `.credentials.json`/Keychain payload. Codex: `auth.json`.
    credentials: serde_json::Value,
    /// Claude only: `{oauthAccount, userID}` merged into `~/.claude.json` on swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_config: Option<serde_json::Value>,
    saved_at: i64,
    /// First time this account was saved — the STABLE sort key, so switching the
    /// active account (which re-snapshots and bumps `saved_at`) never reorders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
    /// Agents that keep one login PER model provider in one store (OpenCode,
    /// Pi, Hermes): the store key this slot's `credentials` entry lives
    /// under (`openai`, `anthropic`, …). Swapping rewrites that entry only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    store_key: Option<String>,
}

/// A live detection result (before it's persisted into a slot).
#[derive(Debug, Clone)]
struct Detected {
    account_key: String,
    profile: SlotProfile,
    /// `None` ⇒ we know a login exists but couldn't read the secret.
    credentials: Option<serde_json::Value>,
    claude_config: Option<serde_json::Value>,
    /// See [`Slot::store_key`].
    store_key: Option<String>,
    /// `false` when the store carries no identity (Devin's bare API key):
    /// the profile is a placeholder, and a snapshot keeps whatever identity
    /// a usage probe already wrote into the slot.
    identity_known: bool,
}

impl Detected {
    /// A login whose store names who it is — the common case.
    fn known(account_key: String, profile: SlotProfile, credentials: serde_json::Value) -> Self {
        Self {
            account_key,
            profile,
            credentials: Some(credentials),
            claude_config: None,
            store_key: None,
            identity_known: true,
        }
    }

    fn keyed(mut self, store_key: &str) -> Self {
        self.store_key = Some(store_key.to_string());
        self
    }
}

// ── login flows ─────────────────────────────────────────────────────────────

enum LoginFlow {
    SymphonyPaste {
        verifier: String,
        state: String,
        started_at: Instant,
    },
    Task {
        harness: HarnessId,
        started_at: Instant,
        state: Arc<Mutex<TaskLoginState>>,
        /// aborting drops the sign-in future, which kills its agent child.
        handle: tokio::task::JoinHandle<()>,
        /// A FIXED loopback port the sign-in holds (ChatGPT's 1455) — a new
        /// sign-in needing the same port supersedes this one.
        port: Option<u16>,
    },
}

#[derive(Default)]
struct TaskLoginState {
    url: Option<String>,
    message: Option<String>,
    outcome: Option<Result<(), String>>,
    /// The device a remote login's callback is forwarded for.
    requester: Option<String>,
}

impl LoginFlow {
    fn started_at(&self) -> Instant {
        match self {
            LoginFlow::SymphonyPaste { started_at, .. } | LoginFlow::Task { started_at, .. } => {
                *started_at
            }
        }
    }
}

// ── service ─────────────────────────────────────────────────────────────────

/// One live usage probe: rate-limit windows plus the plan label the provider
/// reported alongside them (Codex's usage endpoint carries a live `plan_type`,
/// which supersedes the login-time JWT claim — plan changes show up here
/// without a re-login). Claude's usage endpoint has no plan field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageSnapshot {
    windows: Vec<AgentUsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_label: Option<String>,
}

/// Why a usage probe produced no windows. Drives the backoff and the reason
/// the UI shows instead of a bare "Usage unavailable". Persisted with the
/// usage cache so a relaunch neither forgets a Retry-After nor the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum ProbeError {
    /// 401/403 — the token was rejected; the ONLY status worth a refresh.
    #[serde(rename_all = "camelCase")]
    Unauthorized { status: u16 },
    /// 429 — `retry_after_secs` from the `Retry-After` header when sent.
    #[serde(rename_all = "camelCase")]
    RateLimited { retry_after_secs: Option<u64> },
    /// Any other non-2xx: 5xx outages, a 404 from a moved endpoint, ….
    #[serde(rename_all = "camelCase")]
    Http {
        status: u16,
        retry_after_secs: Option<u64>,
    },
    /// Never reached the provider (DNS/connect/TLS) or it didn't answer in time.
    Network { timeout: bool },
    /// A 2xx whose body didn't carry the windows we parse (schema drift).
    Schema,
    /// The slot holds nothing probeable.
    #[serde(rename_all = "camelCase")]
    NoCredentials { why: NoCredentials },
    /// The credentials name a server (issuer, API or portal url) outside the
    /// provider's known hosts: nothing was sent.
    UntrustedEndpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum NoCredentials {
    /// Codex API-key mode: no ChatGPT rate windows exist.
    ApiKey,
    /// The slot has no access token / key at all.
    Missing,
    /// Cursor's minted key is past its expiry.
    KeyExpired,
    /// The provider behind this login has no usage view zeron can read
    /// (a Hermes API key for some other vendor, Pi's Copilot session).
    Unsupported,
}

impl ProbeError {
    /// Short machine class for logs.
    fn class(&self) -> &'static str {
        match self {
            ProbeError::Unauthorized { .. } => "unauthorized",
            ProbeError::RateLimited { .. } => "rate-limited",
            ProbeError::Http { status, .. } if *status >= 500 => "server-error",
            ProbeError::Http { .. } => "http-error",
            ProbeError::Network { timeout: true } => "timeout",
            ProbeError::Network { .. } => "network",
            ProbeError::Schema => "schema",
            ProbeError::NoCredentials { .. } => "no-credentials",
            ProbeError::UntrustedEndpoint => "untrusted-endpoint",
        }
    }

    fn status(&self) -> Option<u16> {
        match self {
            ProbeError::Unauthorized { status } | ProbeError::Http { status, .. } => Some(*status),
            ProbeError::RateLimited { .. } => Some(429),
            _ => None,
        }
    }

    /// How long to leave the provider alone after this failure. A server-sent
    /// Retry-After wins (clamped so a bogus value can't stall usage for a
    /// day, nor a 0 turn into hammering).
    fn backoff(&self) -> Duration {
        const MIN: u64 = 30;
        match self {
            ProbeError::RateLimited { retry_after_secs } => {
                Duration::from_secs(retry_after_secs.unwrap_or(5 * 60).clamp(MIN, 60 * 60))
            }
            ProbeError::Http {
                status,
                retry_after_secs,
            } if *status >= 500 => {
                Duration::from_secs(retry_after_secs.unwrap_or(60).clamp(MIN, 30 * 60))
            }
            // 404/400…: the request itself is wrong — retrying soon won't help.
            ProbeError::Http { .. } | ProbeError::Schema => Duration::from_secs(15 * 60),
            ProbeError::Network { .. } => Duration::from_secs(MIN),
            // Lifted early when the slot's credentials change (re-login, the
            // CLI refreshing its token) — see `UsageEntry::probe_due`.
            ProbeError::Unauthorized { .. }
            | ProbeError::NoCredentials { .. }
            | ProbeError::UntrustedEndpoint => Duration::from_secs(10 * 60),
        }
    }
}

/// Per-account usage state, persisted to `agent-accounts/usage-cache.json`
/// so the first list after launch paints the last known windows instantly.
/// All times are epoch millis (they must survive a restart).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageEntry {
    /// The last SUCCESSFUL probe — kept through later failures (a 429 must
    /// not blank meters that were right a minute ago).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<UsageSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fetched_at: Option<i64>,
    /// The last probe's failure; cleared by the next success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ProbeError>,
    /// Last probe attempt, success or failure.
    #[serde(default)]
    checked_at: i64,
    /// No probe before this (Retry-After / backoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_at: Option<i64>,
    /// Fingerprint of the credentials the last failure was against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credentials: Option<String>,
}

impl UsageEntry {
    /// Whether a forced list should probe this account now.
    fn probe_due(&self, credentials: &str, now: i64) -> bool {
        if now - self.checked_at < FORCED_MIN_INTERVAL.as_millis() as i64 {
            return false;
        }
        match (self.retry_at, &self.error) {
            (Some(at), Some(error)) if now < at => {
                // A token rejection (or a credential-less slot) is about THAT
                // credential: new credentials deserve a probe right away. A
                // rate limit or outage is not, so it holds regardless.
                matches!(
                    error,
                    ProbeError::Unauthorized { .. }
                        | ProbeError::NoCredentials { .. }
                        | ProbeError::UntrustedEndpoint
                ) && self.credentials.as_deref() != Some(credentials)
            }
            _ => true,
        }
    }

    fn record(&mut self, result: Result<UsageSnapshot, ProbeError>, credentials: String, now: i64) {
        self.checked_at = now;
        match result {
            Ok(usage) => {
                self.usage = Some(usage);
                self.fetched_at = Some(now);
                self.error = None;
                self.retry_at = None;
                self.credentials = None;
            }
            Err(error) => {
                self.retry_at = Some(now + error.backoff().as_millis() as i64);
                self.error = Some(error);
                self.credentials = Some(credentials);
            }
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageCacheFile {
    #[serde(default)]
    entries: HashMap<String, UsageEntry>,
}

/// Provider endpoints in production, local endpoints in tests.
#[derive(Debug, Clone)]
struct ProbeEndpoints {
    claude_usage: String,
    claude_profile: String,
    symphony_anthropic_token: String,
    symphony_xai_device: String,
    symphony_xai_token: String,
    codex_usage: String,
    /// Grok's billing view (`/v1/billing?format=credits`).
    grok_usage: String,
    /// OpenAI's OAuth root (`/oauth/authorize`, `/oauth/token`).
    openai_auth: String,
    /// The ChatGPT sign-in's loopback port — FIXED at 1455 by the client
    /// registration; tests bind any free port (0).
    openai_port: u16,
}

impl Default for ProbeEndpoints {
    fn default() -> Self {
        Self {
            claude_usage: CLAUDE_USAGE_URL.into(),
            claude_profile: CLAUDE_PROFILE_URL.into(),
            symphony_anthropic_token: "https://api.anthropic.com/v1/oauth/token".into(),
            symphony_xai_device: "https://auth.x.ai/oauth2/device/code".into(),
            symphony_xai_token: "https://auth.x.ai/oauth2/token".into(),
            codex_usage: CODEX_USAGE_URL.into(),
            grok_usage: usage::GROK_USAGE_URL.into(),
            openai_auth: oauth::OPENAI_AUTH.into(),
            openai_port: oauth::OPENAI_LOOPBACK_PORT,
        }
    }
}

struct Inner {
    config: AgentAccountsConfig,
    http: reqwest::Client,
    endpoints: ProbeEndpoints,
    /// Serializes operations that inspect and then mutate live credential
    /// stores and slots. Without this, a concurrent list can snapshot stale
    /// credentials over a freshly signed-in slot, or a switch can race a
    /// removal and cause the wrong live account to be signed out.
    ops: tokio::sync::Mutex<()>,
    flows: Mutex<HashMap<String, LoginFlow>>,
    /// `"{harness}:{accountKey}"` → usage state; mirrored to disk.
    usage: Mutex<HashMap<String, UsageEntry>>,
    /// Accounts with a usage probe in flight — overlapping forced lists (the
    /// page's paint-then-refresh, two open windows) share one probe.
    inflight_probes: Mutex<std::collections::HashSet<String>>,
    /// Slots with a token refresh in flight — a second refresh of the same
    /// (commonly single-use) refresh token would revoke the family.
    /// Callback ports of logins run for another device (see module docs).
    callback_routes: zeron_preview::login::CallbackRoutes,
    /// Who an opaque live token belongs to (Pi's Claude login, a Copilot
    /// token), by token fingerprint — one profile call per token, not per
    /// list; a failed lookup waits [`IDENTITY_RETRY`] before the next. See
    /// [`stores`].
    identities: Mutex<HashMap<String, IdentityLookup>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Releases single-flight markers on drop. RPC handlers are aborted when the
/// client cancels or disconnects, so a marker removed only after the await
/// would stay forever — and that account would never probe (or refresh)
/// again until restart.
struct InflightGuard<'a> {
    set: &'a Mutex<std::collections::HashSet<String>>,
    keys: Vec<String>,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        let mut set = lock(self.set);
        for key in &self.keys {
            set.remove(key);
        }
    }
}

#[derive(Clone)]
pub struct AgentAccounts {
    inner: Arc<Inner>,
}

impl AgentAccounts {
    pub fn new(config: AgentAccountsConfig) -> Self {
        Self::with_endpoints(config, ProbeEndpoints::default(), Default::default())
    }

    /// [`Self::new`], publishing remote logins' callback ports to `routes`
    /// (the engine's P2P service, which serves them to the requester).
    pub fn with_callback_routes(
        config: AgentAccountsConfig,
        routes: zeron_preview::login::CallbackRoutes,
    ) -> Self {
        Self::with_endpoints(config, ProbeEndpoints::default(), routes)
    }

    fn with_endpoints(
        config: AgentAccountsConfig,
        endpoints: ProbeEndpoints,
        callback_routes: zeron_preview::login::CallbackRoutes,
    ) -> Self {
        // Startup sweep: a previous process that crashed mid-login leaves
        // `.login-<uuid>` throwaway CODEX_HOME dirs — each may hold live OAuth
        // tokens — with no owner to clean them. Reclaim them at boot.
        let root = config.root_dir();
        // A root from an older build may be group/world-readable: tighten it.
        if root.is_dir()
            && let Err(err) = private_dir(&root)
        {
            tracing::warn!(error = %err, "could not make the agent-accounts dir private");
        }
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".login-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        // Last known usage from the previous run — a missing or torn file is
        // just a cold cache.
        let usage = std::fs::read_to_string(config.usage_cache_file())
            .ok()
            .and_then(|raw| serde_json::from_str::<UsageCacheFile>(&raw).ok())
            .map(|file| file.entries)
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                config,
                http,
                endpoints,
                ops: tokio::sync::Mutex::new(()),
                flows: Mutex::new(HashMap::new()),
                usage: Mutex::new(usage),
                inflight_probes: Mutex::new(std::collections::HashSet::new()),
                callback_routes,
                identities: Mutex::new(HashMap::new()),
            }),
        }
    }

    // ── list ────────────────────────────────────────────────────────────────

    /// Detect the CLIs, auto-snapshot the live logins, and assemble the view.
    ///
    /// Usage is stale-while-revalidate: every list serves each account's last
    /// good probe (memory, seeded from disk) with its fetch time, so a plain
    /// list is offline-fast. `force_usage` additionally re-probes — all
    /// accounts concurrently — skipping any still inside a Retry-After /
    /// backoff window or probed in the last [`FORCED_MIN_INTERVAL`].
    pub async fn list(&self, force_usage: bool) -> Result<AgentAccountsSnapshot, EngineError> {
        let _ops = self.inner.ops.lock().await;
        self.list_locked(force_usage).await
    }

    /// Caller holds [`Inner::ops`].
    async fn list_locked(&self, force_usage: bool) -> Result<AgentAccountsSnapshot, EngineError> {
        let harness = HarnessId::Symphony;
        let mut active = std::collections::HashSet::new();
        for detected in self.detect_symphony().await {
            active.insert(detected.account_key.clone());
            self.snapshot_detected(harness, &detected)?;
        }
        let slots = self.read_slots(harness);
        if force_usage {
            let targets: Vec<_> = slots
                .iter()
                .map(|slot| (harness, slot, active.contains(&slot.account_key)))
                .collect();
            self.refresh_usage(&targets).await;
        }
        let now = now_ms();
        let usage = lock(&self.inner.usage).clone();
        let accounts = slots
            .iter()
            .map(|slot| {
                let is_active = active.contains(&slot.account_key);
                let entry = usage.get(&usage_key(harness, &slot.account_key));
                let snapshot = entry.and_then(|entry| entry.usage.as_ref());
                AgentAccount {
                    id: slot.id.clone(),
                    harness,
                    email: Some(slot.profile.email.clone()),
                    plan_label: snapshot
                        .and_then(|usage| usage.plan_label.clone())
                        .or_else(|| slot.profile.plan.clone()),
                    active: is_active,
                    usage_windows: snapshot
                        .map(|usage| usage.windows.clone())
                        .unwrap_or_default(),
                    usage_fetched_at: entry.and_then(|entry| entry.fetched_at),
                    usage_error: entry.and_then(|entry| {
                        usage_error_message(
                            harness,
                            slot.store_key.as_deref(),
                            is_active,
                            entry.error.as_ref()?,
                            entry,
                            now,
                        )
                    }),
                    display_name: slot.profile.display_name.clone(),
                    organization: slot.profile.organization.clone(),
                    auth_kind: Some(slot.profile.auth_kind),
                    switchable: true,
                    saved_at: Some(slot.saved_at),
                    provider: provider_group(harness, slot.store_key.as_deref()),
                }
            })
            .collect();
        Ok(AgentAccountsSnapshot {
            accounts,
            warnings: Vec::new(),
        })
    }

    // ── swap ────────────────────────────────────────────────────────────────

    /// Swap the CLI's live login to a saved slot. Detection runs first, so the
    /// CURRENT login is snapshotted into its slot before being overwritten (the
    /// claude-swap trick — a swap never strands the session it replaces).
    ///
    /// Refused for Hermes (and by [`Self::forget`]): Hermes owns its
    /// credential pool and rotates through it itself — a running Hermes
    /// would write its own order back over any change — so zeron keeps it
    /// read-only (see [`stores`]).
    pub async fn activate(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        if harness != HarnessId::Symphony {
            return Err(EngineError::Other(
                "This fork only manages Symphony provider accounts.".into(),
            ));
        }
        let _ops = self.inner.ops.lock().await;
        self.list_locked(false).await?;
        let slot = self
            .read_slots(harness)
            .into_iter()
            .find(|s| s.id == account_id)
            .ok_or_else(|| {
                EngineError::Other(
                    "That saved login no longer exists — refresh and try again.".into(),
                )
            })?;
        let key = slot
            .store_key
            .as_deref()
            .ok_or_else(|| EngineError::Other("That saved login names no provider.".into()))?;
        self.write_keyed_entry(harness, key, Some(&slot.credentials))?;
        self.list_locked(false).await
    }

    /// The live login's account key, from the identity alone (no secret read
    /// for Claude). `store_key` names the entry for agents that keep one
    /// login per model provider (OpenCode, Pi); an entry that is there but
    /// can't be identified has no key.
    async fn live_account_key(
        &self,
        harness: HarnessId,
        store_key: Option<&str>,
    ) -> Option<String> {
        if harness != HarnessId::Symphony {
            return None;
        }
        self.detect_keyed_entry(harness, store_key?)
            .await
            .flatten()
            .map(|d| d.account_key)
    }

    /// Whether a per-provider agent has a live entry under `store_key` at
    /// all, identified or not — a live login zeron can't identify is still
    /// never replaced unasked.
    fn has_live_entry(&self, harness: HarnessId, store_key: Option<&str>) -> bool {
        harness == HarnessId::Symphony
            && store_key.is_none_or(|key| self.live_keyed_entry(harness, key).is_some())
    }

    /// A fresh sign-in replaces the live login when it IS the live account
    /// (a re-login — otherwise the next list would snapshot the old, possibly
    /// revoked, live tokens straight back over the fresh slot) or when there
    /// is no live login at all (nothing to strand; "add" must mean "works").
    /// Any other live login stays untouched — switching remains explicit.
    async fn adopt_if_live(&self, slot: &Slot) -> Result<(), EngineError> {
        let key = slot
            .store_key
            .as_deref()
            .ok_or_else(|| EngineError::Other("That login names no provider.".into()))?;
        let live = self.live_account_key(slot.harness, Some(key)).await;
        if (live.is_some() || self.has_live_entry(slot.harness, Some(key)))
            && live.as_deref() != Some(slot.account_key.as_str())
        {
            return Ok(());
        }
        self.write_keyed_entry(slot.harness, key, Some(&slot.credentials))
    }

    /// Sign the CLI's live login out, so removing its slot sticks.
    async fn sign_out(
        &self,
        harness: HarnessId,
        store_key: Option<&str>,
        expected_account_key: &str,
    ) -> Result<(), EngineError> {
        if self.live_account_key(harness, store_key).await.as_deref() != Some(expected_account_key)
        {
            return Err(EngineError::Other(
                "The live login changed while it was being removed — refresh and try again.".into(),
            ));
        }
        let key =
            store_key.ok_or_else(|| EngineError::Other("That login names no provider.".into()))?;
        self.write_keyed_entry(harness, key, None)
    }

    // ── forget ──────────────────────────────────────────────────────────────

    pub async fn forget(
        &self,
        harness: HarnessId,
        account_id: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        if harness != HarnessId::Symphony {
            return Err(EngineError::Other(
                "This fork only manages Symphony provider accounts.".into(),
            ));
        }
        // Reject anything that isn't a slot id (16 lowercase hex) BEFORE touching
        // the filesystem: `account_id` is a raw RPC string that becomes a path,
        // so a crafted id (`../../…`) must never reach `remove_file`.
        if account_id.len() != 16
            || !account_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(EngineError::Other("Unknown account.".into()));
        }
        if harness == HarnessId::Hermes {
            // Hermes' rows are its own pool, not zeron slots.
            return Err(EngineError::Other(
                "Hermes keeps this login in its own credential pool — remove it with \
                 `hermes auth remove`."
                    .into(),
            ));
        }
        let _ops = self.inner.ops.lock().await;
        let snapshot = self.list_locked(false).await?;
        let row = snapshot
            .accounts
            .iter()
            .find(|a| a.harness == harness && a.id == account_id);
        // Per-provider agents (OpenCode, Pi): the store entry this row is.
        let store_key = row.and_then(|a| a.provider.clone());
        if row.is_some_and(|a| a.active) {
            // Removing the live login signs the CLI out — dropping only the
            // slot would re-detect (and re-snapshot) it on the next list, and
            // the only account must stay removable.
            let expected = self
                .live_account_key(harness, store_key.as_deref())
                .await
                .ok_or_else(|| {
                    EngineError::Other(
                    "The live login changed while it was being removed — refresh and try again."
                        .into(),
                )
                })?;
            if slot_id_for(harness, &expected) != account_id {
                return Err(EngineError::Other(
                    "The live login changed while it was being removed — refresh and try again."
                        .into(),
                ));
            }
            self.sign_out(harness, store_key.as_deref(), &expected)
                .await?;
        }
        let file = self.slots_dir(harness)?.join(format!("{account_id}.json"));
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        self.list_locked(false).await
    }

    // ── add-account OAuth flows ─────────────────────────────────────────────

    pub async fn start_login(&self, harness: HarnessId) -> Result<AgentLoginStart, EngineError> {
        self.start_login_for(harness, None).await
    }

    /// [`Self::start_login`] on behalf of `requester` — another device, whose
    /// browser finishes the sign-in. The login's loopback callback port is
    /// published for that device alone, for as long as the login runs.
    pub async fn start_login_for(
        &self,
        harness: HarnessId,
        requester: Option<&str>,
    ) -> Result<AgentLoginStart, EngineError> {
        self.start_login_with(harness, None, requester).await
    }

    /// [`Self::start_login_for`] for one model provider of an agent that
    /// keeps a login per provider (`provider`: OpenCode's `openai` /
    /// `github-copilot`, Pi's `openai-codex`, Hermes' `openai-codex` /
    /// `nous`); `None` picks the agent's default.
    pub async fn start_login_with(
        &self,
        harness: HarnessId,
        provider: Option<&str>,
        requester: Option<&str>,
    ) -> Result<AgentLoginStart, EngineError> {
        self.sweep_flows();
        let provider = provider.filter(|p| !p.is_empty());
        if harness != HarnessId::Symphony {
            return Err(EngineError::Other(
                "This fork only supports Symphony provider sign-in.".into(),
            ));
        }
        let mut start = self
            .start_symphony_login(provider.unwrap_or("openai"))
            .await?;
        if start.callback_port.is_none() {
            start.callback_port = loopback_port(&start.url);
        }
        if let (Some(requester), Some(port)) = (requester, start.callback_port) {
            self.inner
                .callback_routes
                .register(&start.login_id, port, requester, FLOW_TTL);
        }
        Ok(start)
    }

    /// Supersede — and reap — any pending spawned flow for `harness` (codex:
    /// `codex login` binds a fixed loopback OAuth port, so a lingering flow
    /// makes every retry exit on EADDRINUSE; cursor: one flow is simply the
    /// sane state).
    fn reap_spawned_flows(&self, harness: HarnessId) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| {
                matches!(
                    f,
                    LoginFlow::Task { harness: h, .. }
                        if *h == harness
                )
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    /// Cancel every pending flow holding the fixed loopback `port` — the
    /// ChatGPT sign-ins (codex's, OpenCode's, Pi's) all need 1455, and a
    /// lingering one makes every retry fail to bind.
    fn reap_port_flows(&self, port: u16) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| match f {
                LoginFlow::Task { port: Some(p), .. } => *p == port,
                _ => false,
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    /// Exchange the pasted `code#state` for tokens and save the account as a
    /// slot (see [`Self::adopt_if_live`] for when it also becomes live).
    pub async fn complete_login(
        &self,
        login_id: &str,
        code: &str,
    ) -> Result<AgentAccountsSnapshot, EngineError> {
        self.sweep_flows();
        match self.complete_symphony_paste_login(login_id, code).await {
            Some(result) => {
                result?;
                self.list(false).await
            }
            None => Err(EngineError::Other(
                "This sign-in does not accept pasted credentials.".into(),
            )),
        }
    }

    pub async fn poll_login(&self, login_id: &str) -> Result<AgentLoginPoll, EngineError> {
        self.sweep_flows();
        if let Some(poll) = self.poll_task_login(login_id) {
            return Ok(poll);
        }
        match lock(&self.inner.flows).get(login_id) {
            Some(LoginFlow::SymphonyPaste { .. }) => Ok(AgentLoginPoll {
                status: AgentLoginStatus::Pending,
                message: None,
                url: None,
                callback_port: None,
            }),
            _ => Err(EngineError::Other(
                "This sign-in attempt expired — start again.".into(),
            )),
        }
    }

    /// Poll an engine-driven sign-in; `None` when `login_id` isn't one. A
    /// page first learned here (Antigravity's) publishes its callback port
    /// for a remote requester before the poll hands the url out.
    fn poll_task_login(&self, login_id: &str) -> Option<AgentLoginPoll> {
        let state = match lock(&self.inner.flows).get(login_id) {
            Some(LoginFlow::Task { state, .. }) => state.clone(),
            _ => return None,
        };
        let poll = {
            let state = lock(&state);
            match &state.outcome {
                None => {
                    let callback_port = state.url.as_deref().and_then(loopback_port);
                    if let (Some(requester), Some(port)) = (&state.requester, callback_port)
                        && !self.inner.callback_routes.is_registered(login_id)
                    {
                        self.inner
                            .callback_routes
                            .register(login_id, port, requester, FLOW_TTL);
                    }
                    return Some(AgentLoginPoll {
                        status: AgentLoginStatus::Pending,
                        message: state.message.clone(),
                        url: state.url.clone(),
                        callback_port,
                    });
                }
                Some(Ok(())) => AgentLoginPoll {
                    status: AgentLoginStatus::Done,
                    message: None,
                    url: None,
                    callback_port: None,
                },
                Some(Err(message)) => AgentLoginPoll {
                    status: AgentLoginStatus::Error,
                    message: Some(zeron_harness::redact::redact_output(message)),
                    url: None,
                    callback_port: None,
                },
            }
        };
        self.remove_flow(login_id);
        Some(poll)
    }

    /// Drop a flow's bookkeeping, and with it any callback route it published.
    fn remove_flow(&self, login_id: &str) -> Option<LoginFlow> {
        self.inner.callback_routes.remove(login_id);
        lock(&self.inner.flows).remove(login_id)
    }

    /// Drop a flow: kill a pending login child (`codex login` holds the fixed
    /// loopback OAuth port; the cursor shim polls Cursor's backend) and
    /// reclaim its throwaway home dir. Idempotent.
    pub fn cancel_login(&self, login_id: &str) {
        if let Some(LoginFlow::Task { handle, .. }) = self.remove_flow(login_id) {
            handle.abort();
        }
    }

    /// Engine shutdown: kill any in-flight login child so an orphan `codex login`
    /// can't survive the restart and brick the next attempt.
    pub fn shutdown(&self) {
        let ids: Vec<String> = lock(&self.inner.flows).keys().cloned().collect();
        for id in ids {
            self.cancel_login(&id);
        }
    }

    /// Lazy TTL sweep (zeron uses a background fiber; native reaps on the next
    /// accounts call — same bound, no standing task).
    fn sweep_flows(&self) {
        let stale: Vec<String> = lock(&self.inner.flows)
            .iter()
            .filter(|(_, f)| f.started_at().elapsed() > FLOW_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.cancel_login(&id);
        }
    }

    /// Persist a detected login into its slot (refreshing stored tokens).
    fn snapshot_detected(&self, harness: HarnessId, d: &Detected) -> Result<(), EngineError> {
        let Some(credentials) = &d.credentials else {
            return Ok(());
        };
        let id = slot_id_for(harness, &d.account_key);
        // A store without identity (Devin's bare key) must not overwrite the
        // identity a usage probe already wrote into the slot.
        let profile = match d.identity_known {
            true => d.profile.clone(),
            false => self
                .read_slot(harness, &id)
                .map(|existing| existing.profile)
                .unwrap_or_else(|| d.profile.clone()),
        };
        self.write_slot(&Slot {
            id,
            harness,
            account_key: d.account_key.clone(),
            profile,
            credentials: credentials.clone(),
            claude_config: d.claude_config.clone(),
            saved_at: now_ms(),
            created_at: None,
            store_key: d.store_key.clone(),
        })
    }

    /// One saved slot by id (`None` when missing or unparseable).
    fn read_slot(&self, harness: HarnessId, id: &str) -> Option<Slot> {
        let file = self.slots_dir(harness).ok()?.join(format!("{id}.json"));
        serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
    }

    // ── slot files ──────────────────────────────────────────────────────────

    fn slots_dir(&self, harness: HarnessId) -> Result<PathBuf, EngineError> {
        let dir = self
            .inner
            .config
            .private_root()?
            .join(harness_slug(harness));
        private_dir(&dir)?;
        Ok(dir)
    }

    fn read_slots(&self, harness: HarnessId) -> Vec<Slot> {
        let Ok(dir) = self.slots_dir(harness) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut slots: Vec<Slot> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // One malformed slot file must skip THAT slot, not brick the page.
            if let Some(slot) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Slot>(&raw).ok())
            {
                slots.push(slot);
            }
        }
        // Creation order — stable across switches (saved_at churns on every
        // auto-snapshot; created_at never does). Slot id breaks creation-time
        // ties: two logins saved in the same millisecond otherwise land in
        // read_dir order, which is filesystem-arbitrary for UUID-named files
        // and reshuffles the page between restarts (issue #161).
        slots.sort_by(|a, b| {
            (a.created_at.unwrap_or(a.saved_at), &a.id)
                .cmp(&(b.created_at.unwrap_or(b.saved_at), &b.id))
        });
        slots
    }

    fn write_slot(&self, slot: &Slot) -> Result<(), EngineError> {
        let file = self
            .slots_dir(slot.harness)?
            .join(format!("{}.json", slot.id));
        let existing: Option<Slot> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let mut full = slot.clone();
        full.created_at = existing
            .and_then(|e| e.created_at.or(Some(e.saved_at)))
            .or(slot.created_at)
            .or_else(|| {
                // A brand-new slot: stamp it strictly after every sibling, so
                // two logins inside the same millisecond still list in the
                // order they were saved (creation order is the page's sort
                // key; ms-resolution ties otherwise fall to read_dir order).
                let floor = self
                    .read_slots(slot.harness)
                    .iter()
                    .map(|s| s.created_at.unwrap_or(s.saved_at))
                    .max()
                    .map(|newest| newest + 1)
                    .unwrap_or(slot.saved_at);
                Some(floor.max(slot.saved_at))
            });
        let json = serde_json::to_string_pretty(&full)
            .map_err(|e| EngineError::Other(format!("serialize slot: {e}")))?;
        // Atomic + 0600 from birth: tokens must never be world-readable, and a
        // crash mid-write must never leave torn JSON.
        write_file_atomic(&file, json.as_bytes(), true)
    }

    // ── remaining usage ─────────────────────────────────────────────────────

    /// Probe every due account concurrently and fold the outcomes into the
    /// usage cache (then disk). Accounts inside a backoff window, probed a
    /// moment ago, or already being probed by an overlapping list are left
    /// alone — they keep serving their last known usage.
    async fn refresh_usage(&self, targets: &[(HarnessId, &Slot, bool)]) {
        let now = now_ms();
        let mut probes = Vec::new();
        let mut claimed = Vec::new();
        {
            let usage = lock(&self.inner.usage);
            let mut inflight = lock(&self.inner.inflight_probes);
            for &(harness, slot, active) in targets {
                let key = usage_key(harness, &slot.account_key);
                let credentials = credentials_fingerprint(&slot.credentials);
                if let Some(entry) = usage.get(&key)
                    && !entry.probe_due(&credentials, now)
                {
                    tracing::debug!(
                        provider = harness_slug(harness),
                        slot = %slot.id,
                        retry_at = ?entry.retry_at,
                        "usage probe skipped (backoff or fresh)"
                    );
                    continue;
                }
                if !inflight.insert(key.clone()) {
                    continue;
                }
                claimed.push(key.clone());
                probes.push(async move {
                    let result = self.probe_usage(harness, slot, active).await;
                    (key, credentials, result)
                });
            }
        }
        let _release = InflightGuard {
            set: &self.inner.inflight_probes,
            keys: claimed,
        };
        if probes.is_empty() {
            return;
        }
        let results = futures::future::join_all(probes).await;
        let now = now_ms();
        let mut usage = lock(&self.inner.usage);
        for (key, credentials, result) in results {
            usage
                .entry(key)
                .or_default()
                .record(result, credentials, now);
        }
        // Drop entries for accounts that no longer have a slot (forgotten).
        let live: std::collections::HashSet<String> = targets
            .iter()
            .map(|(harness, slot, _)| usage_key(*harness, &slot.account_key))
            .collect();
        usage.retain(|key, _| live.contains(key));
        // Persist under the lock: overlapping lists must not interleave
        // writes of the same file.
        let file = UsageCacheFile {
            entries: usage.clone(),
        };
        let persisted = serde_json::to_vec_pretty(&file)
            .map_err(|e| EngineError::Other(e.to_string()))
            .and_then(|json| {
                self.inner.config.private_root()?;
                write_file_atomic(&self.inner.config.usage_cache_file(), &json, true)
            });
        if let Err(err) = persisted {
            tracing::warn!(error = %err, "agent usage cache write failed");
        }
    }

    /// One account's probe. Never touches the cache (see [`Self::refresh_usage`]).
    async fn probe_usage(
        &self,
        harness: HarnessId,
        slot: &Slot,
        is_active: bool,
    ) -> Result<UsageSnapshot, ProbeError> {
        let result = match harness {
            HarnessId::Symphony => self.keyed_usage(harness, slot).await,
            _ => Err(ProbeError::NoCredentials {
                why: NoCredentials::Missing,
            }),
        };
        if let Err(error) = &result {
            // Expected, quiet outcomes (API-key logins have no windows).
            if !matches!(error, ProbeError::NoCredentials { .. }) {
                tracing::warn!(
                    provider = harness_slug(harness),
                    slot = %slot.id,
                    active = is_active,
                    class = error.class(),
                    status = ?error.status(),
                    backoff_s = error.backoff().as_secs(),
                    "agent usage unavailable"
                );
            }
        }
        result
    }

    /// One usage probe: GET the endpoint and parse windows.
    async fn claude_usage_request(&self, access_token: &str) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            "claude-code",
            "usage",
            self.inner
                .http
                .get(&self.inner.endpoints.claude_usage)
                .bearer_auth(access_token)
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("Content-Type", "application/json"),
        )
        .await?;
        claude_usage_windows(&body).ok_or_else(|| schema_error("claude-code", &body))
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// The upstream group a row belongs to (`AgentAccount::provider`): only
/// agents that keep one login PER model provider have one. (Grok slots also
/// carry a store key — the issuer entry they swap — but one Grok login is
/// live at a time, so its rows form a single group.)
fn provider_group(harness: HarnessId, store_key: Option<&str>) -> Option<String> {
    matches!(
        harness,
        HarnessId::Opencode | HarnessId::Pi | HarnessId::Hermes | HarnessId::Symphony
    )
    .then(|| store_key.map(str::to_string))
    .flatten()
}

fn harness_slug(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor",
        HarnessId::Devin => "devin",
        HarnessId::Grok => "grok",
        HarnessId::Hermes => "hermes",
        HarnessId::Pi => "pi",
        HarnessId::Opencode => "opencode",
        HarnessId::Antigravity => "antigravity",
        HarnessId::Mock => "mock",
        HarnessId::Symphony => "symphony",
    }
}

fn read_json(file: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&raw)
        .ok()
        .filter(serde_json::Value::is_object)
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode a JWT payload without verifying — we only mine identity claims from a
/// token the user's own CLI already trusts.
fn jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = BASE64_URL
        .decode(payload)
        .or_else(|_| BASE64.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn slot_id_for(harness: HarnessId, account_key: &str) -> String {
    let digest = Sha256::digest(format!("{}:{account_key}", harness_slug(harness)).as_bytes());
    crate::repos::hex(&digest)[..16].to_string()
}

/// Pretty plan label from Claude's org type + rate-limit tier ("Max 20×").
fn claude_plan(org_type: Option<&str>, tier: Option<&str>) -> Option<String> {
    let base = match org_type {
        Some("claude_max") => "Max",
        Some("claude_pro") => "Pro",
        Some("claude_team") => "Team",
        Some("claude_enterprise") => "Enterprise",
        _ => return None,
    };
    // "…_20x" style tiers carry a multiplier suffix.
    let mult = tier.and_then(|t| {
        let stem = t.strip_suffix('x')?;
        let digits: String = stem
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let preceded = stem.len() > digits.len()
            && stem.as_bytes().get(stem.len() - digits.len() - 1) == Some(&b'_');
        (!digits.is_empty() && preceded).then_some(digits)
    });
    Some(match mult {
        Some(mult) => format!("{base} {mult}×"),
        None => base.to_string(),
    })
}

fn codex_plan(plan: Option<&str>) -> Option<String> {
    let plan = plan?;
    let mut chars = plan.chars();
    let first = chars.next()?;
    Some(format!(
        "ChatGPT {}{}",
        first.to_uppercase(),
        chars.as_str()
    ))
}

/// Meter label for a Codex rate-limit window from its `limit_window_seconds`:
/// the free tier's window is a 30-day month (2_592_000s), Plus runs a 5-hour
/// primary (~18_000s) with a weekly secondary (604_800s). A bare "> 1 day =
/// week" rule mislabeled the monthly window "Week"; thresholds in seconds
/// leave the middle gaps to the nearest label rather than guessing a plan.
fn codex_window_label(span_seconds: i64) -> &'static str {
    const DAY: i64 = 86_400;
    if span_seconds >= 28 * DAY {
        "Month"
    } else if span_seconds >= 5 * DAY {
        "Week"
    } else {
        "Session"
    }
}

fn parse_when(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    match value? {
        serde_json::Value::Number(n) => DateTime::<Utc>::from_timestamp(n.as_i64()?, 0),
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        _ => None,
    }
}

fn usage_key(harness: HarnessId, account_key: &str) -> String {
    format!("{}:{account_key}", harness_slug(harness))
}

/// A short digest of a slot's credential blob — tells "same token that was
/// rejected" from "the CLI refreshed / the user re-logged in". Never the
/// secret itself: this lands in the usage cache file.
fn credentials_fingerprint(credentials: &serde_json::Value) -> String {
    crate::repos::hex(&Sha256::digest(credentials.to_string().as_bytes()))[..16].to_string()
}

/// Send one probe request and decode its JSON body, classifying every
/// failure. Logs provider/step/status/class/Retry-After — never the request
/// (bearer tokens) or the response body (it can echo account details).
async fn probe_json(
    provider: &'static str,
    step: &'static str,
    request: reqwest::RequestBuilder,
) -> Result<serde_json::Value, ProbeError> {
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            let error = ProbeError::Network {
                timeout: err.is_timeout(),
            };
            tracing::warn!(
                provider,
                step,
                class = error.class(),
                connect = err.is_connect(),
                "agent usage probe failed"
            );
            return Err(error);
        }
    };
    let status = response.status();
    if !status.is_success() {
        let retry_after_secs = retry_after_secs(response.headers(), Utc::now());
        let error = classify_status(status.as_u16(), retry_after_secs);
        tracing::warn!(
            provider,
            step,
            status = status.as_u16(),
            class = error.class(),
            retry_after_s = ?retry_after_secs,
            "agent usage probe failed"
        );
        return Err(error);
    }
    response.json().await.map_err(|_| {
        tracing::warn!(
            provider,
            step,
            class = "schema",
            "agent usage probe: body is not JSON"
        );
        ProbeError::Schema
    })
}

fn classify_status(status: u16, retry_after_secs: Option<u64>) -> ProbeError {
    match status {
        401 | 403 => ProbeError::Unauthorized { status },
        429 => ProbeError::RateLimited { retry_after_secs },
        _ => ProbeError::Http {
            status,
            retry_after_secs,
        },
    }
}

/// `Retry-After` as delay-seconds or an HTTP-date (RFC 9110 §10.2.3).
fn retry_after_secs(headers: &reqwest::header::HeaderMap, now: DateTime<Utc>) -> Option<u64> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs);
    }
    let at = DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        at.with_timezone(&Utc)
            .signed_duration_since(now)
            .num_seconds()
            .max(0) as u64,
    )
}

/// A 2xx without the fields we parse: log the top-level KEYS (never values)
/// so schema drift is diagnosable from the log alone.
fn schema_error(provider: &'static str, body: &serde_json::Value) -> ProbeError {
    let keys: Vec<&str> = body
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    tracing::warn!(
        provider,
        class = "schema",
        ?keys,
        "agent usage probe: unexpected response shape"
    );
    ProbeError::Schema
}

/// The UI's reason line for an account whose last probe failed. `None` for
/// failures that need no explanation beyond the missing meters.
fn usage_error_message(
    harness: HarnessId,
    store_key: Option<&str>,
    active: bool,
    error: &ProbeError,
    entry: &UsageEntry,
    now: i64,
) -> Option<String> {
    let provider = match (harness, store_key) {
        (HarnessId::ClaudeCode, _) => "Anthropic",
        (HarnessId::Codex, _) => "OpenAI",
        (HarnessId::Cursor, _) => "Cursor",
        (HarnessId::Grok, _) => "xAI",
        (HarnessId::Devin, _) => "Devin",
        (_, Some(key)) => stores::upstream_vendor(key),
        _ => "the provider",
    };
    let retry = entry
        .retry_at
        .filter(|at| *at > now)
        .map(|at| format!(" — retrying in {}", short_duration(at - now)))
        .unwrap_or_default();
    Some(match error {
        ProbeError::RateLimited { .. } => format!("Rate limited by {provider}{retry}"),
        // Devin's key never expires on its own: a rejection means it was
        // revoked, whichever login it is.
        ProbeError::Unauthorized { .. } if harness == HarnessId::Devin => {
            "Signed out — sign in again".to_string()
        }
        ProbeError::Unauthorized { .. } if active => {
            // The CLI owns (and refreshes) the live token; it just hasn't yet.
            format!(
                "Session expired — it refreshes the next time {} runs",
                stores::cli_name(harness)
            )
        }
        // Codex-style slots aren't refreshed in the background (their
        // refresh tokens rotate); the CLI refreshes a switched-to login on
        // its next run. Hermes refreshes its own pool.
        ProbeError::Unauthorized { .. }
            if matches!(
                harness,
                HarnessId::Codex | HarnessId::Opencode | HarnessId::Pi
            ) =>
        {
            "Session expired — switch to it to refresh".to_string()
        }
        ProbeError::Unauthorized { .. } if harness == HarnessId::Hermes => {
            "Session expired — it refreshes the next time hermes uses it".to_string()
        }
        ProbeError::Unauthorized { .. } => "Signed out — sign in again".to_string(),
        ProbeError::Http { status, .. } if *status >= 500 => {
            format!("{provider} is having trouble ({status}){retry}")
        }
        ProbeError::Http { status, .. } => format!("Usage check failed ({status})"),
        ProbeError::Network { timeout: true } => format!("{provider} didn't respond{retry}"),
        ProbeError::Network { .. } => format!("Couldn't reach {provider}{retry}"),
        ProbeError::Schema => "Usage format changed — update zeron".to_string(),
        ProbeError::NoCredentials {
            why: NoCredentials::ApiKey,
        } => "API keys have no plan usage".to_string(),
        ProbeError::NoCredentials {
            why: NoCredentials::KeyExpired,
        } => "API key expired — connect again".to_string(),
        ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        } => return None,
        ProbeError::NoCredentials {
            why: NoCredentials::Unsupported,
        } => "No usage view for this login".to_string(),
        ProbeError::UntrustedEndpoint => {
            "Usage skipped — this login names a server zeron doesn't recognize".to_string()
        }
    })
}

/// "45s" / "2m" / "3h" — the coarse countdown the reason line needs.
fn short_duration(ms: i64) -> String {
    let secs = (ms + 999) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", (secs + 59) / 60)
    } else {
        format!("{}h", (secs + 3599) / 3600)
    }
}

/// Codex `/wham/usage`: primary/secondary windows + the live plan.
fn codex_usage_snapshot(body: &serde_json::Value) -> Option<UsageSnapshot> {
    let rl = body.get("rate_limit")?;
    let mut windows = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        if let Some(w) = rl.get(key)
            && let Some(used) = w.get("used_percent").and_then(|v| v.as_f64())
        {
            let span = w
                .get("limit_window_seconds")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            windows.push(AgentUsageWindow {
                label: codex_window_label(span).to_string(),
                used_fraction: (used / 100.0) as f32,
                resets_at: parse_when(w.get("reset_at")),
            });
        }
    }
    if windows.is_empty() {
        return None;
    }
    // Live plan ("free"/"plus"/"pro"…) — beats the login-time JWT claim,
    // so a plan change shows up on the next forced refresh without a
    // re-login.
    let plan_label = codex_plan(str_field(body, "plan_type").as_deref());
    Some(UsageSnapshot {
        windows,
        plan_label,
    })
}

/// Windows from Claude's `/api/oauth/usage`: the 5-hour session and weekly
/// buckets, each a 0-100 `utilization` with an RFC3339 `resets_at`.
fn claude_usage_windows(body: &serde_json::Value) -> Option<UsageSnapshot> {
    let mut windows = Vec::new();
    for (key, label) in [("five_hour", "Session"), ("seven_day", "Week")] {
        if let Some(w) = body.get(key)
            && let Some(utilization) = w.get("utilization").and_then(|v| v.as_f64())
        {
            windows.push(AgentUsageWindow {
                label: label.to_string(),
                used_fraction: (utilization / 100.0) as f32,
                resets_at: parse_when(w.get("resets_at")),
            });
        }
    }
    (!windows.is_empty()).then_some(UsageSnapshot {
        windows,
        plan_label: None,
    })
}

/// 32 random bytes (two v4 uuids — OS randomness), base64url without
/// padding (43 chars): the size Claude Code uses for both its PKCE verifier
/// and its OAuth `state`.
fn random_url_token() -> String {
    let raw: Vec<u8> = uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .chain(uuid::Uuid::new_v4().as_bytes())
        .copied()
        .collect();
    BASE64_URL.encode(&raw)
}

/// PKCE: a verifier of 32 random bytes and its S256 challenge.
fn pkce_pair() -> (String, String) {
    let verifier = random_url_token();
    let challenge = BASE64_URL.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// The loopback port an authorize url's `redirect_uri` lands on — where the
/// login's CLI (or our own listener) waits for the browser. `None` for flows
/// that don't redirect to this machine.
/// Whether a remote login's reported callback `port` may be forwarded on this
/// device: never a privileged port, and only the one its authorize `url`
/// actually redirects to — a buggy or hostile peer can't make us bind (and
/// receive local traffic on) an arbitrary loopback port.
pub(crate) fn tunnel_port_allowed(port: u16, url: Option<&str>) -> bool {
    port >= 1024
        && url
            .and_then(loopback_port)
            .is_some_and(|redirect| redirect == port)
}

pub(crate) fn loopback_port(url: &str) -> Option<u16> {
    let url = reqwest::Url::parse(url).ok()?;
    let redirect = url
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value.into_owned())?;
    let redirect = reqwest::Url::parse(&redirect).ok()?;
    let loopback = matches!(
        redirect.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]")
    );
    (redirect.scheme() == "http" && loopback)
        .then(|| redirect.port())
        .flatten()
}

/// [`await_claude_callback`] for any loopback redirect: serve `path` until a
/// request carries our `state`; `who` names the provider in a refusal.
async fn await_loopback_callback(
    listener: &tokio::net::TcpListener,
    path: &str,
    state: &str,
    who: &str,
) -> (Result<String, String>, tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt as _;
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Some(target) = read_request_target(&mut socket).await else {
            continue;
        };
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).ok();
        let Some(url) = url.filter(|url| url.path() == path) else {
            let _ = socket
                .write_all(http_response("404 Not Found", &[], "").as_bytes())
                .await;
            continue;
        };
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        if param("state").as_deref() != Some(state) {
            let _ = socket
                .write_all(http_response("400 Bad Request", &[], "Unknown sign-in.").as_bytes())
                .await;
            continue;
        }
        if let Some(code) = param("code").filter(|code| !code.is_empty()) {
            return (Ok(code), socket);
        }
        let reason = param("error_description")
            .or_else(|| param("error"))
            .unwrap_or_else(|| "no authorization code came back".into());
        return (Err(format!("{who} sign-in failed: {reason}")), socket);
    }
}

/// The request target of a browser's `GET` (headers read and discarded).
async fn read_request_target(socket: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    let read = async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&chunk[..n]);
        }
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .ok()??;
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts
        .next()
        .filter(|target| target.starts_with('/'))
        .map(str::to_string)
}

fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut response = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    ));
    response
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Minimal percent-encoding for OAuth query params (matches `encodeURIComponent`
/// for the constant inputs used here).
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn remove_if_exists(file: &Path) -> Result<(), EngineError> {
    match std::fs::remove_file(file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Atomic write via a same-dir temp file + rename; `secret` = 0600 from birth.
///
/// The temp file has a random name and is created exclusively (`O_CREAT |
/// O_EXCL`, which never follows or reuses a pre-planted path or symlink),
/// gets its permissions set on the open handle before any byte is written,
/// is fsynced, then renamed over `file` — replacing a symlink there rather
/// than writing through it. A crash leaves at worst an owner-only temp file
/// and never a torn target.
fn write_file_atomic(file: &Path, bytes: &[u8], secret: bool) -> Result<(), EngineError> {
    stage_file_atomic(file, bytes, secret)?
        .persist(file)
        .map_err(|e| EngineError::from(e.error))?;
    Ok(())
}

/// The first half of [`write_file_atomic`]: `bytes` written and synced to a
/// fresh, exclusive, same-directory temp file with the final permissions,
/// ready to `persist` over `file`. Callers that must re-check the target
/// right before replacing it (see `stores::merge_json_entry`) stage first, so
/// only the comparison and the rename remain between their check and the
/// swap. Dropping the returned file deletes it.
fn stage_file_atomic(
    file: &Path,
    bytes: &[u8],
    secret: bool,
) -> Result<tempfile::NamedTempFile, EngineError> {
    use std::io::Write;
    let dir = file
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Secrets: owner-only. Anything else keeps the target's mode (a
        // config file the user made group-readable stays so).
        let mode = if secret {
            0o600
        } else {
            std::fs::metadata(file)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o644)
        };
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = secret;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}
