# Symphony-first Zeron port

**Status:** Proposed  
**Target:** Turn `zeron-symphony` into a Symphony-first desktop and local/remote session manager.  
**Architecture:** Symphony remains the coding-agent runtime; Zeron becomes its purpose-built app and manager.

## Goal

Deliver a Zeron-derived app whose only coding-agent runtime is Symphony, with a UI designed around Symphony's real capabilities. This goes beyond registering Symphony as one selectable provider. Users should install and open the app, configure Symphony, choose a workspace and model, and use the full Symphony experience without installing or selecting another agent.

The port should preserve Zeron's useful app foundations—desktop shell, workspace handling, engine lifecycle, session browsing, and optional device sync—where they fit. It should replace provider-centric product flows and redesign the UI to fit Symphony.

## Repository boundary

- **Symphony repository:** Own the versioned host protocol under `protocols/`, including the stdio transport and schemas. The protocol is the stable boundary between Symphony and host apps.
- **Zeron fork:** Own Symphony runtime management, protocol client, session and event presentation, app configuration, and the Symphony-first product/UI.
- Keep Symphony's agent loop, model providers, tools, skills, plugins, hooks, memories, compaction, and persistence in Symphony. Do not reimplement these systems in Zeron.
- Keep the current provider PR as an integration milestone. This full-port plan is a separate, broader effort.

## Work plan

### 1. Lock the product and protocol contract

- [ ] Define the first supported Symphony host protocol version in Symphony's `protocols/` directory.
- [ ] Specify handshake, protocol version negotiation, capability discovery, structured errors, request IDs, cancellation, and clean shutdown.
- [ ] Define bidirectional operations for starting and resuming runs, steering, approvals and user questions, interruption, and image/file attachments.
- [ ] Define structured events for assistant output, reasoning, tool lifecycle/results, plan changes, child agents, skills/plugins, compaction, and terminal run status.
- [ ] Define session operations needed by the app: list, inspect, resume, rename, and delete, including how Symphony's checkpoint IDs map to app-visible sessions.
- [ ] Publish example JSONL transcripts and protocol compatibility tests in the Symphony repository.
- [ ] Document which capabilities are optional and how older protocol versions behave.

**Exit condition:** Zeron can implement against schemas and fixtures without parsing human-readable CLI output or inferring features from event names.

### 2. Make Symphony the managed runtime

- [ ] Replace the selectable-provider setup with a single Symphony runtime configuration in the app.
- [ ] Add setup and health checks for the Symphony executable, Python environment, configuration, and at least one usable model.
- [ ] Choose and implement a supported install strategy for macOS, Linux, and Windows: bundled runtime or a managed isolated environment.
- [ ] Add clear configuration for model credentials and Symphony settings without copying secrets into Zeron logs or synced session data.
- [ ] Supervise Symphony processes, stream diagnostics safely, handle crashes/timeouts, and support clean cancellation.
- [ ] Support local and remote Zeron engines with the runtime installed and configured on the machine that executes the session.
- [ ] Define how app and Symphony runtime versions are pinned, upgraded, and rolled back.

**Exit condition:** A clean machine can install the app, complete guided setup, and start Symphony without manually locating a Python virtual environment or editing internal paths.

### 3. Build a protocol-native session layer

- [ ] Replace provider-specific assumptions in the Zeron session layer with Symphony protocol operations and typed events.
- [ ] Preserve session identity across app restarts and engine reconnects; resume from Symphony checkpoints.
- [ ] Provide explicit states for starting, running, waiting for input, interrupted, completed, and failed sessions.
- [ ] Preserve structured tool inputs/results, reasoning visibility settings, attachments, and error details in the transcript.
- [ ] Handle unknown optional events safely and report incompatible required protocol versions clearly.
- [ ] Keep Symphony as the source of truth for agent state; store only app metadata locally unless the protocol defines a durable export format.

**Exit condition:** Start, resume, interrupt, reconnect, and inspect sessions through the protocol with no dependency on the current provider adapter's private behavior.

### 4. Redesign the app around Symphony

- [ ] Remove the multi-provider picker and provider-specific install/update/settings screens from the user-facing product.
- [ ] Establish the Symphony-first information architecture and visual design for workspace selection, conversation history, active runs, and settings.
- [ ] Redesign the composer for prompts, attachments, model selection, and queued follow-up instructions.
- [ ] Present approval and user-question requests with their exact choices and request IDs; closing a dialog must never silently approve an action.
- [ ] Build clear transcript views for assistant output, tool activity, diffs, failures, and run completion.
- [ ] Add dedicated plan and child-agent views when the protocol reports those capabilities.
- [ ] Surface skills, plugins, hooks, memories, and compaction in ways that help users understand what Symphony did without exposing noisy internals by default.
- [ ] Keep keyboard navigation, accessibility, responsive layouts, theme support, and localization working across the redesigned flows.

**Exit condition:** A first-time user can configure Symphony, start a task, understand progress, answer an approval, and resume the session using the app's normal UI.

### 5. Match safety controls to actual enforcement

- [ ] Map each Zeron workspace/sandbox setting to a Symphony-enforced policy or remove that setting from Symphony sessions.
- [ ] Define filesystem, shell, network, and approval boundaries, including whether Symphony can access paths outside the selected workspace.
- [ ] Ensure the UI states the effective policy before a run starts and never implies isolation that the runtime does not enforce.
- [ ] Add tests for denied and approved operations, cancellation during approval, path escape attempts, and unattended mode.
- [ ] Default to a restrictive, understandable policy when enforcement is unavailable.

**Exit condition:** Every safety label shown by Zeron corresponds to a tested runtime control.

### 6. Package, validate, and release

- [ ] Add protocol conformance tests shared with Symphony fixtures.
- [ ] Add end-to-end tests for setup, first run, streamed events, approval, image attachment, interruption, restart, and resume.
- [ ] Verify supported operating systems and both local-only and synced/remote-engine workflows.
- [ ] Measure startup time, event latency, memory use, and recovery after a Symphony crash.
- [ ] Update installation, troubleshooting, privacy, and upgrade documentation.
- [ ] Publish a preview release and collect feedback before removing transitional provider code from the fork.

**Exit condition:** CI covers the supported platform matrix, setup is documented, safety behavior is verified, and the release can upgrade the app and runtime without losing sessions.

## Completion criteria

The port is complete when:

1. The fork launches as a Symphony-only product, with no other agent provider required or exposed.
2. Symphony is installed/configured and supervised through the app's supported setup flow.
3. Runs use the versioned `protocols/` contract for events and control; no text scraping or one-off adapter protocol remains.
4. Sessions resume reliably and show Symphony's core UI capabilities, including approvals, steering, plans, child agents, tools, and attachments as supported by the protocol.
5. Zeron's visible safety controls match tested enforcement in Symphony.
6. The app builds, tests, and runs on each declared platform, with clear setup and recovery guidance.

## Decisions to make before implementation

- [ ] Bundle Symphony or manage a pinned isolated Python environment.
- [ ] Decide whether Symphony protocol/session operations need a long-lived daemon mode in addition to stdio.
- [ ] Decide which Symphony capabilities are required for the first release versus capability-gated.
- [ ] Choose the default safety policy and the supported workspace boundary.
- [ ] Define how much of Zeron's device sync remains in the Symphony-first app.

## Current baseline and known gaps

The existing provider integration starts Symphony with `symphony stdio`, streams a subset of events, discovers models, forwards images, and stores a session ID for resumption. It is a useful protocol spike. It does not yet deliver managed runtime installation, a Symphony-only product flow, complete session operations, dedicated plan/child-agent/skill views, live steering, or equivalent sandbox enforcement. The existing Symphony setup still asks users to install and locate a Python environment manually.
