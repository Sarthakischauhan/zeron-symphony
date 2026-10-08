//! Native Symphony ChatGPT authorization through a PKCE loopback callback.

use super::*;

/// OpenAI's OAuth issuer (`/oauth/authorize`, `/oauth/token`).
pub(super) const OPENAI_AUTH: &str = "https://auth.openai.com";
/// The Codex CLI's public client — the one OpenCode and Pi sign in with.
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Fixed by the client's registered redirect.
pub(super) const OPENAI_LOOPBACK_PORT: u16 = 1455;
const OPENAI_SCOPES: &str = "openid profile email offline_access";

/// How long a sign-in waits on the browser before giving up.
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);

impl AgentAccounts {
    /// Symphony ChatGPT sign-in uses PKCE against our loopback on port 1455.
    pub(super) async fn start_openai_login(
        &self,
        harness: HarnessId,
        store_key: &'static str,
    ) -> Result<AgentLoginStart, EngineError> {
        debug_assert_eq!(harness, HarnessId::Symphony);
        self.reap_spawned_flows(harness);
        let wanted = self.inner.endpoints.openai_port;
        self.reap_port_flows(OPENAI_LOOPBACK_PORT);
        let listener = bind_loopback(wanted).await.map_err(|err| {
            EngineError::Other(if err.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "Port {wanted} is in use — another ChatGPT sign-in (codex login, OpenCode or \
                     Pi) is running. Finish or cancel it, then try again."
                )
            } else {
                format!("Could not open the sign-in callback: {err}")
            })
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| EngineError::Other(format!("sign-in callback has no port: {e}")))?
            .port();
        let login_id = new_id();
        let (verifier, challenge) = pkce_pair();
        let state = random_url_token();
        let redirect = format!("http://localhost:{port}/auth/callback");
        let originator = "symphony";
        let url = format!(
            "{}/oauth/authorize?response_type=code&client_id={OPENAI_CLIENT_ID}\
             &redirect_uri={}&scope={}&code_challenge={challenge}\
             &code_challenge_method=S256&id_token_add_organizations=true\
             &codex_cli_simplified_flow=true&state={state}&originator={originator}",
            self.inner.endpoints.openai_auth.trim_end_matches('/'),
            urlencode(&redirect),
            urlencode(OPENAI_SCOPES),
        );
        let task_state = Arc::new(Mutex::new(TaskLoginState {
            url: Some(url.clone()),
            ..Default::default()
        }));
        let this = self.clone();
        let outcome_state = task_state.clone();
        let handle = tokio::spawn(async move {
            let outcome = tokio::time::timeout(
                LOGIN_WAIT,
                this.finish_openai_login(
                    listener, harness, store_key, &state, &verifier, &redirect,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(EngineError::Other("The sign-in timed out.".into())));
            lock(&outcome_state).outcome = Some(outcome.map_err(|e| e.to_string()));
        });
        lock(&self.inner.flows).insert(
            login_id.clone(),
            LoginFlow::Task {
                harness,
                started_at: Instant::now(),
                state: task_state,
                handle,
                port: Some(OPENAI_LOOPBACK_PORT),
            },
        );
        Ok(AgentLoginStart {
            login_id,
            url,
            mode: AgentLoginMode::Browser,
            callback_port: Some(port),
        })
    }

    async fn finish_openai_login(
        &self,
        listener: tokio::net::TcpListener,
        harness: HarnessId,
        store_key: &str,
        state: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<(), EngineError> {
        use tokio::io::AsyncWriteExt as _;
        let (callback, mut browser) =
            await_loopback_callback(&listener, "/auth/callback", state, "ChatGPT").await;
        drop(listener);
        let result = match callback {
            Ok(code) => {
                self.redeem_openai_code(harness, store_key, &code, verifier, redirect)
                    .await
            }
            Err(message) => Err(EngineError::Other(message)),
        };
        let (status, body) = match &result {
            Ok(()) => (
                "200 OK",
                "<!doctype html><title>Signed in</title><p>Signed in to ChatGPT.</p>\
                 <p>You can close this tab and return to Zeron.</p>"
                    .to_string(),
            ),
            Err(error) => (
                "400 Bad Request",
                format!(
                    "<!doctype html><title>Sign-in failed</title><p>{}</p>\
                     <p>Return to Zeron to try again.</p>",
                    html_escape(&error.to_string())
                ),
            ),
        };
        let response = http_response(
            status,
            &[("Content-Type", "text/html; charset=utf-8")],
            &body,
        );
        let _ = browser.write_all(response.as_bytes()).await;
        let _ = browser.shutdown().await;
        result
    }

    /// Redeem the code and save the login in the agent's entry format.
    async fn redeem_openai_code(
        &self,
        harness: HarnessId,
        store_key: &str,
        code: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<(), EngineError> {
        let response = self
            .inner
            .http
            .post(format!(
                "{}/oauth/token",
                self.inner.endpoints.openai_auth.trim_end_matches('/')
            ))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("client_id", OPENAI_CLIENT_ID),
                ("code_verifier", verifier),
            ])
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            // Never echo the body: it can carry token material on odd errors.
            return Err(EngineError::Other(format!(
                "OpenAI rejected the sign-in ({status}) — try again."
            )));
        }
        let tokens: serde_json::Value = response
            .json()
            .await
            .map_err(|e| EngineError::Other(format!("token exchange returned junk: {e}")))?;
        let (Some(access), Some(refresh)) = (
            str_field(&tokens, "access_token"),
            str_field(&tokens, "refresh_token"),
        ) else {
            return Err(EngineError::Other(
                "OpenAI returned no usable tokens — try signing in again.".into(),
            ));
        };
        let id_token = str_field(&tokens, "id_token");
        let expires_in = tokens
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let mut entry = serde_json::json!({
            "type": "oauth",
            "access": access,
            "refresh": refresh,
            "expires": now_ms() + expires_in * 1000,
        });
        let account_id = [id_token.as_deref(), Some(access.as_str())]
            .into_iter()
            .flatten()
            .filter_map(jwt_claims)
            .find_map(|claims| {
                claims
                    .get("https://api.openai.com/auth")
                    .and_then(|auth| str_field(auth, "chatgpt_account_id"))
            });
        if let (Some(account_id), Some(map)) = (account_id, entry.as_object_mut()) {
            map.insert("accountId".into(), serde_json::json!(account_id));
        }
        if harness == HarnessId::Symphony {
            let token = serde_json::json!({
                "access_token": access,
                "refresh_token": refresh,
                "id_token": id_token,
                "token_type": "Bearer",
                "expires_at": (now_ms() / 1000) + expires_in,
                "account_id": entry.get("accountId").and_then(|v| v.as_str()).unwrap_or(""),
                "scope": tokens.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
            });
            let detected = stores::symphony_detected(store_key, &token).ok_or_else(|| {
                EngineError::Other("Could not identify the signed-in ChatGPT account.".into())
            })?;
            return self.save_new_login(harness, &detected).await;
        }
        Err(EngineError::Other(
            "Only Symphony sign-in is supported.".into(),
        ))
    }
}

/// A loopback listener on `port` (0 = any): `localhost` resolves to either
/// family, so bind IPv4 and fall back to IPv6.
async fn bind_loopback(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => Err(err),
        Err(_) => tokio::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await,
    }
}
