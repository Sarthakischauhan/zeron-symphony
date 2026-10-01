# Symphony accounts

Zeron's **Settings → Symphony → Accounts** manages Symphony's own subscription
credentials, independently from Claude Code, Codex and Grok CLI accounts.

Supported sign-ins:

- **ChatGPT**: OpenAI PKCE browser login with the registered localhost callback
  on port 1455. Remote-device sign-in uses Zeron's existing callback tunnel.
- **Claude**: paste a token from `claude setup-token`, or complete the browser
  authorization and paste the full `code#state` / callback URL. API keys are not
  accepted as subscription setup tokens.
- **Grok**: xAI device authorization. Approve the displayed code in the browser;
  Zeron polls until the grant succeeds, expires or is cancelled.

Existing credentials are detected from `~/.symphony/oauth/openai.json`,
`anthropic.json` and `grok.json`. If the Symphony harness uses a checkout-local
home (a `SYMPHONY_ROOT` or executable ancestor containing `.symphony/config.json`),
the manager uses that same checkout's `.symphony` directory instead. Each provider
has its own active account.
Switching ChatGPT does not replace Claude or Grok. Adding another account saves
it without replacing an existing different account; the first account and a
re-login to the current account are adopted automatically.

Activation writes Symphony's native token schema, retains additional fields,
and sets `SYMPHONY_<PROVIDER>_AUTH=oauth` in `~/.symphony/.env`, preserving other
settings and API keys. Process environment and workspace `.env` overrides still
have Symphony's normal precedence. Restart a running Symphony chat after
switching accounts so its already-loaded credentials are replaced.

Forgetting an inactive account removes its Zeron snapshot. Forgetting an active
account also removes that provider's live OAuth file; other providers are left
alone. Credential files and snapshots are owner-only and secrets are not
included in the account-list response.

## Tests

Tests use temporary homes and mocked endpoints, never your real credentials:

```sh
cargo test --locked -p zeron-engine --lib symphony -- --test-threads=1
cargo test --locked -p zeron-engine --test symphony_accounts --test symphony_accounts_rpc
cargo test --locked -p zeron-ui --lib symphony -- --test-threads=1
cargo test --locked -p zeron-harness symphony -- --test-threads=1
```

A successful mocked OAuth test is not a live provider/browser smoke test;
interactive approval against production providers must be verified separately.
