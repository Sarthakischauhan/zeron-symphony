# Symphony harness adapter

This crate bridges Zeron's engine contracts to version 2 of `symphony stdio`. It owns process lifecycle, model/command discovery, event mapping, questions and session resume. The scripted mock supports deterministic engine and UI tests.

Symphony is installed separately. No vendor agent installers, native agent SDK shims or ACP execution adapters are bundled. `can_install` is always false; attempted install RPCs fail with Symphony setup guidance.

See [setup and limitations](../../docs/symphony.md), [accounts](../../docs/reference/symphony-accounts.md) and [commands](../../docs/reference/symphony-commands.md).

```bash
cargo test --locked -p zeron-harness
```
