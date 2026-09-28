# Symphony in Zeron

This fork runs the Symphony coding agent through its `symphony stdio` JSONL
transport. Zeron hosts the session UI, transcript, approval questions, and
optional device sync; Symphony owns its model loop, tools, skills, plugins,
and checkpoint files. The GPUI implementation remains in `crates/ui/` for
further customization.

Zeron's `sandbox` setting is not applied by this driver. Symphony's own
approval and deny rules govern tools; its workspace tools can address paths
outside the selected repository. Do not treat Zeron's `WorkspaceWrite` label
as filesystem isolation for Symphony sessions.

## Local setup

1. Install the Symphony branch containing `symphony stdio` on the machine
   running the Zeron engine. From the Symphony source checkout, run
   `uv sync --all-packages` and set `SYMPHONY_EXECUTABLE` to the absolute path
   of its `.venv/bin/symphony`. Configure a model provider in Symphony.
2. Build this fork with `cargo build --release --locked -p zeron`.
3. Run the built `target/release/zeron`, enable Symphony under Settings →
   Providers if needed, and select Symphony for a new session. For a remote
   engine, install Symphony and set `SYMPHONY_EXECUTABLE` there instead.

Each Zeron prompt starts a Symphony subprocess. Symphony's session ID is
stored with the Zeron chat so later prompts resume its JSONL checkpoint.
The model picker lists models from Symphony's configured providers. Approval questions and `ask_user` are handled
through Zeron's input panel. This driver does not yet map Symphony's child
agent tree, plan widgets, or live steering into
specialized Zeron views. Those events remain in Symphony's own persistence.
