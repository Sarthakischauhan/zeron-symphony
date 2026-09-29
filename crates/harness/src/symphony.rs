//! Native Symphony driver. One stdio process handles one persisted turn.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, ToolCall,
    UserInputQuestion,
};

use crate::{
    Harness, HarnessError, RunControls,
    process::{Command, Stdio},
};

pub struct SymphonyHarness;

const PROTOCOL_VERSION: u64 = 2;

impl SymphonyHarness {
    pub fn new() -> Self {
        Self
    }

    fn executable() -> Option<std::path::PathBuf> {
        if let Some(path) = std::env::var_os("SYMPHONY_EXECUTABLE") {
            return crate::executable::validate_native_override(std::path::Path::new(&path)).ok();
        }
        crate::executable::find_on_paths("symphony", Vec::new())
    }

    /// Return the checkout that owns the Symphony executable, when it has a
    /// project-local `.symphony/config.json`. Symphony resolves its config
    /// from `Path.home()`, so the child needs the checkout as its home rather
    /// than Zeron's workspace (which is usually a different repository).
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

    fn configure(command: &mut Command, executable: &std::path::Path) {
        if let Some(root) = Self::root(executable) {
            command.env("HOME", &root).env("USERPROFILE", &root);
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
        "write_file" => ToolCall::WriteFile {
            path: field(args, "path").into(),
            content: None,
        },
        "patch" => ToolCall::EditFile {
            path: field(args, "path").into(),
            old_string: args
                .get("old_str")
                .or_else(|| args.get("old_string"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            new_string: args
                .get("new_str")
                .or_else(|| args.get("new_string"))
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
        let Some(mapped) = map_event(event, payload) else {
            return Vec::new();
        };
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
            let request = json!({"type":"command/list", "request_id":request_id}).to_string() + "\n";
            stdin.write_all(request.as_bytes()).await?;
            let frame = lines.next_line().await?.ok_or_else(|| {
                HarnessError::Protocol("Symphony exited before listing commands".into())
            })?;
            let frame: Value = serde_json::from_str(&frame)
                .map_err(|err| HarnessError::Protocol(format!("Symphony command frame: {err}")))?;
            if field(&frame, "type") != "commands" || field(&frame, "request_id") != request_id {
                return Err(HarnessError::Protocol("Symphony command response did not match".into()));
            }
            let mut commands: Vec<SlashCommand> = serde_json::from_value(frame["commands"].clone())
                .map_err(|err| HarnessError::Protocol(format!("Symphony commands: {err}")))?;
            // These actions already belong to Zeron's shell. Keep their
            // unqualified names for the native picker, new-chat and diff UI.
            commands.retain(|item| !matches!(item.name.as_str(), "model" | "new" | "diff"));
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
        let exe =
            Self::executable().ok_or_else(|| HarnessError::NotInstalled("symphony".into()))?;
        let mut command = Command::new(&exe);
        command.arg("stdio");
        Self::configure(&mut command, &exe);
        crate::compose_child_path(&mut command, &exe);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("Symphony model probe has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("Symphony model probe has no stdout".into()))?;
        let probe = async {
            let mut lines = BufReader::new(stdout).lines();
            let ready = lines.next_line().await?.ok_or_else(|| {
                HarnessError::Protocol("Symphony exited before the model handshake".into())
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
            let request = json!({"type":"model/list", "request_id":request_id}).to_string() + "\n";
            stdin.write_all(request.as_bytes()).await?;
            let response = lines.next_line().await?.ok_or_else(|| {
                HarnessError::Protocol("Symphony exited before listing models".into())
            })?;
            let response: Value = serde_json::from_str(&response)
                .map_err(|err| HarnessError::Protocol(format!("Symphony model frame: {err}")))?;
            if field(&response, "type") == "error" {
                return Err(HarnessError::Protocol(field(&response, "message").into()));
            }
            catalog_models(&response, &request_id)
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), probe)
            .await
            .map_err(|_| HarnessError::Protocol("Symphony model discovery timed out".into()))?;
        let _ = child.start_kill();
        let _ = child.wait().await;
        result
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe =
            Self::executable().ok_or_else(|| HarnessError::NotInstalled("symphony".into()))?;
        let mut command = Command::new(&exe);
        command.arg("stdio").arg("--workspace").arg(&request.cwd);
        if let Some(resume) = &request.resume {
            command.arg("--session-id").arg(resume);
        }
        if let Some(model) = &request.model {
            if model != "default" {
                command.arg("--model").arg(model);
            }
        }
        if request.auto_approve {
            command.arg("--unattended");
        }
        Self::configure(&mut command, &exe);
        command
            .current_dir(&request.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::compose_child_path(&mut command, &exe);
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("no Symphony stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("no Symphony stdout".into()))?;
        // Drain diagnostics independently so a verbose provider cannot block its stdout.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("Symphony: {line}");
                }
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut session_id = request.resume.clone();
            let mut ended = false;
            let mut interrupted = false;
            let mut started = false;
            let mut error = None;
            let mut subagents = SubagentMapper::default();
            // Symphony emits ready before accepting a prompt.
            loop {
                let next = tokio::select! {
                    _ = controls.interrupt.cancelled() => {
                        interrupted = true;
                        let _ = stdin.write_all(b"{\"type\":\"interrupt\"}\n").await;
                        break;
                    }
                    line = lines.next_line() => line,
                };
                let Ok(Some(line)) = next else { break };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match field(&frame, "type") {
                    "ready" => {
                        if let Err(err) = check_version(&frame) {
                            error = Some(err.to_string());
                            break;
                        }
                        if let Err(err) =
                            check_ready_model(request.model.as_deref(), field(&frame, "model"))
                        {
                            error = Some(err.to_string());
                            break;
                        }
                        session_id = Some(field(&frame, "session_id").into());
                        let _ = tx.send(Ok(AgentEvent::SessionStarted {
                            harness: HarnessId::Symphony,
                            model: field(&frame, "model").into(),
                            tools: vec![],
                            cwd: request.cwd.clone(),
                            session_id: session_id.clone().unwrap_or_default(),
                            assistant_message_id: uuid::Uuid::new_v4().to_string(),
                        }));
                        let line =
                            json!({"type":"run", "prompt":request.prompt, "attachments":request.attachments}).to_string() + "\n";
                        if stdin.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                        started = true;
                    }
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
                        let answer = tokio::select! {
                            result = receiver => result.ok().and_then(|answers| answers.first().and_then(|a| a.labels.first().cloned())),
                            _ = controls.interrupt.cancelled() => { interrupted = true; None },
                        };
                        if interrupted {
                            break;
                        }
                        // Closing the prompt is a denial, never an implicit approval.
                        let line = json!({"type":"answer", "request_id":request_id, "value":answer.unwrap_or_else(|| "Deny".into())}).to_string() + "\n";
                        if stdin.write_all(line.as_bytes()).await.is_err() {
                            break;
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
                        let _ = tx.send(Ok(AgentEvent::Done {
                            status,
                            result: None,
                            error: error.clone(),
                            session_id: session_id.clone(),
                        }));
                        break;
                    }
                    _ => {}
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
                    session_id,
                }));
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
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
