# Symphony in Zeron

This fork runs the Symphony coding agent through its versioned `symphony stdio` JSONL
transport. Zeron hosts the session UI, transcript, approval questions, and
optional device sync; Symphony owns its model loop, tools, skills, plugins,
and checkpoint files. The GPUI implementation remains in `crates/ui/` for
further customization.

New Symphony chats record full filesystem access because Zeron's sandbox
setting is not applied by this driver. Symphony's own
approval and deny rules govern tools; its workspace tools can address paths
outside the selected repository. Older chats may still show `WorkspaceWrite`;
that label does not provide filesystem isolation for Symphony sessions.

## Local setup

1. Check out the current `main` branch from `Sarthakischauhan/symphony` on the
   machine running the Zeron engine. Run `uv sync --all-packages` there and
   set `SYMPHONY_EXECUTABLE` to the absolute path of `.venv/bin/symphony`.
   Configure a model provider in Symphony and check it with
   `printf '{"type":"model/list","request_id":"probe"}\n' | "$SYMPHONY_EXECUTABLE" stdio`.
2. Build this fork with `cargo build --release --locked -p zeron`.
3. Run the built `target/release/zeron` from this branch. New sessions use
   Symphony; it is the only advertised runtime. For a remote engine, install
   Symphony and set `SYMPHONY_EXECUTABLE` on that device instead.

Each Zeron session owns a persistent Symphony subprocess, reused across successful turns. Symphony's session ID is stored with the Zeron chat so later process launches can resume its JSONL checkpoint.
The model picker lists models from Symphony's configured providers. After the
version 2 `ready` frame, Zeron sends a correlated `model/list` request on
stdio. The `models` response includes a qualified ID, display label, provider,
context limit, and reasoning levels. The model rows and selected model chip
show provider marks for OpenAI, Anthropic, Gemini, and Grok; other namespaces
use the Symphony mark. The harness rail always uses the Symphony mark. The composer shows context usage reported by Symphony. Approval questions and `ask_user` are handled
through Zeron's input panel. Zeron's auto-approve option selects Symphony's
unattended policy, including its deny rules. Symphony child-agent activity
is routed to Zeron's existing spawn cards and subagent transcript tabs using
the spawning tool call ID. Plan widgets and live steering are not yet mapped
into Zeron's specialized views.
Zeron owns the durable prompt queue and dispatches follow-ups at turn
boundaries. The host protocol announces version 2; a mismatched Symphony
installation fails with an update message before starting a prompt.

## Runtime cleanup and compatibility

Only Symphony and the scripted test mock remain in the harness crate. Native Claude Code, Codex, Cursor and OpenCode executors, ACP adapters, their managed installers and vendor-specific maintenance jobs have been removed. The provider accounts UI discovers and mutates only Symphony credentials.

Historical harness IDs remain in persisted/wire schemas for read compatibility. A chat that still names an old runtime cannot execute it; use a new Symphony chat. No migration silently changes its agent or model. Old on-disk credentials and session data are left intact.

Automatic titles have no production driver yet. Workspace services, terminals, browser previews, device sync and the Zeron MCP server remain part of the app.
