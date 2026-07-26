use super::{Tool, ToolContext, ToolOutput};
use crate::protocol::{
    HistoryMessage, Request, ServerEvent, default_comm_await_target_statuses,
    latest_assistant_comm_report,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::Duration;

const REQUEST_ID: u64 = 1;

pub struct DelegateTool;

impl DelegateTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Clone, Deserialize)]
struct DelegateInput {
    /// The task to delegate to the more capable model.
    task: String,
    /// Optional context to provide to the delegate model (e.g. file contents,
    /// intermediate results, reasoning).
    #[serde(default)]
    context: Option<String>,
    /// Optional model override (e.g. "claude-opus-4-8", "gpt-5.5").
    /// Overrides the configured delegate_model for this delegation.
    #[serde(default)]
    model: Option<String>,
}

impl DelegateTool {
    /// Send a request to the server via the Unix socket and wait for the response.
    async fn send_request(request: Request) -> Result<ServerEvent> {
        let path = crate::server::socket_path();
        let stream = crate::server::connect_socket(&path).await?;
        let (reader, mut writer) = stream.into_split();

        let request_id = request.id();
        let timeout = Duration::from_secs(30);

        let json = serde_json::to_string(&request)? + "\n";
        writer.write_all(json.as_bytes()).await?;

        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            let n = tokio::time::timeout(timeout, reader.read_line(&mut line)).await??;
            if n == 0 {
                return Err(anyhow::anyhow!(
                    "Connection closed before receiving response"
                ));
            }

            let value: Value = serde_json::from_str(line.trim())?;
            let event_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let event_id = value.get("id").and_then(|v| v.as_u64());

            // Skip acks, skip broadcast events that don't match our request ID
            if event_type != "ack" && event_id != Some(request_id) {
                continue;
            }

            match event_type {
                "ack" => continue,
                // Skip broadcast/async events not tied to our request
                "swarm_status"
                | "swarm_plan"
                | "swarm_plan_proposal"
                | "swarm_event"
                | "notification"
                | "soft_interrupt_injected"
                | "session"
                | "session_id"
                | "history"
                | "mcp_status"
                | "memory_injected"
                | "compaction"
                | "connection_type"
                | "connection_phase"
                | "status_detail"
                | "upstream_provider"
                | "reloading"
                | "reload_progress"
                | "available_models_updated"
                | "side_panel_state"
                | "transcript"
                | "interrupted" => continue,
                "error" => {
                    let msg = value
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error");
                    return Err(anyhow::anyhow!("{}", msg));
                }
                _ => return Ok(serde_json::from_value(value)?),
            }
        }
    }

    /// Extract the last assistant message content from a conversation history.
    fn extract_last_assistant_message(messages: &[HistoryMessage]) -> Option<String> {
        latest_assistant_comm_report(messages)
    }

    /// Build the initial message for the spawned delegate agent.
    fn build_initial_message(task: &str, context: Option<&str>) -> String {
        match context {
            Some(ctx) if !ctx.trim().is_empty() => {
                format!(
                    "You are a delegate agent. Complete the task below and report your results.\n\nTask: {}\n\nAdditional context:\n{}",
                    task, ctx
                )
            }
            _ => {
                format!(
                    "You are a delegate agent. Complete the task below and report your results.\n\nTask: {}",
                    task
                )
            }
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &'static str {
        "delegate"
    }

    fn description(&self) -> &'static str {
        "Delegate a difficult sub-task to a more capable model. Use this when you determine a task is too complex, requires deep reasoning, or is outside your capabilities. The delegate model will process the task independently and return its result. Returns the full response from the delegate model."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task to delegate to the more capable model. Be specific about what you need."
                },
                "context": {
                    "type": "string",
                    "description": "Optional additional context for the delegate model (file contents, code, reasoning, etc.)"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override. Use when you need a specific model for this delegation."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: DelegateInput = serde_json::from_value(input)?;

        // Determine the delegate model
        let delegate_cfg = &crate::config::config().delegate;
        let delegate_model = params
            .model
            .or_else(|| delegate_cfg.delegate_model.clone());

        let delegate_model_str = delegate_model
            .as_deref()
            .unwrap_or("default")
            .to_string();

        let initial_message = Self::build_initial_message(&params.task, params.context.as_deref());

        // Step 1: Spawn a sub-agent with the delegate model
        let spawn_request = Request::CommSpawn {
            id: REQUEST_ID,
            session_id: ctx.session_id.clone(),
            working_dir: None,
            initial_message: Some(initial_message),
            request_nonce: None,
            spawn_mode: Some("headless".to_string()),
            model: delegate_model,
            effort: None,
            label: Some("delegate".to_string()),
        };

        let spawned_session_id = match Self::send_request(spawn_request).await? {
            ServerEvent::CommSpawnResponse { new_session_id, .. }
                if !new_session_id.is_empty() =>
            {
                new_session_id
            }
            ServerEvent::CommSpawnResponse { .. } => {
                return Err(anyhow::anyhow!("Spawn returned empty session ID."));
            }
            _ => {
                return Err(anyhow::anyhow!("Failed to spawn delegate agent."));
            }
        };

        // Step 2: Wait for the spawned agent to complete
        let timeout_minutes = delegate_cfg.timeout_minutes;
        let timeout_secs = timeout_minutes as u64 * 60;
        let _socket_timeout = Duration::from_secs(timeout_secs + 30);

        let await_request = Request::CommAwaitMembers {
            id: REQUEST_ID,
            session_id: ctx.session_id.clone(),
            target_status: default_comm_await_target_statuses(),
            session_ids: vec![spawned_session_id.clone()],
            mode: None,
            timeout_secs: Some(timeout_secs),
            background: false,
            notify: false,
            wake: false,
        };

        let (completed, _members) = match Self::send_request(await_request).await {
            Ok(ServerEvent::CommAwaitMembersResponse {
                completed,
                members,
                ..
            }) => (completed, members),
            Ok(response) => {
                let msg = if let ServerEvent::Error { message, .. } = &response {
                    message.clone()
                } else {
                    "Unknown response".to_string()
                };
                return Err(anyhow::anyhow!(
                    "Delegate task failed: {}. Session: {}",
                    msg,
                    spawned_session_id
                ));
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Delegate task timed out or failed: {}. Session: {}",
                    e,
                    spawned_session_id
                ));
            }
        };

        if !completed {
            // Clean up the spawned session if it timed out
            let _ = Self::send_request(Request::CommStop {
                id: REQUEST_ID,
                session_id: ctx.session_id.clone(),
                target_session: spawned_session_id.clone(),
                force: Some(true),
            })
            .await;
            return Err(anyhow::anyhow!(
                "Delegate task did not complete within {} minutes. Session: {}",
                timeout_minutes,
                spawned_session_id
            ));
        }

        // Step 3: Read the delegate agent's response
        let read_request = Request::CommReadContext {
            id: REQUEST_ID,
            session_id: ctx.session_id.clone(),
            target_session: spawned_session_id.clone(),
        };

        let delegate_response = match Self::send_request(read_request).await {
            Ok(ServerEvent::CommContextHistory { messages, .. }) => {
                Self::extract_last_assistant_message(&messages)
                    .unwrap_or_else(|| "<no response from delegate>".to_string())
            }
            Ok(response) => {
                let msg = if let ServerEvent::Error { message, .. } = &response {
                    message.clone()
                } else {
                    "Unknown response".to_string()
                };
                format!("<delegate agent completed but response unavailable: {}>", msg)
            }
            Err(e) => {
                format!("<delegate agent completed but response unavailable: {}>", e)
            }
        };

        // Step 4: Clean up the spawned session
        let _ = Self::send_request(Request::CommStop {
            id: REQUEST_ID,
            session_id: ctx.session_id.clone(),
            target_session: spawned_session_id.clone(),
            force: Some(true),
        })
        .await;

        Ok(ToolOutput::new(format!(
            "## Delegation Result\n\n**Delegate model:** {}\n\n**Task:** {}\n\n**Response:**\n\n{}",
            delegate_model_str, params.task, delegate_response
        )))
    }
}