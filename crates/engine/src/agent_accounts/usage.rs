//! Quota probes for Symphony OpenAI, Anthropic and Grok provider accounts.

use super::*;

pub(super) const GROK_USAGE_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
impl AgentAccounts {
    async fn grok_usage_request(&self, access_token: &str) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            "grok",
            "usage",
            self.inner
                .http
                .get(&self.inner.endpoints.grok_usage)
                .bearer_auth(access_token)
                .header("X-XAI-Token-Auth", "xai-grok-cli"),
        )
        .await?;
        grok_usage_snapshot(&body).ok_or_else(|| schema_error("grok", &body))
    }

    /// A per-provider login (OpenCode / Pi entry, Hermes pool entry): probe
    /// the vendor behind it with its own token.
    pub(super) async fn keyed_usage(
        &self,
        harness: HarnessId,
        slot: &Slot,
    ) -> Result<UsageSnapshot, ProbeError> {
        if harness != HarnessId::Symphony {
            return Err(ProbeError::NoCredentials {
                why: NoCredentials::Unsupported,
            });
        }
        let key = slot.store_key.as_deref().ok_or(ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        })?;
        let access =
            str_field(&slot.credentials, "access_token").ok_or(ProbeError::NoCredentials {
                why: NoCredentials::Missing,
            })?;
        match key {
            "openai" => {
                self.openai_usage(
                    "symphony",
                    &access,
                    &str_field(&slot.credentials, "account_id").unwrap_or_default(),
                )
                .await
            }
            "anthropic" => self.claude_usage_request(&access).await,
            "grok" => self.grok_usage_request(&access).await,
            _ => Err(ProbeError::NoCredentials {
                why: NoCredentials::Unsupported,
            }),
        }
    }

    async fn openai_usage(
        &self,
        provider: &'static str,
        access_token: &str,
        account_id: &str,
    ) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            provider,
            "usage",
            self.inner
                .http
                .get(&self.inner.endpoints.codex_usage)
                .bearer_auth(access_token)
                .header("chatgpt-account-id", account_id),
        )
        .await?;
        codex_usage_snapshot(&body).ok_or_else(|| schema_error(provider, &body))
    }
}

/// Grok's `/billing?format=credits`: the current period, with the Grok Build
/// product's share of it (the blended `creditUsagePercent` as fallback).
pub(super) fn grok_usage_snapshot(body: &serde_json::Value) -> Option<UsageSnapshot> {
    let config = body.get("config")?;
    let period = config.get("currentPeriod");
    let used_percent = config
        .get("productUsage")
        .and_then(|u| u.as_array())
        .and_then(|products| {
            products.iter().find_map(|p| {
                (p.get("product").and_then(|v| v.as_str()) == Some("GrokBuild"))
                    .then(|| p.get("usagePercent").and_then(|v| v.as_f64()))
                    .flatten()
            })
        })
        .or_else(|| config.get("creditUsagePercent").and_then(|v| v.as_f64()))?;
    let label = match period.and_then(|p| p.get("type")).and_then(|v| v.as_str()) {
        Some("USAGE_PERIOD_TYPE_DAILY") => "Day",
        Some("USAGE_PERIOD_TYPE_WEEKLY") => "Week",
        Some("USAGE_PERIOD_TYPE_MONTHLY") => "Month",
        _ => "Period",
    };
    Some(UsageSnapshot {
        windows: vec![AgentUsageWindow {
            label: label.to_string(),
            used_fraction: (used_percent / 100.0) as f32,
            resets_at: parse_when(
                period
                    .and_then(|p| p.get("end"))
                    .or_else(|| config.get("billingPeriodEnd")),
            ),
        }],
        plan_label: None,
    })
}
