//! Native Symphony driver. One stdio process handles one Zeron session.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, ToolCall, ToolDiff, UserInputQuestion,
};

use crate::{
    Harness, HarnessError, RunControls,
    process::{Command, Stdio},
};

pub struct SymphonyHarness {
    sessions: Arc<SessionTable>,
}

const PROTOCOL_VERSION: u64 = 2;
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

impl SymphonyHarness {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(SessionTable::default()),
        }
    }

    fn executable() -> Option<std::path::PathBuf> {
        if let Some(path) = std::env::var_os("SYMPHONY_EXECUTABLE") {
            return crate::executable::validate_native_override(std::path::Path::new(&path)).ok();
        }
        crate::executable::find_on_paths("symphony", Vec::new())
    }

    /// Return the checkout that owns the Symphony executable, when it has a
    /// project-local `.symphony/config.json`.
    fn root(executable: &std::path::Path) -> Option<std::path::PathBuf> {
        if let Some(root) = std::env::var_os("SYMPHONY_ROOT") {
            let root = std::path::PathBuf::from(root);
            return root.join(".symphony/config.json").is_file().then_some(root);
        }
        executable.ancestors().find_map(|candidate| {
            candidate
                .join(".symphony/config.json")
                .is_file()
                .then(|| candidate.to_path_buf())
        })
    }

    /// Credential/config directory used by the spawned agent. Account management
    /// must resolve the same home, including checkout-local configuration.
    pub fn credential_home() -> std::path::PathBuf {
        Self::executable()
            .as_deref()
            .and_then(Self::root)
            .unwrap_or_else(crate::executable::home_or_current_dir)
            .join(".symphony")
    }

    fn configure(command: &mut Command, executable: &std::path::Path) {
        if let Some(root) = Self::root(executable) {
            // Point only Symphony's config lookup at the checkout. Replacing
            // HOME makes Cargo, rustup, and other tools write caches there.
            command.env("SYMPHONY_HOME", root.join(".symphony"));
        }
    }
}

impl Default for SymphonyHarness {
    fn default() -> Self {
        Self::new()
    }
}

fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}

fn tool_call(name: &str, args: &Value) -> ToolCall {
    match name {
        "bash" => ToolCall::Exec {
            command: field(args, "command").into(),
        },
        "read_file" => ToolCall::ReadFile {
            path: field(args, "path").into(),
        },
        // `content` is WriteFileArgs.content, forwarded on
        // tool_execution_started.arguments (coding_agent/.../tools/write_file.py).
        "write_file" => ToolCall::WriteFile {
            path: field(args, "path").into(),
            content: args
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        // PatchArgs fields are path, old_str, new_str, replace_all
        // (coding_agent/.../tools/patch.py). EditFile has no replace_all slot.
        "patch" => ToolCall::EditFile {
            path: field(args, "path").into(),
            old_string: args
                .get("old_str")
                .and_then(Value::as_str)
                .map(str::to_owned),
            new_string: args
                .get("new_str")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "grep" | "search" => ToolCall::Search {
            pattern: field(args, "pattern").into(),
            path: args.get("path").and_then(Value::as_str).map(str::to_owned),
        },
        "glob" => ToolCall::Glob {
            pattern: field(args, "pattern").into(),
        },
        "web_search" => ToolCall::WebSearch {
            query: field(args, "query").into(),
        },
        "spawn_agent" => {
            let label = field(args, "label").trim();
            ToolCall::Unknown {
                name: if label.is_empty() {
                    "Agent".into()
                } else {
                    format!("Agent: {label}")
                },
                input: Some(args.clone()),
            }
        }
        _ => ToolCall::Unknown {
            name: name.into(),
            input: Some(args.clone()),
        },
    }
}

/// Relate Symphony's child IDs to the parent spawn call Zeron uses for its
/// clickable subagent chip and nested transcript. Events without a known
/// child are kept on the parent feed.
#[derive(Default)]
struct SubagentMapper {
    paths: HashMap<String, Vec<String>>,
    settled: HashSet<String>,
    patches: HashMap<String, Value>,
}

impl SubagentMapper {
    fn tag(path: &[String], mut event: AgentEvent) -> AgentEvent {
        for parent in path.iter().rev() {
            event = AgentEvent::Subagent {
                parent_tool_use_id: parent.clone(),
                event: Box::new(event),
            };
        }
        event
    }

    fn map(&mut self, event: &str, payload: &Value) -> Vec<AgentEvent> {
        let tool_id = field(payload, "tool_call_id");
        if event == "tool_execution_started" && field(payload, "tool_name") == "patch" {
            self.patches
                .insert(tool_id.into(), payload["arguments"].clone());
        }
        if event == "agent_spawned" {
            let child_id = field(payload, "child_id");
            let spawn_id = field(payload, "tool_call_id");
            if child_id.is_empty() || spawn_id.is_empty() {
                return Vec::new();
            }
            if !field(payload, "parent_id").is_empty()
                && !self.paths.contains_key(field(payload, "agent_id"))
            {
                return Vec::new();
            }
            let mut path = self
                .paths
                .get(field(payload, "agent_id"))
                .cloned()
                .unwrap_or_default();
            path.push(spawn_id.into());
            self.paths.insert(child_id.into(), path.clone());
            let prompt = field(payload, "prompt");
            return if prompt.is_empty() {
                Vec::new()
            } else {
                vec![Self::tag(
                    &path,
                    AgentEvent::UserMessage {
                        text: prompt.into(),
                    },
                )]
            };
        }
        if matches!(event, "agent_completed" | "agent_failed") {
            let child_id = field(payload, "child_id");
            let Some(path) = self.paths.remove(child_id) else {
                return Vec::new();
            };
            self.settled.insert(child_id.into());
            let failed = event == "agent_failed";
            let cancelled = failed && field(payload, "error_type") == "HarnessCancelled";
            return vec![Self::tag(
                &path,
                AgentEvent::Done {
                    status: if cancelled {
                        DoneStatus::Interrupted
                    } else if failed {
                        DoneStatus::Errored
                    } else {
                        DoneStatus::Completed
                    },
                    result: None,
                    error: failed.then(|| field(payload, "message").to_owned()),
                    session_id: None,
                },
            )];
        }
        let mut mapped = match map_event(event, payload) {
            Some(mapped) => mapped,
            None => return Vec::new(),
        };
        if event == "tool_execution_completed" {
            let args = self.patches.remove(tool_id);
            if let (Some(args), AgentEvent::ToolResult { diff, .. }) = (args, &mut mapped) {
                *diff = patch_diff(&args, payload);
            }
        }
        let agent_id = field(payload, "agent_id");
        if self.settled.contains(agent_id) {
            return Vec::new();
        }
        match self.paths.get(agent_id) {
            Some(path) => vec![Self::tag(path, mapped)],
            None if field(payload, "parent_id").is_empty() => vec![mapped],
            None => Vec::new(),
        }
    }
}

fn patch_diff(args: &Value, result: &Value) -> Option<ToolDiff> {
    if field(result, "status") != "success" || !field(result, "result").starts_with("patched ") {
        return None;
    }
    let path = field(args, "path");
    let old = args.get("old_str")?.as_str()?;
    let new = args.get("new_str")?.as_str()?;
    if path.is_empty() || old == new || args["replace_all"].as_bool() == Some(true) {
        return None;
    }
    // The TUI shows the exact replacement hunk, not a whole-file diff.
    // Keep both sides bounded before sending them through the transcript.
    const CAP: usize = 4_096;
    if old.len() > CAP || new.len() > CAP {
        return None;
    }
    Some(ToolDiff {
        path: path.into(),
        old_text: Some(old.into()),
        new_text: new.into(),
    })
}

fn map_event(event: &str, payload: &Value) -> Option<AgentEvent> {
    match event {
        "text_delta" => Some(AgentEvent::TextDelta {
            text: field(payload, "delta").into(),
        }),
        "reasoning_delta" => Some(AgentEvent::ReasoningDelta {
            text: field(payload, "delta").into(),
        }),
        "tool_execution_started" => Some(AgentEvent::ToolCall {
            id: field(payload, "tool_call_id").into(),
            call: tool_call(field(payload, "tool_name"), &payload["arguments"]),
        }),
        "tool_execution_completed" => Some(AgentEvent::ToolResult {
            id: field(payload, "tool_call_id").into(),
            is_error: matches!(field(payload, "status"), "error" | "cancelled" | "timeout"),
            output: Some(field(payload, "result").chars().take(8_000).collect()),
            // tool_completed_payload does not carry a diff. Leave it unset
            // until Symphony emits one; do not invent a key.
            diff: None,
        }),
        "assistant_message_completed" => Some(AgentEvent::AssistantMessageCompleted {
            assistant_message_id: field(payload, "assistant_message_id").into(),
        }),
        _ => None,
    }
}

fn catalog_models(frame: &Value, request_id: &str) -> Result<Vec<Model>, HarnessError> {
    check_version(frame)?;
    if field(frame, "type") != "models" || field(frame, "request_id") != request_id {
        return Err(HarnessError::Protocol(
            "Unexpected Symphony model response".into(),
        ));
    }
    let models = frame["models"]
        .as_array()
        .ok_or_else(|| HarnessError::Protocol("Symphony did not return a model list".into()))?;
    Ok(models
        .iter()
        .filter_map(|item| {
            let id = item["id"].as_str()?;
            let reasoning_levels = item["reasoning_levels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|level| serde_json::from_value::<ReasoningLevel>(level.clone()).ok())
                .collect();
            Some(Model {
                id: id.into(),
                label: field(item, "label").into(),
                description: Some(field(item, "description").into()),
                reasoning_levels,
                options: vec![],
            })
        })
        .collect())
}

type StdioLines = tokio::io::Lines<BufReader<crate::process::ChildStdout>>;

struct Bridge {
    child: crate::process::Child,
    stdin: crate::process::ChildStdin,
    lines: StdioLines,
    cwd: String,
    auto_approve: bool,
    /// `--model` captured at spawn. `None` means Symphony's configured default.
    launched_model: Option<String>,
    model_name: String,
    session_id: String,
}

struct Shared {
    stop: crate::CancellationToken,
    bridge: Mutex<Bridge>,
}

#[derive(Default)]
struct SessionTable {
    by_id: StdMutex<HashMap<String, Arc<Shared>>>,
    catalog: Mutex<Option<Bridge>>,
}

impl SessionTable {
    fn insert(&self, session_id: String, shared: Arc<Shared>) {
        self.by_id
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(session_id, shared);
    }

    fn remove_if_same(&self, session_id: &str, shared: &Arc<Shared>) {
        let mut guard = self.by_id.lock().unwrap_or_else(|err| err.into_inner());
        if guard
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, shared))
        {
            guard.remove(session_id);
        }
    }

    fn release(&self, session_id: &str) {
        if let Some(shared) = self
            .by_id
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(session_id)
        {
            shared.stop.cancel();
        }
    }

    fn idle_sessions(&self) -> Vec<Arc<Shared>> {
        self.by_id
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

fn child_running(child: &mut crate::process::Child) -> bool {
    !matches!(child.try_wait(), Ok(Some(_)))
}

fn launched_model(model: Option<&str>) -> Option<String> {
    model
        .filter(|model| *model != "default" && !model.is_empty())
        .map(str::to_owned)
}

fn bridge_matches(bridge: &Bridge, request: &RunRequest) -> bool {
    bridge.cwd == request.cwd
        && bridge.auto_approve == request.auto_approve
        && bridge.launched_model == launched_model(request.model.as_deref())
}

async fn take_matching(table: &SessionTable, request: &RunRequest) -> Option<Arc<Shared>> {
    let Some(session_id) = request.resume.as_deref().filter(|id| !id.is_empty()) else {
        return None;
    };
    let shared = table
        .by_id
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(session_id)
        .cloned()?;
    if shared.stop.is_cancelled() {
        table.remove_if_same(session_id, &shared);
        return None;
    }
    let mut bridge = shared.bridge.lock().await;
    let reusable = child_running(&mut bridge.child) && bridge_matches(&bridge, request);
    drop(bridge);
    if reusable {
        return Some(shared);
    }
    table.remove_if_same(session_id, &shared);
    None
}

fn launch(
    command: &mut crate::process::Command,
    cwd: &str,
    auto_approve: bool,
    model: Option<String>,
) -> Result<Bridge, HarnessError> {
    command
        .stdin(crate::process::Stdio::piped())
        .stdout(crate::process::Stdio::piped())
        .stderr(crate::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Protocol("no Symphony stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("no Symphony stdout".into()))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("Symphony: {line}");
            }
        });
    }
    Ok(Bridge {
        child,
        stdin,
        lines: BufReader::new(stdout).lines(),
        cwd: cwd.to_owned(),
        auto_approve,
        launched_model: model,
        model_name: String::new(),
        session_id: String::new(),
    })
}

fn run_command(exe: &std::path::Path, request: &RunRequest) -> crate::process::Command {
    let mut command = crate::process::Command::new(exe);
    command.arg("stdio").arg("--workspace").arg(&request.cwd);
    if let Some(resume) = request.resume.as_deref().filter(|id| !id.is_empty()) {
        command.arg("--session-id").arg(resume);
    }
    if let Some(model) = launched_model(request.model.as_deref()) {
        command.arg("--model").arg(model);
    }
    if request.auto_approve {
        command.arg("--unattended");
    }
    SymphonyHarness::configure(&mut command, exe);
    command.current_dir(&request.cwd);
    crate::compose_child_path(&mut command, exe);
    command
}

async fn read_frame(lines: &mut StdioLines) -> Result<Option<Value>, HarnessError> {
    let Some(line) = lines.next_line().await? else {
        return Ok(None);
    };
    match serde_json::from_str::<Value>(&line) {
        Ok(frame) => Ok(Some(frame)),
        Err(_) => Ok(Some(Value::Null)),
    }
}

async fn list_models(bridge: &mut Bridge) -> Result<Vec<Model>, HarnessError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = json!({"type":"model/list", "request_id": request_id}).to_string() + "\n";
    bridge.stdin.write_all(request.as_bytes()).await?;
    let probe = async {
        loop {
            let Some(frame) = read_frame(&mut bridge.lines).await? else {
                return Err(HarnessError::Protocol(
                    "Symphony exited before listing models".into(),
                ));
            };
            if frame.is_null() {
                continue;
            }
            if field(&frame, "type") == "error" {
                return Err(HarnessError::Protocol(field(&frame, "message").into()));
            }
            if field(&frame, "type") == "models" {
                return catalog_models(&frame, &request_id);
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), probe)
        .await
        .map_err(|_| HarnessError::Protocol("Symphony model discovery timed out".into()))?
}

async fn accept_ready(
    bridge: &mut Bridge,
    requested: Option<&str>,
    enforce_model: bool,
) -> Result<(), HarnessError> {
    loop {
        let Some(frame) = read_frame(&mut bridge.lines).await? else {
            return Err(HarnessError::Protocol(
                "Symphony exited before the handshake".into(),
            ));
        };
        if frame.is_null() {
            continue;
        }
        if field(&frame, "type") == "error" {
            return Err(HarnessError::Protocol(field(&frame, "message").into()));
        }
        check_version(&frame)?;
        if field(&frame, "type") != "ready" {
            return Err(HarnessError::Protocol("Symphony did not send ready".into()));
        }
        bridge.model_name = field(&frame, "model").to_owned();
        bridge.session_id = field(&frame, "session_id").to_owned();
        if enforce_model {
            check_ready_model(requested, field(&frame, "model"))?;
            if bridge.session_id.is_empty() {
                return Err(HarnessError::Protocol(
                    "Symphony ready frame has no session id".into(),
                ));
            }
        }
        return Ok(());
    }
}

async fn reap(bridge: &mut Bridge) {
    if child_running(&mut bridge.child) {
        let _ = bridge.child.start_kill();
    }
    let _ = bridge.child.wait().await;
}

#[async_trait]
impl Harness for SymphonyHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Symphony
    }
    fn display_name(&self) -> &str {
        "Symphony"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    fn installed(&self) -> bool {
        Self::executable().is_some()
    }
    fn executable_path(&self) -> Option<std::path::PathBuf> {
        Self::executable()
    }
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    fn fallback_models(&self) -> Vec<Model> {
        vec![Model {
            id: "default".into(),
            label: "Symphony default".into(),
            description: Some("Uses the model configured in Symphony".into()),
            reasoning_levels: vec![],
            options: vec![],
        }]
    }
    fn release_session(&self, session_id: &str) {
        self.sessions.release(session_id);
    }

    async fn commands_for(&self, cwd: &std::path::Path) -> Result<Vec<SlashCommand>, HarnessError> {
        let exe =
            Self::executable().ok_or_else(|| HarnessError::NotInstalled("symphony".into()))?;
        let mut command = Command::new(&exe);
        command.arg("stdio").arg("--workspace").arg(cwd);
        Self::configure(&mut command, &exe);
        crate::compose_child_path(&mut command, &exe);
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("Symphony command probe has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("Symphony command probe has no stdout".into()))?;
        let probe = async {
            let mut lines = BufReader::new(stdout).lines();
            let ready = lines.next_line().await?.ok_or_else(|| {
                HarnessError::Protocol("Symphony exited before the command handshake".into())
            })?;
            let ready: Value = serde_json::from_str(&ready)
                .map_err(|err| HarnessError::Protocol(format!("Symphony ready frame: {err}")))?;
            if field(&ready, "type") == "error" {
                return Err(HarnessError::Protocol(field(&ready, "message").into()));
            }
            check_version(&ready)?;
            if field(&ready, "type") != "ready" {
                return Err(HarnessError::Protocol("Symphony did not send ready".into()));
            }
            let request_id = uuid::Uuid::new_v4().to_string();
            let request =
                json!({"type":"command/list", "request_id":request_id}).to_string() + "\n";
            stdin.write_all(request.as_bytes()).await?;
            let frame = lines.next_line().await?.ok_or_else(|| {
                HarnessError::Protocol("Symphony exited before listing commands".into())
            })?;
            let frame: Value = serde_json::from_str(&frame)
                .map_err(|err| HarnessError::Protocol(format!("Symphony command frame: {err}")))?;
            if field(&frame, "type") != "commands" || field(&frame, "request_id") != request_id {
                return Err(HarnessError::Protocol(
                    "Symphony command response did not match".into(),
                ));
            }
            check_version(&frame)?;
            let mut commands: Vec<SlashCommand> = serde_json::from_value(frame["commands"].clone())
                .map_err(|err| HarnessError::Protocol(format!("Symphony commands: {err}")))?;
            // These actions already belong to Zeron's shell. Keep their
            // unqualified names for the native picker, new-chat and diff UI.
            prepare_commands(&mut commands);
            Ok(commands)
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), probe)
            .await
            .map_err(|_| HarnessError::Protocol("Symphony command discovery timed out".into()))?;
        let _ = child.start_kill();
        let _ = child.wait().await;
        result
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        for shared in self.sessions.idle_sessions() {
            if shared.stop.is_cancelled() {
                continue;
            }
            let Ok(mut bridge) = shared.bridge.try_lock() else {
                continue;
            };
            if !child_running(&mut bridge.child) {
                drop(bridge);
                self.sessions
                    .remove_if_same(&shared_session_id(&shared).await, &shared);
                continue;
            }
            match list_models(&mut bridge).await {
                Ok(models) => return Ok(models),
                Err(err) => {
                    if !child_running(&mut bridge.child) {
                        let session_id = bridge.session_id.clone();
                        reap(&mut bridge).await;
                        drop(bridge);
                        if !session_id.is_empty() {
                            self.sessions.remove_if_same(&session_id, &shared);
                        }
                    }
                    return Err(err);
                }
            }
        }
        self.catalog_models().await
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe =
            Self::executable().ok_or_else(|| HarnessError::NotInstalled("symphony".into()))?;
        let sessions = self.sessions.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            drive_session(sessions, exe, request, controls, tx).await;
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

async fn shared_session_id(shared: &Shared) -> String {
    shared.bridge.lock().await.session_id.clone()
}

impl SymphonyHarness {
    async fn catalog_models(&self) -> Result<Vec<Model>, HarnessError> {
        let mut slot = self.sessions.catalog.lock().await;
        let spawn_catalog = |slot: &mut Option<Bridge>| -> Result<(), HarnessError> {
            let exe =
                Self::executable().ok_or_else(|| HarnessError::NotInstalled("symphony".into()))?;
            let mut command = crate::process::Command::new(&exe);
            command.arg("stdio");
            Self::configure(&mut command, &exe);
            crate::compose_child_path(&mut command, &exe);
            *slot = Some(launch(&mut command, "", false, None)?);
            Ok(())
        };
        if slot
            .as_mut()
            .is_none_or(|bridge| !child_running(&mut bridge.child))
        {
            if let Some(bridge) = slot.as_mut() {
                reap(bridge).await;
            }
            *slot = None;
            spawn_catalog(&mut slot)?;
            if let Err(err) =
                accept_ready(slot.as_mut().expect("catalog just spawned"), None, false).await
            {
                if let Some(bridge) = slot.as_mut() {
                    reap(bridge).await;
                }
                *slot = None;
                return Err(err);
            }
        }
        let bridge = slot.as_mut().expect("catalog bridge");
        match list_models(bridge).await {
            Ok(models) => Ok(models),
            Err(err) => {
                reap(bridge).await;
                *slot = None;
                Err(err)
            }
        }
    }
}

fn prepare_commands(commands: &mut Vec<SlashCommand>) {
    // Native workspace commands own these actions.
    commands.retain(|item| !matches!(item.name.as_str(), "model" | "new" | "diff"));
    for command in commands {
        // Older versions advertise TUI dialogs that stdio cannot open.
        match command.name.as_str() {
            "provider" => {
                command.description =
                    "Show provider setup instructions; manage accounts in Zeron settings".into()
            }
            "langfuse" => command.description = "Show Langfuse telemetry setup instructions".into(),
            "clear" => command.description = "Show how to clear the transcript in Zeron".into(),
            "quit" => command.description = "Show how to exit Zeron".into(),
            "dashboard" => command.description = "List active Symphony agents".into(),
            _ => {}
        }
        command.options.retain(|option| {
            !option.value.is_empty() && !option.value.chars().any(char::is_control)
        });
    }
}

fn run_frame(request: &RunRequest) -> Value {
    let mut frame =
        json!({"type":"run", "prompt": request.prompt, "attachments": request.attachments});
    if let Some(effort) = request
        .model_options
        .get("symphonyReasoningEffort")
        .and_then(Value::as_str)
    {
        frame["reasoning_effort"] = json!(effort);
    } else if let Some(reasoning) = request.reasoning {
        frame["reasoning_effort"] = json!(reasoning);
    }
    frame
}

async fn drive_session(
    sessions: Arc<SessionTable>,
    exe: std::path::PathBuf,
    request: RunRequest,
    controls: RunControls,
    tx: mpsc::UnboundedSender<Result<AgentEvent, HarnessError>>,
) {
    let shared = match take_matching(&sessions, &request).await {
        Some(shared) => shared,
        None => {
            let model = launched_model(request.model.as_deref());
            let mut command = run_command(&exe, &request);
            match launch(&mut command, &request.cwd, request.auto_approve, model) {
                Ok(bridge) => Arc::new(Shared {
                    stop: crate::CancellationToken::new(),
                    bridge: Mutex::new(bridge),
                }),
                Err(err) => {
                    let _ = tx.send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(err.to_string()),
                        session_id: request.resume.clone(),
                    }));
                    return;
                }
            }
        }
    };
    let mut bridge = shared.bridge.lock().await;
    if bridge.session_id.is_empty() {
        if let Err(err) = accept_ready(&mut bridge, request.model.as_deref(), true).await {
            reap(&mut bridge).await;
            let _ = tx.send(Ok(AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some(err.to_string()),
                session_id: request.resume.clone(),
            }));
            return;
        }
        sessions.insert(bridge.session_id.clone(), shared.clone());
    }
    let mut ended = false;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut started = true;
    let mut error = None;
    let mut kill = false;
    let mut subagents = SubagentMapper::default();
    let session_id = bridge.session_id.clone();
    let model_name = bridge.model_name.clone();
    let _ = tx.send(Ok(AgentEvent::SessionStarted {
        harness: HarnessId::Symphony,
        model: model_name,
        tools: vec![],
        cwd: request.cwd.clone(),
        session_id: session_id.clone(),
        assistant_message_id: uuid::Uuid::new_v4().to_string(),
    }));
    let line = run_frame(&request).to_string() + "\n";
    if bridge.stdin.write_all(line.as_bytes()).await.is_err() {
        started = false;
        kill = true;
    }
    let mut deadline: Option<tokio::time::Instant> = None;
    while !kill && !ended {
        let stop = shared.stop.clone();
        let wake = tokio::select! {
            biased;
            _ = stop.cancelled() => DriveWake::Stop,
            _ = controls.interrupt.cancelled(), if !interrupt_sent => DriveWake::Interrupt,
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => DriveWake::Timeout,
            line = bridge.lines.next_line() => DriveWake::Line(line),
        };
        match wake {
            DriveWake::Stop => {
                kill = true;
                interrupted = true;
                break;
            }
            DriveWake::Interrupt => {
                interrupted = true;
                interrupt_sent = true;
                let _ = bridge.stdin.write_all(b"{\"type\":\"interrupt\"}\n").await;
                deadline = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
            }
            DriveWake::Timeout => {
                kill = true;
                interrupted = true;
                break;
            }
            DriveWake::Line(Err(_)) | DriveWake::Line(Ok(None)) => {
                kill = true;
                break;
            }
            DriveWake::Line(Ok(Some(line))) => {
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match field(&frame, "type") {
                    "event" => {
                        for mapped in subagents.map(field(&frame, "event"), &frame["payload"]) {
                            let _ = tx.send(Ok(mapped));
                        }
                    }
                    "input_requested" => {
                        let choices: Vec<String> = frame["choices"]
                            .as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let request_id = field(&frame, "request_id").to_owned();
                        let receiver = (controls.request_input)(vec![UserInputQuestion {
                            id: request_id.clone(),
                            header: field(&frame, "kind").into(),
                            question: field(&frame, "question").into(),
                            options: choices,
                            multi_select: false,
                        }]);
                        let stop = shared.stop.clone();
                        let answered = tokio::select! {
                            biased;
                            _ = stop.cancelled() => InputWake::Stop,
                            _ = controls.interrupt.cancelled() => InputWake::Interrupt,
                            result = receiver => InputWake::Answer(result.ok().and_then(|answers| {
                                answers.first().and_then(|answer| answer.labels.first().cloned())
                            })),
                        };
                        match answered {
                            InputWake::Stop => {
                                kill = true;
                                interrupted = true;
                                break;
                            }
                            InputWake::Interrupt => {
                                interrupted = true;
                                interrupt_sent = true;
                                let _ = bridge.stdin.write_all(b"{\"type\":\"interrupt\"}\n").await;
                                deadline = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                            }
                            InputWake::Answer(answer) => {
                                let line = json!({"type":"answer", "request_id": request_id, "value": answer.unwrap_or_else(|| "Deny".into())}).to_string() + "\n";
                                if bridge.stdin.write_all(line.as_bytes()).await.is_err() {
                                    kill = true;
                                    break;
                                }
                            }
                        }
                    }
                    "error" => {
                        error = Some(field(&frame, "message").to_owned());
                    }
                    "done" => {
                        ended = true;
                        let status = match field(&frame, "status") {
                            "completed" => DoneStatus::Completed,
                            "interrupted" => DoneStatus::Interrupted,
                            _ => DoneStatus::Errored,
                        };
                        // A clean interrupted done is the checkpoint. Do not kill.
                        let _ = tx.send(Ok(AgentEvent::Done {
                            status,
                            result: None,
                            error: error.clone(),
                            session_id: Some(session_id.clone()),
                        }));
                    }
                    _ => {}
                }
            }
        }
    }
    if !ended {
        let _ = tx.send(Ok(AgentEvent::Done {
            status: if interrupted {
                DoneStatus::Interrupted
            } else {
                DoneStatus::Errored
            },
            result: None,
            error: if interrupted {
                None
            } else {
                error.or_else(|| {
                    Some(
                        if started {
                            "Symphony exited before completing the turn"
                        } else {
                            "Symphony failed to start"
                        }
                        .into(),
                    )
                })
            },
            session_id: Some(session_id.clone()),
        }));
    }
    let released = shared.stop.is_cancelled();
    let dead = !child_running(&mut bridge.child);
    if kill || released || dead {
        reap(&mut bridge).await;
        drop(bridge);
        sessions.remove_if_same(&session_id, &shared);
    }
}

enum DriveWake {
    Stop,
    Interrupt,
    Timeout,
    Line(std::io::Result<Option<String>>),
}

enum InputWake {
    Stop,
    Interrupt,
    Answer(Option<String>),
}

fn check_version(frame: &Value) -> Result<(), HarnessError> {
    let version = frame["protocol_version"].as_u64();
    if version == Some(PROTOCOL_VERSION) {
        Ok(())
    } else {
        Err(HarnessError::Protocol(format!(
            "Symphony protocol version {version:?}; expected {PROTOCOL_VERSION}. Update the Symphony installation"
        )))
    }
}

fn check_ready_model(requested: Option<&str>, actual: &str) -> Result<(), HarnessError> {
    if actual.is_empty() {
        return Err(HarnessError::Protocol(
            "Symphony did not report its model".into(),
        ));
    }
    if let Some(requested) = requested.filter(|model| *model != "default")
        && requested != actual
    {
        return Err(HarnessError::Protocol(format!(
            "Symphony selected {actual} instead of requested model {requested}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_choices_are_preserved_and_native_actions_are_not_advertised() {
        let mut commands: Vec<SlashCommand> = serde_json::from_value(json!([
            {"name":"personality", "options":[{"value":"precise", "label":"Precise"}, {"value":"bad\nvalue"}]},
            {"name":"model"}, {"name":"new"}, {"name":"diff"}, {"name":"provider"}
        ])).unwrap();
        prepare_commands(&mut commands);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].options.len(), 1);
        assert_eq!(commands[0].options[0].value, "precise");
        assert!(commands[1].description.contains("settings"));
        let legacy: SlashCommand = serde_json::from_value(json!({"name":"status"})).unwrap();
        assert!(legacy.options.is_empty());
    }

    #[test]
    fn write_file_and_patch_use_symphony_schema_names() {
        assert_eq!(
            map_event(
                "tool_execution_started",
                &json!({
                    "tool_call_id": "w1",
                    "tool_name": "write_file",
                    "arguments": {"path": "a.txt", "content": "hello\n"}
                })
            ),
            Some(AgentEvent::ToolCall {
                id: "w1".into(),
                call: ToolCall::WriteFile {
                    path: "a.txt".into(),
                    content: Some("hello\n".into()),
                },
            }),
        );
        assert_eq!(
            map_event(
                "tool_execution_started",
                &json!({
                    "tool_call_id": "p1",
                    "tool_name": "patch",
                    "arguments": {
                        "path": "a.txt",
                        "old_str": "hello",
                        "new_str": "world",
                        "replace_all": false,
                        "old_string": "ignored",
                        "new_string": "ignored"
                    }
                })
            ),
            Some(AgentEvent::ToolCall {
                id: "p1".into(),
                call: ToolCall::EditFile {
                    path: "a.txt".into(),
                    old_string: Some("hello".into()),
                    new_string: Some("world".into()),
                },
            }),
        );
        assert_eq!(
            map_event(
                "tool_execution_completed",
                &json!({"tool_call_id": "w1", "status": "success", "result": "wrote a.txt"})
            ),
            Some(AgentEvent::ToolResult {
                id: "w1".into(),
                is_error: false,
                output: Some("wrote a.txt".into()),
                diff: None,
            }),
        );
    }

    #[test]
    fn maps_streamed_text_and_tools() {
        assert_eq!(
            map_event("text_delta", &json!({"delta":"hello"})),
            Some(AgentEvent::TextDelta {
                text: "hello".into()
            }),
        );
        assert_eq!(
            map_event(
                "tool_execution_started",
                &json!({
                    "tool_call_id":"t1", "tool_name":"bash", "arguments":{"command":"pwd"}
                })
            ),
            Some(AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "pwd".into()
                },
            }),
        );
    }

    #[test]
    fn completed_patch_exposes_the_tui_replacement_hunk() {
        let mut mapper = SubagentMapper::default();
        mapper.map("tool_execution_started", &json!({
            "tool_call_id":"edit-1", "tool_name":"patch",
            "arguments":{"path":"src/main.py", "old_str":"print('old')\n", "new_str":"print('new')\n"}
        }));
        let result = mapper.map(
            "tool_execution_completed",
            &json!({
                "tool_call_id":"edit-1", "tool_name":"patch", "status":"success",
                "result":"patched src/main.py (1 replacement(s), +0 bytes)"
            }),
        );
        assert!(
            matches!(&result[..], [AgentEvent::ToolResult { output: Some(output), diff: Some(diff), .. }]
            if output.starts_with("patched src/main.py")
                && diff.path == "src/main.py"
                && diff.old_text.as_deref() == Some("print('old')\n")
                && diff.new_text == "print('new')\n")
        );
        assert!(mapper.patches.is_empty());

        mapper.map(
            "tool_execution_started",
            &json!({
                "tool_call_id":"edit-2", "tool_name":"patch",
                "arguments":{"path":"src/main.py", "old_str":"missing", "new_str":"new"}
            }),
        );
        let failed = mapper.map(
            "tool_execution_completed",
            &json!({
                "tool_call_id":"edit-2", "tool_name":"patch", "status":"success",
                "result":"old_str not found in src/main.py"
            }),
        );
        assert!(matches!(
            &failed[..],
            [AgentEvent::ToolResult { diff: None, .. }]
        ));
    }

    #[test]
    fn requires_matching_protocol_version() {
        assert!(check_version(&json!({"protocol_version": 2})).is_ok());
        assert!(check_version(&json!({"protocol_version": 1})).is_err());
        assert!(check_version(&json!({})).is_err());
    }

    #[test]
    fn requested_model_must_match_the_agent_ready_frame() {
        assert!(check_ready_model(Some("grok:grok-4.7"), "grok:grok-4.7").is_ok());
        assert!(check_ready_model(Some("default"), "openai:gpt-5.6-luna").is_ok());
        assert!(check_ready_model(Some("grok:grok-4.7"), "openai:gpt-5.6-luna").is_err());
        assert!(check_ready_model(None, "").is_err());
    }

    #[test]
    fn model_response_must_match_the_request() {
        let frame = json!({
            "type":"models", "protocol_version":2, "request_id":"probe-1",
            "models":[{"id":"openai:gpt-test", "label":"gpt-test",
                       "description":"openai", "reasoning_levels":[]}]
        });
        assert_eq!(
            catalog_models(&frame, "probe-1").unwrap()[0].id,
            "openai:gpt-test"
        );
        assert!(catalog_models(&frame, "probe-2").is_err());
    }

    #[test]
    fn routes_child_activity_to_the_spawn_chip() {
        let mut mapper = SubagentMapper::default();
        let call = mapper.map(
            "tool_execution_started",
            &json!({
                "agent_id":"parent", "tool_call_id":"spawn-1", "tool_name":"spawn_agent",
                "arguments":{"prompt":"Inspect tests", "label":"Tests", "model_id":"provider:small"}
            }),
        );
        assert!(matches!(&call[..], [AgentEvent::ToolCall { id, call }]
            if id == "spawn-1" && call.is_subagent_spawn()
                && call.subagent_model() == Some("provider:small")));

        let opening = mapper.map(
            "agent_spawned",
            &json!({
                "agent_id":"parent", "child_id":"child-1", "tool_call_id":"spawn-1",
                "prompt":"Inspect tests"
            }),
        );
        assert!(
            matches!(&opening[..], [AgentEvent::Subagent { parent_tool_use_id, event }]
            if parent_tool_use_id == "spawn-1"
                && matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "Inspect tests"))
        );

        let child = mapper.map(
            "text_delta",
            &json!({"agent_id":"child-1", "delta":"Found tests"}),
        );
        assert!(
            matches!(&child[..], [AgentEvent::Subagent { parent_tool_use_id, event }]
            if parent_tool_use_id == "spawn-1"
                && matches!(event.as_ref(), AgentEvent::TextDelta { text } if text == "Found tests"))
        );
        let child_tool = mapper.map(
            "tool_execution_started",
            &json!({
                "agent_id":"child-1", "parent_id":"parent", "tool_call_id":"read-1",
                "tool_name":"read_file", "arguments":{"path":"tests/test_agent.py"}
            }),
        );
        assert!(
            matches!(&child_tool[..], [AgentEvent::Subagent { parent_tool_use_id, event }]
            if parent_tool_use_id == "spawn-1"
                && matches!(event.as_ref(), AgentEvent::ToolCall { id, .. } if id == "read-1"))
        );
        assert_eq!(
            mapper.map(
                "text_delta",
                &json!({"agent_id":"parent", "delta":"Parent"})
            ),
            vec![AgentEvent::TextDelta {
                text: "Parent".into()
            }]
        );

        let done = mapper.map("agent_completed", &json!({"child_id":"child-1"}));
        assert!(
            matches!(&done[..], [AgentEvent::Subagent { parent_tool_use_id, event }]
            if parent_tool_use_id == "spawn-1"
                && matches!(event.as_ref(), AgentEvent::Done { status: DoneStatus::Completed, .. }))
        );
        assert!(mapper.paths.is_empty());
        assert!(
            mapper
                .map(
                    "text_delta",
                    &json!({
                        "agent_id":"child-1", "parent_id":"parent", "delta":"late"
                    })
                )
                .is_empty()
        );
    }

    #[test]
    fn child_failure_and_unmatched_spawn_do_not_leak_to_parent() {
        let mut mapper = SubagentMapper::default();
        assert!(
            mapper
                .map(
                    "agent_spawned",
                    &json!({
                        "child_id":"orphan", "tool_call_id":"", "prompt":"task"
                    })
                )
                .is_empty()
        );
        assert!(
            mapper
                .map("agent_failed", &json!({"child_id":"orphan"}))
                .is_empty()
        );
        assert!(
            mapper
                .map(
                    "text_delta",
                    &json!({
                        "agent_id":"orphan", "parent_id":"parent", "delta":"unknown child"
                    })
                )
                .is_empty()
        );
        mapper.map(
            "agent_spawned",
            &json!({
                "child_id":"child", "tool_call_id":"spawn", "prompt":"task"
            }),
        );
        let failed = mapper.map(
            "agent_failed",
            &json!({
                "child_id":"child", "error_type":"HarnessCancelled", "message":"Child cancelled"
            }),
        );
        assert!(matches!(&failed[..], [AgentEvent::Subagent { event, .. }]
            if matches!(event.as_ref(), AgentEvent::Done {
                status: DoneStatus::Interrupted, error: Some(message), ..
            } if message == "Child cancelled")));
    }
}
