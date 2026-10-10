//! Native Symphony credential storage, identity discovery and account slots.

use super::*;

pub(super) fn cli_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Grok => "grok",
        HarnessId::Devin => "devin",
        HarnessId::Opencode => "opencode",
        HarnessId::Pi => "pi",
        HarnessId::Hermes => "hermes",
        HarnessId::Antigravity => "Antigravity",
        HarnessId::Mock => "mock",
        HarnessId::Symphony => "symphony",
    }
}

/// The vendor behind a per-provider store key, for usage reasons.
pub(super) fn upstream_vendor(store_key: &str) -> &'static str {
    match store_key {
        "openai" | "openai-codex" => "OpenAI",
        "anthropic" => "Anthropic",
        "github-copilot" | "copilot" => "GitHub",
        "nous" => "Nous",
        "xai-oauth" | "xai" => "xAI",
        _ => "the provider",
    }
}

fn hashed_key(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    crate::repos::hex(&digest)[..12].to_string()
}

fn opaque_secret(entry: &serde_json::Value) -> Option<String> {
    str_field(entry, "refresh").or_else(|| str_field(entry, "access"))
}

// ── service ─────────────────────────────────────────────────────────────────

impl AgentAccounts {
    /// The live entry under `store_key`, if it's an OAuth login.
    pub(super) fn live_keyed_entry(
        &self,
        harness: HarnessId,
        store_key: &str,
    ) -> Option<serde_json::Value> {
        if harness != HarnessId::Symphony {
            return None;
        }
        self.symphony_file(store_key)
            .ok()
            .and_then(|p| read_json(&p))
            .filter(|e| str_field(e, "access_token").is_some())
    }

    /// Replace one native Symphony provider entry without changing other providers.
    pub(super) fn write_keyed_entry(
        &self,
        harness: HarnessId,
        store_key: &str,
        entry: Option<&serde_json::Value>,
    ) -> Result<(), EngineError> {
        if harness != HarnessId::Symphony {
            return Err(EngineError::Other(
                "This fork only manages Symphony provider accounts.".into(),
            ));
        }
        self.write_symphony_entry(store_key, entry)
    }

    /// The live login under ONE store key: `Some(None)` when an OAuth entry
    /// is there but couldn't be identified, `None` when there is none.
    pub(super) async fn detect_keyed_entry(
        &self,
        harness: HarnessId,
        store_key: &str,
    ) -> Option<Option<Detected>> {
        let entry = self.live_keyed_entry(harness, store_key)?;
        Some(self.identify_symphony_entry(store_key, &entry).await)
    }

    /// Who an opaque live entry is: the slot already holding this exact
    /// token, else the remembered answer for it, else one read-only profile
    /// call (cached per token, so a list doesn't re-ask).
    async fn identify_opaque(
        &self,
        harness: HarnessId,
        store_key: &str,
        entry: &serde_json::Value,
    ) -> Option<Detected> {
        let secret = opaque_secret(entry)?;
        if let Some(slot) = self.read_slots(harness).into_iter().find(|slot| {
            slot.store_key.as_deref() == Some(store_key)
                && opaque_secret(&slot.credentials).as_deref() == Some(secret.as_str())
        }) {
            return Some(
                Detected::known(slot.account_key, slot.profile, entry.clone()).keyed(store_key),
            );
        }
        let fingerprint = format!("{store_key}:{}", hashed_key(&secret));
        let cached = lock(&self.inner.identities).get(&fingerprint).cloned();
        let (account_key, profile) = match cached {
            Some(IdentityLookup::Known(key, profile)) => (key, profile),
            Some(IdentityLookup::Failed(at)) if at.elapsed() < IDENTITY_RETRY => return None,
            _ => {
                let known = match str_field(entry, "access") {
                    Some(access) => self.anthropic_identity(store_key, &access).await,
                    None => None,
                };
                let lookup = match &known {
                    Some((key, profile)) => IdentityLookup::Known(key.clone(), profile.clone()),
                    None => IdentityLookup::Failed(Instant::now()),
                };
                lock(&self.inner.identities).insert(fingerprint, lookup);
                known?
            }
        };
        Some(Detected::known(account_key, profile, entry.clone()).keyed(store_key))
    }

    /// Claude's profile for a live access token (`user:profile` scope).
    async fn anthropic_identity(
        &self,
        store_key: &str,
        access_token: &str,
    ) -> Option<(String, SlotProfile)> {
        let profile: serde_json::Value = self
            .inner
            .http
            .get(&self.inner.endpoints.claude_profile)
            .bearer_auth(access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .ok()
            .filter(|res| res.status().is_success())?
            .json()
            .await
            .ok()?;
        let account = profile.get("account")?;
        let org = profile.get("organization").cloned().unwrap_or_default();
        let email = str_field(account, "email_address")?;
        let uuid = str_field(account, "uuid").unwrap_or_else(|| email.clone());
        Some((
            format!("{store_key}:{uuid}"),
            SlotProfile {
                email,
                display_name: str_field(account, "display_name")
                    .or_else(|| str_field(account, "full_name")),
                organization: str_field(&org, "name"),
                plan: claude_plan(
                    str_field(&org, "organization_type").as_deref(),
                    str_field(&org, "rate_limit_tier").as_deref(),
                )
                .map(|plan| format!("Claude {plan}"))
                .or_else(|| Some("Claude".to_string())),
                auth_kind: AgentAuthKind::Oauth,
            },
        ))
    }

    /// Persist a fresh login as a slot, then let it take over the live login
    /// where [`AgentAccounts::adopt_if_live`] says so: no live login yet
    /// ("Connect" means runs work afterwards), or a re-login of the live
    /// account (else the next list would snapshot the old, possibly revoked,
    /// live tokens straight back over the fresh slot). Any other live login
    /// is left alone — switching stays explicit. Under [`Inner::ops`], so a
    /// concurrent list can't land between the two writes.
    pub(super) async fn save_new_login(
        &self,
        harness: HarnessId,
        detected: &Detected,
    ) -> Result<(), EngineError> {
        let _ops = self.inner.ops.lock().await;
        self.snapshot_detected(harness, detected)?;
        let id = slot_id_for(harness, &detected.account_key);
        if let Some(slot) = self.read_slot(harness, &id) {
            self.adopt_if_live(&slot).await?;
        }
        Ok(())
    }
}

/// See [`AgentAccounts::devin_login_dirs`].
const SYMPHONY_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "grok",
    "openrouter",
    "vercel",
    "ollama",
    "local",
];

pub(super) fn symphony_detected(provider: &str, entry: &serde_json::Value) -> Option<Detected> {
    let token = str_field(entry, "access_token")?;
    let claims = jwt_claims(&token).unwrap_or_default();
    let id = str_field(entry, "id_token")
        .and_then(|s| jwt_claims(&s))
        .unwrap_or_default();
    let auth = claims
        .get("https://api.openai.com/auth")
        .or_else(|| id.get("https://api.openai.com/auth"));
    let email = str_field(entry, "email")
        .or_else(|| str_field(&claims, "email"))
        .or_else(|| str_field(&id, "email"))
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|p| str_field(p, "email"))
        });
    let workspace = str_field(entry, "account_id")
        .or_else(|| auth.and_then(|a| str_field(a, "chatgpt_account_id")));
    let seat = auth
        .and_then(|a| str_field(a, "chatgpt_user_id"))
        .or_else(|| str_field(&id, "sub"));
    let identity = workspace
        .map(|workspace| match seat {
            Some(seat) => format!("{seat}:{workspace}"),
            None => workspace,
        })
        .or_else(|| email.clone())
        .or_else(|| str_field(&claims, "sub"))
        .unwrap_or_else(|| {
            format!(
                "local-{}",
                BASE64_URL.encode(Sha256::digest(token.as_bytes()))
            )
        });
    let (label, auth_kind) = match provider {
        "openai" => ("ChatGPT", AgentAuthKind::Oauth),
        "anthropic" => ("Claude", AgentAuthKind::Oauth),
        "grok" => ("Grok", AgentAuthKind::Oauth),
        "gemini" => ("Gemini", AgentAuthKind::ApiKey),
        "openrouter" => ("OpenRouter", AgentAuthKind::ApiKey),
        "vercel" => ("Vercel", AgentAuthKind::ApiKey),
        "ollama" => ("Ollama", AgentAuthKind::ApiKey),
        "local" => ("Local", AgentAuthKind::ApiKey),
        _ => return None,
    };
    Some(
        Detected::known(
            format!("{provider}:{identity}"),
            SlotProfile {
                email: email.unwrap_or_else(|| format!("{label} account")),
                display_name: str_field(&claims, "name"),
                organization: None,
                plan: Some(label.to_string()),
                auth_kind,
            },
            entry.clone(),
        )
        .keyed(provider),
    )
}

impl AgentAccounts {
    fn symphony_env_key(provider: &str) -> Option<&'static str> {
        match provider {
            "gemini" => Some("GEMINI_API_KEY"),
            "openrouter" => Some("OPENROUTER_API_KEY"),
            "vercel" => Some("AI_GATEWAY_API_KEY"),
            "ollama" => Some("OLLAMA_BASE_URL"),
            "local" => Some("LOCAL_BASE_URL"),
            _ => None,
        }
    }

    fn symphony_file(&self, provider: &str) -> Result<PathBuf, EngineError> {
        if !SYMPHONY_PROVIDERS.contains(&provider) {
            return Err(EngineError::Other("Unknown Symphony provider.".into()));
        }
        Ok(self
            .inner
            .config
            .symphony_home
            .join("oauth")
            .join(format!("{provider}.json")))
    }

    async fn identify_symphony_entry(
        &self,
        provider: &str,
        entry: &serde_json::Value,
    ) -> Option<Detected> {
        let detected = symphony_detected(provider, entry)?;
        if provider != "anthropic" || !detected.account_key.starts_with("anthropic:local-") {
            return Some(detected);
        }
        // Python's refresh may omit account_id. Reuse the known identity when
        // either credential still matches, otherwise resolve through the same
        // cached read-only profile lookup used for Pi's opaque Claude tokens.
        if let Some(slot) = self
            .read_slots(HarnessId::Symphony)
            .into_iter()
            .find(|slot| {
                slot.store_key.as_deref() == Some(provider)
                    && ["access_token", "refresh_token"].iter().any(|field| {
                        str_field(entry, field).is_some_and(|value| {
                            str_field(&slot.credentials, field).as_deref() == Some(value.as_str())
                        })
                    })
            })
        {
            return Some(
                Detected::known(slot.account_key, slot.profile, entry.clone()).keyed(provider),
            );
        }
        let generic = serde_json::json!({"access": entry.get("access_token"), "refresh": entry.get("refresh_token")});
        match self
            .identify_opaque(HarnessId::Symphony, provider, &generic)
            .await
        {
            Some(identity) => Some(
                Detected::known(identity.account_key, identity.profile, entry.clone())
                    .keyed(provider),
            ),
            None => Some(detected),
        }
    }

    pub(super) async fn detect_symphony(&self) -> Vec<Detected> {
        let mut detected = Vec::new();
        for provider in SYMPHONY_PROVIDERS {
            if let Some(entry) = self.live_keyed_entry(HarnessId::Symphony, provider) {
                if let Some(account) = self.identify_symphony_entry(provider, &entry).await {
                    detected.push(account);
                }
            } else if let Some(key) = Self::symphony_env_key(provider)
                && let Some(value) = self.symphony_env_value(key)
            {
                let entry = serde_json::json!({"access_token": value, "auth": "api-key"});
                if let Some(account) = symphony_detected(provider, &entry) {
                    detected.push(account);
                }
            }
        }
        detected
    }

    fn symphony_env_value(&self, key: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.inner.config.symphony_home.join(".env")).ok()?;
        text.lines().rev().find_map(|line| {
            let bare = line
                .trim_start()
                .strip_prefix("export ")
                .unwrap_or(line.trim_start());
            let (name, value) = bare.split_once('=')?;
            (name.trim() == key && !value.trim().is_empty()).then(|| value.trim().to_string())
        })
    }

    fn write_symphony_entry(
        &self,
        provider: &str,
        entry: Option<&serde_json::Value>,
    ) -> Result<(), EngineError> {
        let file = self.symphony_file(provider)?;
        if let Some(env_key) = Self::symphony_env_key(provider) {
            return match entry {
                Some(entry) => {
                    let value = str_field(entry, "access_token").ok_or_else(|| {
                        EngineError::Other("Symphony API credentials have no value.".into())
                    })?;
                    let home = &self.inner.config.symphony_home;
                    private_dir(home)?;
                    let env_file = home.join(".env");
                    let text = match std::fs::read_to_string(&env_file) {
                        Ok(text) => text,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                        Err(e) => return Err(e.into()),
                    };
                    let mut lines: Vec<String> = text
                        .lines()
                        .filter(|line| {
                            let bare = line
                                .trim_start()
                                .strip_prefix("export ")
                                .unwrap_or(line.trim_start());
                            bare.split_once('=')
                                .is_none_or(|(name, _)| name.trim() != env_key)
                        })
                        .map(str::to_string)
                        .collect();
                    lines.push(format!("{env_key}={value}"));
                    write_file_atomic(
                        &env_file,
                        format!("{}\n", lines.join("\n")).as_bytes(),
                        true,
                    )
                }
                None => self.remove_symphony_env(env_key),
            };
        }
        match entry {
            Some(entry) => {
                if str_field(entry, "access_token").is_none() {
                    return Err(EngineError::Other(
                        "Symphony OAuth credentials have no access token.".into(),
                    ));
                }
                private_dir(&self.inner.config.symphony_home)?;
                private_dir(file.parent().unwrap())?;
                write_file_atomic(
                    &file,
                    serde_json::to_vec_pretty(entry)
                        .map_err(|e| EngineError::Other(e.to_string()))?
                        .as_slice(),
                    true,
                )?;
                let env_file = self.inner.config.symphony_home.join(".env");
                let text = match std::fs::read_to_string(&env_file) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e.into()),
                };
                let key = format!("SYMPHONY_{}_AUTH", provider.to_ascii_uppercase());
                let mut lines: Vec<String> = text
                    .lines()
                    .filter(|line| {
                        let line = line
                            .trim_start()
                            .strip_prefix("export ")
                            .unwrap_or(line.trim_start());
                        line.split_once('=')
                            .is_none_or(|(name, _)| name.trim() != key)
                    })
                    .map(str::to_string)
                    .collect();
                lines.push(format!("{key}=oauth"));
                write_file_atomic(
                    &env_file,
                    format!("{}\n", lines.join("\n")).as_bytes(),
                    true,
                )
            }
            None => remove_if_exists(&file),
        }
    }

    fn remove_symphony_env(&self, key: &str) -> Result<(), EngineError> {
        let env_file = self.inner.config.symphony_home.join(".env");
        let text = match std::fs::read_to_string(&env_file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let lines: Vec<_> = text
            .lines()
            .filter(|line| {
                let bare = line
                    .trim_start()
                    .strip_prefix("export ")
                    .unwrap_or(line.trim_start());
                bare.split_once('=')
                    .is_none_or(|(name, _)| name.trim() != key)
            })
            .collect();
        write_file_atomic(
            &env_file,
            format!("{}\n", lines.join("\n")).as_bytes(),
            true,
        )
    }
}

#[cfg(test)]
mod symphony_storage_tests {
    use super::*;

    #[tokio::test]
    async fn symphony_snapshots_switches_and_removes_one_provider() {
        let temp = tempfile::tempdir().unwrap();
        let config = AgentAccountsConfig::isolated(temp.path());
        let home = config.symphony_home.clone();
        let accounts = AgentAccounts::new(config);
        let first = serde_json::json!({"access_token":"first", "refresh_token":"refresh", "expires_at":123.5, "account_id":"one", "email":"one@example.com", "extra":{"opaque":true}});
        let second = serde_json::json!({"access_token":"second", "account_id":"two", "email":"two@example.com"});
        let other = serde_json::json!({"access_token":"claude", "account_id":"claude-id", "email":"claude@example.com"});
        accounts
            .write_keyed_entry(HarnessId::Symphony, "openai", Some(&first))
            .unwrap();
        accounts
            .write_keyed_entry(HarnessId::Symphony, "anthropic", Some(&other))
            .unwrap();
        std::fs::write(
            home.join(".env"),
            "UNRELATED=value\nSYMPHONY_OPENAI_AUTH=key\nSYMPHONY_ANTHROPIC_AUTH=oauth\n",
        )
        .unwrap();
        let snapshot = accounts.list(false).await.unwrap();
        let id = snapshot
            .accounts
            .iter()
            .find(|a| a.harness == HarnessId::Symphony && a.provider.as_deref() == Some("openai"))
            .unwrap()
            .id
            .clone();
        accounts
            .write_keyed_entry(HarnessId::Symphony, "openai", Some(&second))
            .unwrap();
        let snapshot = accounts.activate(HarnessId::Symphony, &id).await.unwrap();
        assert_eq!(
            snapshot
                .accounts
                .iter()
                .filter(|a| a.harness == HarnessId::Symphony)
                .count(),
            3
        );
        assert_eq!(
            accounts.live_keyed_entry(HarnessId::Symphony, "openai"),
            Some(first)
        );
        assert_eq!(
            accounts.live_keyed_entry(HarnessId::Symphony, "anthropic"),
            Some(other.clone())
        );
        let env = std::fs::read_to_string(home.join(".env")).unwrap();
        assert!(env.contains("UNRELATED=value\n"));
        assert!(env.contains("SYMPHONY_OPENAI_AUTH=oauth\n"));
        assert!(!env.contains("SYMPHONY_OPENAI_AUTH=key"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join("oauth/openai.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(home.join("oauth"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        accounts.forget(HarnessId::Symphony, &id).await.unwrap();
        assert!(!home.join("oauth/openai.json").exists());
        assert_eq!(
            accounts.live_keyed_entry(HarnessId::Symphony, "anthropic"),
            Some(other)
        );
        assert!(
            accounts
                .write_keyed_entry(HarnessId::Symphony, "../bad", Some(&second))
                .is_err()
        );
    }
    #[test]
    fn symphony_chatgpt_team_seats_have_distinct_keys() {
        let token = |seat: &str| {
            let claims = serde_json::json!({"email":format!("{seat}@example.com"),
                "https://api.openai.com/auth":{"chatgpt_account_id":"shared-team", "chatgpt_user_id":seat}});
            serde_json::json!({"access_token":format!("e30.{}.sig", BASE64_URL.encode(claims.to_string())), "account_id":"shared-team"})
        };
        assert_ne!(
            symphony_detected("openai", &token("one"))
                .unwrap()
                .account_key,
            symphony_detected("openai", &token("two"))
                .unwrap()
                .account_key
        );
    }

    #[tokio::test]
    async fn symphony_claude_refresh_reuses_saved_identity_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let accounts = AgentAccounts::new(AgentAccountsConfig::isolated(temp.path()));
        let first = serde_json::json!({"access_token":"old", "refresh_token":"same-refresh", "account_id":"stable-account"});
        accounts
            .write_keyed_entry(HarnessId::Symphony, "anthropic", Some(&first))
            .unwrap();
        let snapshot = accounts.list(false).await.unwrap();
        let id = snapshot.accounts[0].id.clone();
        let refreshed = serde_json::json!({"access_token":"new", "refresh_token":"same-refresh"});
        accounts
            .write_keyed_entry(HarnessId::Symphony, "anthropic", Some(&refreshed))
            .unwrap();
        let snapshot = accounts.list(false).await.unwrap();
        assert_eq!(snapshot.accounts.len(), 1);
        assert_eq!(snapshot.accounts[0].id, id);
        assert!(snapshot.accounts[0].active);
    }
}
