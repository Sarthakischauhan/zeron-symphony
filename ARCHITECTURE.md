# Zeron Symphony architecture

Zeron is the native desktop workspace and session manager. Symphony is the sole production agent runtime. The engine speaks Symphony's version 2 JSONL stdio protocol; it never falls back to a different agent CLI or SDK.

## Responsibilities

| Component | Responsibility |
| --- | --- |
| `apps/zeron` | Desktop/headless entrypoint and Zeron MCP entrypoint |
| `crates/ui` | GPUI chat, composer, model/command pickers, subagent cards, browser and terminal views |
| `crates/engine` | Prompt queue, workspace state, provider-account management, process coordination and RPC |
| `crates/harness` | Symphony stdio adapter, process ownership, discovery and scripted test mock |
| `crates/proto`, `crates/rpc` | Shared events, commands and device RPC contracts |
| `crates/doc`, `crates/sync` | Loro session documents, local persistence and optional synchronization |
| `crates/preview`, `crates/client`, `crates/mobile` | Preview transports and shared device/mobile support |
| `edge` | Optional auth, device relay, registry and chat synchronization |
| Symphony | Provider/model selection, agent loop, tool execution, approvals, skills/plugins and checkpoints |

## Session lifecycle

The composer queues a durable prompt. The host engine dispatches it through `SymphonyHarness`. A Symphony subprocess is owned by the Zeron session and reused across successful turns. Zeron negotiates protocol version 2 and discovers the live model/command catalogs; correlated responses prevent discovery frames from being confused with prompt events.

Text, tool results, approval questions, context usage and child-agent events become Zeron transcript parts and session documents. Follow-up prompts are dispatched at turn boundaries. Interrupt, session release and engine shutdown clean up the owned child process. A successful turn does not terminate it.

Models and their provider labels come from Symphony. Provider OAuth/API-key management writes Symphony's native credential files. Removing old agent runtimes does not remove OpenAI, Anthropic or Grok model access through Symphony.

## Compatibility

Historical harness identifiers and optional wire fields remain serializable so old session documents and peers can be read. They do not register executable runtimes. Stored chats that explicitly select a removed harness cannot execute with that harness; create a Symphony session instead. Existing stored files are not deleted by this source cleanup.

The mock harness exists for deterministic engine/UI tests and demos. Production startup uses `symphony_registry`, which exposes only Symphony. Automatic title generation has no production driver until Symphony implements a restricted title path.

## Execution and deployment

The same binary can run headed or with `zeron headless`. The UI can connect to an existing engine; local documents persist without online sync. Optional edge services and mobile/shared transport components remain because they support the workspace rather than running agent models.

Symphony must be installed independently on the engine host. Set `SYMPHONY_EXECUTABLE`, and optionally `SYMPHONY_ROOT`. Zeron's sandbox label does not enforce isolation for Symphony: Symphony's own tool policy governs filesystem access and approvals. See [integration setup](docs/symphony.md) for current limitations.
