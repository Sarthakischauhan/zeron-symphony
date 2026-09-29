//! Native Symphony driver. One stdio process handles one persisted turn.

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode, ToolCall,
    UserInputQuestion,
};

use crate::{
    Harness, HarnessError, RunControls,
    process::{Command, Stdio},
};

pub struct SymphonyHarness;

const PROTOCOL_VERSION: u64 = 1;

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
        _ => ToolCall::Unknown {
            name: name.into(),
            input: Some(args.clone()),
        },
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
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let exe =
            Self::executable().ok_or_else(|| HarnessError::NotInstalled("symphony".into()))?;
        let mut command = Command::new(&exe);
        command.args(["stdio", "--models"]);
        crate::compose_child_path(&mut command, &exe);
        let output = command.output().await?;
        let frame: Value = serde_json::from_slice(&output.stdout)
            .map_err(|err| HarnessError::Protocol(format!("Symphony model catalog: {err}")))?;
        if !output.status.success() {
            return Err(HarnessError::Protocol(field(&frame, "message").into()));
        }
        check_version(&frame)?;
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
                    .filter_map(|level| {
                        serde_json::from_value::<ReasoningLevel>(level.clone()).ok()
                    })
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
                        if let Some(mapped) = map_event(field(&frame, "event"), &frame["payload"]) {
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
        assert!(check_version(&json!({"protocol_version": 1})).is_ok());
        assert!(check_version(&json!({"protocol_version": 2})).is_err());
        assert!(check_version(&json!({})).is_err());
    }
}
