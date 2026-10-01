# Symphony commands in Zeron

## Argument completion

Symphony's `command/list` response can include an optional `options` array:

```json
{
  "name": "personality",
  "description": "View or switch the agent personality",
  "inputHint": "[id]",
  "options": [
    {"value": "precise", "label": "Precise", "description": "Careful and exact"}
  ]
}
```

Zeron preserves this metadata through engine RPC. Selecting `/personality` opens
an argument completion popup; typing `/personality ` also opens it. Choices are
filtered as the argument is typed, and keyboard or mouse selection inserts the
argument. Submit to apply the choice. The command identity remains `personality`,
not `personality precise`, including when the composer uses canonical references.

The updated Symphony checkout provides dynamic choices for personality, mode,
effort, Jev, and saved workspace plans. Older installations that omit `options`
continue to support ordinary command insertion and submission. Bare commands
still return their available choices in the transcript.

## Native actions

For Symphony, Zeron owns these actions:

- `/model`: model picker; `/model <id>` selects a known model.
- `/new`: new conversation.
- `/diff`: workspace changes surface.
- `/provider`: provider settings and accounts.
- `/clear`: hide existing entries in this window. Saved history, backend context,
  and other viewers are unchanged. New entries remain visible; leaving and
  reopening the conversation restores its history.
- `/quit`: Zeron's normal quit flow, including its existing save safeguards.

Native actions consume their trigger without submitting the remaining draft or
attachments to the agent.

## Backend commands

`/personality`, `/mode`, `/plan`, `/effort`, `/jev`, `/plans`, `/compact`, `/status`,
`/context`, `/learning`, `/installed`, `/dashboard`, `/reload`, `/help`, and
`/langfuse` execute through Symphony's stdio command handlers rather than an LLM
request. Transcript results remain the fallback presentation for these commands.

The updated backend makes `/reload` reload environment/configuration and rebuild
the live system prompt. `/langfuse` remains a setup-instructions command; it does
not install an SDK or open a telemetry configuration dialog. `/dashboard` lists
active agents rather than promising the TUI's metrics modal.

Personality and Jev settings use Symphony's existing global config persistence.
Mode and effort retain Symphony's session-state persistence. Zeron forwards
reasoning selections in run frames and remembers submitted effort commands in
chat settings, including `none` and `default`, so the next turn does not silently
undo the command. Selecting a reasoning level in the picker replaces that override.

Known backend commands reject image attachments explicitly instead of silently
falling back to an ordinary model prompt. Whitespace-separated arguments work in
the backend, including tabs and trailing spaces.

## Installation and verification

Dynamic options and live reload require the accompanying changes in the local
Symphony checkout (`coding_agent/protocols/commands.py` and `stdio.py`, plus environment reload support in `credentials.py`); rebuilding
Zeron alone does not upgrade a separately installed Symphony executable.

Targeted checks:

```sh
cargo test --locked -p zeron-proto --lib invocation
cargo test --locked -p zeron-harness --lib symphony
cargo test --locked -p zeron-ui --lib command_argument
cargo test --locked -p zeron-ui --lib personality_argument_picker
cargo test --locked -p zeron-ui --lib workspace_command
cargo test --locked -p zeron-ui --lib symphony_native_commands
cargo test --locked -p zeron-ui --lib clear_view
# In the Symphony coding_agent directory:
python -m pytest tests/test_stdio.py tests/test_personalities.py tests/test_credentials.py
```

The existing engine native-command guard keeps slash commands separate from fork
history bootstrapping. No new protocol version is required: argument metadata
and run reasoning fields are additive.
