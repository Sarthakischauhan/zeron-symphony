//! Native Symphony OAuth. OpenAI reuses `oauth::start_openai_login`.
//!
//! Claude supports setup-token paste or PKCE callback paste; Grok uses device
//! authorization. All grants are saved in Symphony's native provider files.

use super::*;

const ANTHROPIC_AUTHORIZE: &str = "https://claude.ai/oauth/authorize";
const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

impl AgentAccounts {
    pub(super) async fn start_symphony_login(
        &self,
        provider: &str,
    ) -> Result<AgentLoginStart, EngineError> {
        match provider {
            "openai" | "openai-codex" => {
                self.start_openai_login(HarnessId::Symphony, "openai").await
            }
            "anthropic" => {
                self.reap_spawned_flows(HarnessId::Symphony);
                // UUID v4 bytes supply independent unpredictable verifier and state.
                let (verifier, challenge) = pkce_pair();
                let state = random_url_token();
                let mut url = reqwest::Url::parse(ANTHROPIC_AUTHORIZE).unwrap();
                url.query_pairs_mut().extend_pairs([
                    ("code", "true"),
                    ("client_id", CLAUDE_CLIENT_ID),
                    ("response_type", "code"),
                    ("redirect_uri", CLAUDE_REDIRECT),
                    ("scope", CLAUDE_SCOPES),
                    ("code_challenge", &challenge),
                    ("code_challenge_method", "S256"),
                    ("state", &state),
                ]);
                let login_id = new_id();
                lock(&self.inner.flows).insert(
                    login_id.clone(),
                    LoginFlow::SymphonyPaste {
                        started_at: Instant::now(),
                        verifier,
                        state,
                    },
                );
                Ok(AgentLoginStart {
                    login_id,
                    url: url.into(),
                    mode: AgentLoginMode::PasteCode,
                    callback_port: None,
                })
            }
            "xai" | "grok" => self.start_symphony_xai_login().await,
            "gemini" | "openrouter" | "vercel" | "ollama" | "local" => {
                self.reap_spawned_flows(HarnessId::Symphony);
                let login_id = new_id();
                lock(&self.inner.flows).insert(
                    login_id.clone(),
                    LoginFlow::SymphonyPaste {
                        started_at: Instant::now(),
                        verifier: provider.to_string(),
                        state: "api-key".into(),
                    },
                );
                Ok(AgentLoginStart {
                    login_id,
                    url: String::new(),
                    mode: AgentLoginMode::PasteCode,
                    callback_port: None,
                })
            }
            _ => Err(EngineError::Other(format!(
                "Symphony does not support sign-in for {provider}."
            ))),
        }
    }

    /// Returns None for another kind of login. Keep a pending login on invalid
    /// input or a failed exchange, so a mistaken paste does not destroy PKCE.
    pub(super) async fn complete_symphony_paste_login(
        &self,
        login_id: &str,
        pasted: &str,
    ) -> Option<Result<(), EngineError>> {
        let (verifier, expected_state, started_at) = match lock(&self.inner.flows).get(login_id) {
            Some(LoginFlow::SymphonyPaste {
                verifier,
                state,
                started_at,
            }) => (verifier.clone(), state.clone(), *started_at),
            _ => return None,
        };
        if expected_state == "api-key" {
            let result = self.save_symphony_api_key(&verifier, pasted, started_at).await;
            if result.is_ok() {
                self.remove_flow(login_id);
            }
            return Some(result);
        }
        let result = async {
            if started_at.elapsed() >= FLOW_TTL {
                return Err(EngineError::Other(
                    "The sign-in expired — start again.".into(),
                ));
            }
            let raw = pasted.trim();
            if !raw.contains('#')
                && !raw.contains("://")
                && !raw.starts_with("sk-ant-api")
                && (raw.starts_with("sk-ant-oat") || raw.starts_with("eyJ") || raw.len() >= 100)
            {
                return self
                    .save_symphony_token("anthropic", serde_json::json!({"access_token": raw}))
                    .await;
            }
            let (code, state) = parse_callback(pasted)?;
            if state != expected_state {
                return Err(EngineError::Other(
                    "OAuth state mismatch — paste the full code#state or callback URL.".into(),
                ));
            }
            let reply = self
                .inner
                .http
                .post(&self.inner.endpoints.symphony_anthropic_token)
                .header("Accept", "application/json")
                .json(&serde_json::json!({
                    "grant_type": "authorization_code", "client_id": CLAUDE_CLIENT_ID,
                    "code": code, "state": state, "redirect_uri": CLAUDE_REDIRECT,
                    "code_verifier": verifier,
                }))
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| EngineError::Other(format!("Anthropic token exchange failed: {e}")))?;
            let token = checked_json(reply, "Anthropic token exchange").await?;
            self.save_symphony_token("anthropic", token).await
        }
        .await;
        if result.is_ok() {
            self.remove_flow(login_id);
        }
        Some(result)
    }

    async fn start_symphony_xai_login(&self) -> Result<AgentLoginStart, EngineError> {
        self.reap_spawned_flows(HarnessId::Symphony);
        let reply = self
            .inner
            .http
            .post(&self.inner.endpoints.symphony_xai_device)
            .header("Accept", "application/json")
            .form(&[("client_id", XAI_CLIENT), ("scope", XAI_SCOPE)])
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("xAI device code request failed: {e}")))?;
        let device = checked_json(reply, "xAI device code request").await?;
        let device_code = required(&device, "device_code")?;
        let user_code = required(&device, "user_code")?;
        let uri = required(&device, "verification_uri")?;
        let url = str_field(&device, "verification_uri_complete")
            .unwrap_or_else(|| format!("{}/{user_code}", uri.trim_end_matches('/')));
        let expires = device
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(300)
            .max(30);
        let interval = device
            .get("interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .max(1);
        let login_id = new_id();
        let state = Arc::new(Mutex::new(TaskLoginState {
            url: Some(url.clone()),
            message: Some(format!("Enter the code {user_code} on xAI.")),
            ..Default::default()
        }));
        let this = self.clone();
        let task_state = state.clone();
        let handle = tokio::spawn(async move {
            let outcome = tokio::time::timeout(
                Duration::from_secs(expires.min(900)),
                this.finish_symphony_xai_login(device_code, interval),
            )
            .await
            .unwrap_or_else(|_| {
                Err(EngineError::Other(
                    "The xAI sign-in expired — start again.".into(),
                ))
            });
            lock(&task_state).outcome = Some(outcome.map_err(|e| e.to_string()));
        });
        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Task {
                harness: HarnessId::Symphony,
                started_at: Instant::now(),
                state,
                handle,
                home: None,
                port: None,
            },
        );
        Ok(AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::Browser,
            callback_port: None,
        })
    }

    async fn finish_symphony_xai_login(
        &self,
        device_code: String,
        mut interval: u64,
    ) -> Result<(), EngineError> {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let response = self
                .inner
                .http
                .post(&self.inner.endpoints.symphony_xai_token)
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", XAI_CLIENT),
                    ("device_code", device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .timeout(Duration::from_secs(30))
                .send()
                .await;
            let response = match response {
                Ok(r) => r,
                Err(_) => continue,
            };
            let success = response.status().is_success();
            let payload: serde_json::Value = response.json().await.map_err(|_| {
                EngineError::Other("xAI returned an invalid token response.".into())
            })?;
            match str_field(&payload, "error").as_deref() {
                Some("authorization_pending") => continue,
                Some("slow_down") => {
                    interval = interval.saturating_add(5);
                    continue;
                }
                Some("access_denied" | "authorization_denied") => {
                    return Err(EngineError::Other("xAI login was denied.".into()));
                }
                Some("expired_token") => {
                    return Err(EngineError::Other(
                        "xAI login expired — start again.".into(),
                    ));
                }
                Some(_) => {
                    return Err(EngineError::Other(
                        "xAI token request failed — start again.".into(),
                    ));
                }
                None if !success => {
                    return Err(EngineError::Other("xAI rejected the token request.".into()));
                }
                None => return self.save_symphony_token("grok", payload).await,
            }
        }
    }

    async fn save_symphony_api_key(
        &self,
        provider: &str,
        pasted: &str,
        started_at: Instant,
    ) -> Result<(), EngineError> {
        if started_at.elapsed() >= FLOW_TTL {
            return Err(EngineError::Other("The sign-in expired — start again.".into()));
        }
        let value = pasted.trim();
        let (env_key, env_value, label) = match provider {
            "gemini" => ("GEMINI_API_KEY", value, "Gemini"),
            "openrouter" => ("OPENROUTER_API_KEY", value, "OpenRouter"),
            "vercel" => ("AI_GATEWAY_API_KEY", value, "Vercel"),
            "ollama" => ("OLLAMA_BASE_URL", value, "Ollama"),
            "local" => ("LOCAL_BASE_URL", value, "Local"),
            _ => return Err(EngineError::Other("Unknown Symphony provider.".into())),
        };
        if env_value.is_empty() || env_value.contains(['\n', '\r', '\0']) {
            return Err(EngineError::Other(format!("Enter a {label} credential.")));
        }
        if matches!(provider, "ollama" | "local") && reqwest::Url::parse(env_value).is_err() {
            return Err(EngineError::Other("Enter a valid server URL.".into()));
        }
        self.write_symphony_env(env_key, env_value)?;
        let entry = serde_json::json!({"access_token": env_value, "auth": "api-key"});
        let detected = stores::symphony_detected(provider, &entry).ok_or_else(|| {
            EngineError::Other("Could not identify the Symphony credential.".into())
        })?;
        self.save_new_login(HarnessId::Symphony, &detected).await
    }

    fn write_symphony_env(&self, key: &str, value: &str) -> Result<(), EngineError> {
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
                let bare = line.trim_start().strip_prefix("export ").unwrap_or(line.trim_start());
                bare.split_once('=').is_none_or(|(name, _)| name.trim() != key)
            })
            .map(str::to_string)
            .collect();
        lines.push(format!("{key}={value}"));
        write_file_atomic(&env_file, format!("{}\n", lines.join("\n")).as_bytes(), true)
    }

    async fn save_symphony_token(
        &self,
        provider: &str,
        payload: serde_json::Value,
    ) -> Result<(), EngineError> {
        let access = required(&payload, "access_token")?;
        let expires = payload
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let mut token = serde_json::json!({
            "access_token": access,
            "refresh_token": str_field(&payload, "refresh_token").unwrap_or_default(),
            "id_token": str_field(&payload, "id_token").unwrap_or_default(),
            "token_type": str_field(&payload, "token_type").unwrap_or_else(|| "Bearer".into()),
            "expires_at": if expires == 0 { 0.0 } else { now_ms() as f64 / 1000.0 + expires as f64 },
            "scope": str_field(&payload, "scope").unwrap_or_default(),
        });
        // Anthropic access tokens are opaque. Fetch the stable account UUID
        // rather than deriving a slot identity from secret token material.
        if provider == "anthropic" {
            let reply = self
                .inner
                .http
                .get(&self.inner.endpoints.claude_profile)
                .bearer_auth(&access)
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("Accept", "application/json")
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| EngineError::Other(format!("Anthropic profile lookup failed: {e}")))?;
            let profile = checked_json(reply, "Anthropic profile lookup").await?;
            let account = profile.get("account").unwrap_or(&profile);
            let identity = str_field(account, "uuid")
                .or_else(|| str_field(account, "id"))
                .or_else(|| str_field(account, "email"))
                .ok_or_else(|| {
                    EngineError::Other("Anthropic profile omitted the account identity.".into())
                })?;
            token["account_id"] = serde_json::json!(identity);
            if let Some(email) =
                str_field(account, "email_address").or_else(|| str_field(account, "email"))
            {
                token["email"] = serde_json::json!(email);
            }
        } else if let Some(claims) = str_field(&payload, "id_token")
            .as_deref()
            .and_then(jwt_claims)
        {
            if let Some(identity) = str_field(&claims, "sub") {
                token["account_id"] = serde_json::json!(identity);
            }
            if let Some(email) = str_field(&claims, "email") {
                token["email"] = serde_json::json!(email);
            }
        }
        let detected = stores::symphony_detected(provider, &token).ok_or_else(|| {
            EngineError::Other("Could not identify the signed-in Symphony account.".into())
        })?;
        self.save_new_login(HarnessId::Symphony, &detected).await
    }
}

async fn checked_json(
    response: reqwest::Response,
    label: &str,
) -> Result<serde_json::Value, EngineError> {
    if !response.status().is_success() {
        // Do not echo response bodies, which may contain credentials.
        return Err(EngineError::Other(format!(
            "{label} failed (HTTP {}).",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|_| EngineError::Other(format!("{label} returned invalid JSON.")))
}

fn required(value: &serde_json::Value, field: &str) -> Result<String, EngineError> {
    str_field(value, field)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| EngineError::Other(format!("OAuth response omitted {field}.")))
}

fn parse_callback(pasted: &str) -> Result<(String, String), EngineError> {
    let raw = pasted.trim().trim_matches(['\'', '"']);
    if raw.contains("://") {
        let url = reqwest::Url::parse(raw)
            .map_err(|_| EngineError::Other("Invalid callback URL.".into()))?;
        let mut pairs: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if let Some(fragment) = url.fragment() {
            let fragment_url =
                reqwest::Url::parse(&format!("https://callback.invalid/?{fragment}"))
                    .map_err(|_| EngineError::Other("Invalid callback fragment.".into()))?;
            pairs.extend(
                fragment_url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned())),
            );
        }
        if pairs.iter().any(|(k, _)| k == "error") {
            return Err(EngineError::Other(
                "Anthropic authorization was declined.".into(),
            ));
        }
        let field = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let code = field("code");
        if !code.is_empty() {
            return Ok((code, field("state")));
        }
    } else if let Some((code, state)) = raw.split_once('#') {
        if !code.trim().is_empty() {
            return Ok((code.trim().into(), state.trim().into()));
        }
    }
    Err(EngineError::Other(
        "Paste the full authorization code#state or callback URL.".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Handler = dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync;

    /// A local stand-in for provider endpoints: `handler(method, path+query,
    /// body)` answers every request; `hits` records `"METHOD path"`.
    struct MockServer {
        base: String,
        hits: Arc<Mutex<Vec<String>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl MockServer {
        async fn start(
            handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static,
        ) -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let hits: Arc<Mutex<Vec<String>>> = Arc::default();
            let handler: Arc<Handler> = Arc::new(handler);
            let task_hits = hits.clone();
            let task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let handler = handler.clone();
                    let hits = task_hits.clone();
                    tokio::spawn(async move {
                        let mut raw = Vec::new();
                        let mut chunk = [0u8; 8192];
                        let (head_end, length) = loop {
                            let n = socket.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            raw.extend_from_slice(&chunk[..n]);
                            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head =
                                    String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                                let length = head
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                break (end + 4, length);
                            }
                        };
                        while raw.len() < head_end + length {
                            let n = socket.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            raw.extend_from_slice(&chunk[..n]);
                        }
                        let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
                        let body = String::from_utf8_lossy(&raw[head_end..]).to_string();
                        let mut first = head.lines().next().unwrap_or("").split_whitespace();
                        let method = first.next().unwrap_or("").to_string();
                        let path = first.next().unwrap_or("").to_string();
                        lock(&hits).push(format!("{method} {path}"));
                        let (status, reply) = handler(&method, &path, &body);
                        let response = http_response(
                            &format!("{status} X"),
                            &[("Content-Type", "application/json")],
                            &reply,
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });
            Self {
                base,
                hits,
                _task: task,
            }
        }

        fn hits(&self, prefix: &str) -> usize {
            lock(&self.hits)
                .iter()
                .filter(|h| h.starts_with(prefix))
                .count()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self._task.abort();
        }
    }

    fn mocked_accounts(root: &Path, base: &str) -> AgentAccounts {
        AgentAccounts::with_endpoints(
            AgentAccountsConfig::isolated(root),
            ProbeEndpoints {
                symphony_anthropic_token: format!("{base}/anthropic/token"),
                symphony_xai_device: format!("{base}/xai/device"),
                symphony_xai_token: format!("{base}/xai/token"),
                claude_profile: format!("{base}/anthropic/profile"),
                allow_slot_refresh: false,
                ..Default::default()
            },
            Default::default(),
        )
    }

    async fn await_success(accounts: &AgentAccounts, id: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let poll = accounts.poll_login(id).await.unwrap();
                if poll.status != AgentLoginStatus::Pending {
                    assert_eq!(poll.status, AgentLoginStatus::Done, "{poll:?}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("mock login did not settle");
    }

    #[tokio::test]
    async fn symphony_grok_device_pending_slow_down_success() {
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let times = Arc::new(Mutex::new(Vec::new()));
        let recorded = times.clone();
        let server = MockServer::start(move |method, path, body| {
            assert_eq!(method, "POST");
            if path == "/xai/device" {
                assert!(body.contains("client_id="));
                assert!(body.contains("scope="));
                return (200, serde_json::json!({
                    "device_code":"device-secret", "user_code":"ABCD",
                    "verification_uri":"https://mock.invalid/activate",
                    "verification_uri_complete":"https://mock.invalid/activate?code=ABCD",
                    "expires_in":60, "interval":1
                }).to_string());
            }
            assert_eq!(path, "/xai/token");
            assert!(body.contains("device_code=device-secret"));
            assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
            lock(&recorded).push(Instant::now());
            match requests.fetch_add(1, Ordering::SeqCst) {
                0 => (400, r#"{"error":"authorization_pending"}"#.into()),
                1 => (400, r#"{"error":"slow_down"}"#.into()),
                2 => (200, serde_json::json!({
                    "access_token":"mock-grok-access", "refresh_token":"mock-refresh",
                    "id_token":format!("e30.{}.sig", BASE64_URL.encode(br#"{"sub":"grok-user","email":"grok@mock.invalid"}"#)),
                    "expires_in":3600
                }).to_string()),
                _ => panic!("unexpected extra token request"),
            }
        }).await;
        let tmp = tempfile::tempdir().unwrap();
        let accounts = mocked_accounts(tmp.path(), &server.base);
        let start = accounts
            .start_login_with(HarnessId::Symphony, Some("grok"), None)
            .await
            .unwrap();
        assert_eq!(start.url, "https://mock.invalid/activate?code=ABCD");
        let pending = accounts.poll_login(&start.login_id).await.unwrap();
        assert_eq!(pending.status, AgentLoginStatus::Pending);
        assert!(pending.message.unwrap().contains("ABCD"));
        await_success(&accounts, &start.login_id).await;
        assert_eq!(count.load(Ordering::SeqCst), 3);
        let times = lock(&times);
        assert!(times[2].duration_since(times[1]) >= Duration::from_secs(6));
        let auth = read_json(&tmp.path().join("symphony/oauth/grok.json")).unwrap();
        assert_eq!(auth["account_id"], "grok-user");
        assert!(!tmp.path().join("symphony/oauth/xai.json").exists());
    }

    #[tokio::test]
    async fn symphony_grok_cancel_stops_polling() {
        let server = MockServer::start(|_, path, _| {
            assert_eq!(path, "/xai/device");
            (
                200,
                serde_json::json!({"device_code":"d", "user_code":"CANCEL",
                "verification_uri":"https://mock.invalid", "interval":1})
                .to_string(),
            )
        })
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let accounts = mocked_accounts(tmp.path(), &server.base);
        let start = accounts
            .start_login_with(HarnessId::Symphony, Some("xai"), None)
            .await
            .unwrap();
        accounts.cancel_login(&start.login_id);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(accounts.poll_login(&start.login_id).await.is_err());
        assert_eq!(server.hits("POST /xai/token"), 0);
        assert!(!tmp.path().join("symphony/oauth/grok.json").exists());
    }

    #[tokio::test]
    async fn symphony_claude_setup_token_rejects_api_key_and_preserves_state() {
        let server = MockServer::start(|method, path, _| {
            assert_eq!((method, path), ("GET", "/anthropic/profile"));
            (
                200,
                r#"{"account":{"uuid":"claude-user","email":"claude@mock.invalid"}}"#.into(),
            )
        })
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let accounts = mocked_accounts(tmp.path(), &server.base);
        let start = accounts
            .start_login_with(HarnessId::Symphony, Some("anthropic"), None)
            .await
            .unwrap();
        for invalid in [
            format!("sk-ant-api03-{}", "x".repeat(120)),
            "code#wrong-state".into(),
        ] {
            assert!(
                accounts
                    .complete_login(&start.login_id, &invalid)
                    .await
                    .is_err()
            );
            assert_eq!(
                accounts.poll_login(&start.login_id).await.unwrap().status,
                AgentLoginStatus::Pending
            );
            assert_eq!(server.hits("GET"), 0);
            assert!(!tmp.path().join("symphony/oauth/anthropic.json").exists());
        }
        accounts
            .complete_login(&start.login_id, "sk-ant-oat01-mock-setup-token")
            .await
            .unwrap();
        assert!(accounts.poll_login(&start.login_id).await.is_err());
        let auth = read_json(&tmp.path().join("symphony/oauth/anthropic.json")).unwrap();
        assert_eq!(auth["account_id"], "claude-user");
        assert_eq!(server.hits("GET /anthropic/profile"), 1);
    }

    #[tokio::test]
    async fn symphony_claude_mock_code_exchange() {
        let expected = Arc::new(Mutex::new(None::<(String, String)>));
        let checked = expected.clone();
        let server = MockServer::start(move |method, path, body| {
            match (method, path) {
                ("POST", "/anthropic/token") => {
                    let json: serde_json::Value = serde_json::from_str(body).unwrap();
                    let (state, challenge) = lock(&checked).clone().unwrap();
                    assert_eq!(json["state"], state);
                    assert_eq!(json["code"], "mock-code");
                    assert_eq!(json["grant_type"], "authorization_code");
                    assert_eq!(json["client_id"], CLAUDE_CLIENT_ID);
                    assert_eq!(json["redirect_uri"], CLAUDE_REDIRECT);
                    use sha2::Digest;
                    assert_eq!(BASE64_URL.encode(sha2::Sha256::digest(json["code_verifier"].as_str().unwrap().as_bytes())), challenge);
                    (200, r#"{"access_token":"mock-code-token","refresh_token":"mock-refresh","expires_in":3600}"#.into())
                }
                ("GET", "/anthropic/profile") => (200, r#"{"account":{"uuid":"code-user"}}"#.into()),
                _ => panic!("unexpected request {method} {path}"),
            }
        }).await;
        let tmp = tempfile::tempdir().unwrap();
        let accounts = mocked_accounts(tmp.path(), &server.base);
        let start = accounts
            .start_login_with(HarnessId::Symphony, Some("anthropic"), None)
            .await
            .unwrap();
        let url = reqwest::Url::parse(&start.url).unwrap();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .unwrap()
                .1
                .into_owned()
        };
        let state = param("state");
        *lock(&expected) = Some((state.clone(), param("code_challenge")));
        accounts
            .complete_login(&start.login_id, &format!("mock-code#{state}"))
            .await
            .unwrap();
        assert_eq!(server.hits("POST /anthropic/token"), 1);
        assert_eq!(server.hits("GET /anthropic/profile"), 1);
        assert!(accounts.poll_login(&start.login_id).await.is_err());
        assert_eq!(
            read_json(&tmp.path().join("symphony/oauth/anthropic.json")).unwrap()["account_id"],
            "code-user"
        );
    }

    #[test]
    fn callback_formats() {
        assert_eq!(
            parse_callback(" code#state ").unwrap(),
            ("code".into(), "state".into())
        );
        assert_eq!(
            parse_callback("https://console.anthropic.com/oauth/code/callback?code=a%2Bb&state=s")
                .unwrap(),
            ("a+b".into(), "s".into())
        );
        assert_eq!(
            parse_callback("https://example.com/#code=a&state=s").unwrap(),
            ("a".into(), "s".into())
        );
        assert!(parse_callback("bare-code").is_err());
        assert!(parse_callback("https://example.com/?error=denied").is_err());
    }
}
