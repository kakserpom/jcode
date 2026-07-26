use super::{Tool, ToolContext, ToolOutput};
use crate::protocol::{
    Request, ServerEvent, default_comm_await_target_statuses,
};
use crate::session_delegate_config::{
    effective_delegate_model, effective_delegate_timeout, effective_enabled,
    validate_model_allowed, allowed_models,
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
        "Delegate a difficult sub-task to a more capable model. Use this when you determine a task is too complex, requires deep reasoning, or is outside your capabilities. The delegate model will process the task independently and return its result. Returns the full response from the delegate model. You can specify which model to delegate to via the `model` parameter. Use `configure_delegate` to see which models are available."
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
                    "description": "Optional model override. Use when you need a specific model for this delegation. See allowed models via `configure_delegate`."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: DelegateInput = serde_json::from_value(input)?;

        // Check if delegation is enabled for this session
        if !effective_enabled(&ctx.session_id) {
            return Ok(ToolOutput::new("Delegation is disabled for this session. Use `configure_delegate(enabled=true)` to enable it.".to_string()));
        }

        // Determine the delegate model: explicit param > session config > file config
        let delegate_model = params
            .model
            .or_else(|| effective_delegate_model(&ctx.session_id));

        let delegate_model_str = delegate_model
            .as_deref()
            .unwrap_or("default")
            .to_string();

        // Validate the model is in the allowed list (if the list is non-empty)
        if let Some(ref model) = delegate_model {
            if let Err(msg) = validate_model_allowed(&ctx.session_id, model) {
                let allowed = allowed_models(&ctx.session_id);
                return Ok(ToolOutput::new(format!(
                    "Cannot delegate: {}\n\nAvailable models: {}\nUse `configure_delegate` to see available models or change the delegate model.",
                    msg,
                    if allowed.is_empty() { "(no restriction — any model)".to_string() } else { allowed.join(", ") }
                )));
            }
        }

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
        let timeout_minutes = effective_delegate_timeout(&ctx.session_id);
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

        let delegate_response = match Self::send_request(await_request).await {
            Ok(ServerEvent::CommAwaitMembersResponse {
                completed: true,
                members,
                ..
            }) => {
                // Extract the completion report from the member status
                let report = members
                    .first()
                    .and_then(|m| m.completion_report.clone())
                    .unwrap_or_else(|| "<no response from delegate>".to_string());
                report
            }
            Ok(ServerEvent::CommAwaitMembersResponse {
                completed: false,
                ..
            }) => {
                // Clean up the spawned session if it timed out
                let _ = Self::send_request(Request::CommStop {
                    id: REQUEST_ID,
                    session_id: ctx.session_id.clone(),
                    target_session: spawned_session_id.clone(),
                    force: Some(true),
                })
                .await;
                format!(
                    "<delegate task did not complete within {} minutes>",
                    timeout_minutes
                )
            }
            Ok(response) => {
                let msg = if let ServerEvent::Error { message, .. } = &response {
                    message.clone()
                } else {
                    "Unknown response".to_string()
                };
                format!("<delegate task failed: {}. Session: {}>", msg, spawned_session_id)
            }
            Err(e) => {
                format!(
                    "<delegate task failed: {}. Session: {}>",
                    e, spawned_session_id
                )
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

/// Tool to configure the delegate settings at runtime.
/// Allows the model to set/change the delegate model, provider, or timeout
/// without editing the config file.
pub struct ConfigureDelegateTool;

impl ConfigureDelegateTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Clone, Deserialize)]
struct ConfigureDelegateInput {
    /// Model to use for delegation (e.g. "claude-opus-4-8", "gpt-5.5").
    /// Pass empty string to clear and fall back to config file value.
    #[serde(default)]
    delegate_model: Option<String>,
    /// Provider to use for the delegate model (e.g. "openai", "claude").
    /// Pass empty string to clear.
    #[serde(default)]
    delegate_provider: Option<String>,
    /// Timeout in minutes for delegated tasks.
    /// Pass 0 to clear and fall back to config file value.
    #[serde(default)]
    timeout_minutes: Option<u32>,
    /// Whether delegation is enabled. Pass empty string, "true", or "false".
    /// When not provided, the current value is unchanged.
    #[serde(default)]
    enabled: Option<String>,
    /// List of allowed models for delegation (comma-separated).
    /// Pass empty string to clear and fall back to config file.
    /// Example: "claude-opus-4-8,gpt-5.5"
    #[serde(default)]
    allowed_models: Option<String>,
}

/// Normalize: treat empty string as None (clear/fallback).
fn normalize_opt_string(v: Option<String>) -> Option<Option<String>> {
    match v {
        Some(s) if s.trim().is_empty() => Some(None),
        Some(s) => Some(Some(s.trim().to_string())),
        None => None,
    }
}

/// Normalize: treat 0 as None (clear/fallback).
fn normalize_opt_u32(v: Option<u32>) -> Option<Option<u32>> {
    match v {
        Some(0) => Some(None),
        Some(n) => Some(Some(n)),
        None => None,
    }
}

#[async_trait]
impl Tool for ConfigureDelegateTool {
    fn name(&self) -> &'static str {
        "configure_delegate"
    }

    fn description(&self) -> &'static str {
        "Configure the delegate model settings at runtime. Use this to set which model handles delegated tasks, change the provider, adjust the timeout, enable/disable delegation, or restrict which models can be used. Settings persist for the current session and override the config file. Pass empty string or 0 to clear a setting and fall back to the config file."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "delegate_model": {
                    "type": "string",
                    "description": "Model to use for delegation (e.g. \"claude-opus-4-8\", \"gpt-5.5\"). Pass empty string to clear and fall back to config file."
                },
                "delegate_provider": {
                    "type": "string",
                    "description": "Provider for the delegate model (e.g. \"openai\", \"claude\"). Pass empty string to clear."
                },
                "timeout_minutes": {
                    "type": "integer",
                    "description": "Timeout in minutes for delegated tasks. Pass 0 to clear and fall back to config file (default: 30)."
                },
                "enabled": {
                    "type": "string",
                    "description": "Enable or disable delegation. Pass \"true\" to enable, \"false\" to disable, or empty string to fall back to config file."
                },
                "allowed_models": {
                    "type": "string",
                    "description": "Comma-separated list of allowed models for delegation. Pass empty string to clear and fall back to config file. Example: \"claude-opus-4-8,gpt-5.5\""
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: ConfigureDelegateInput = serde_json::from_value(input)?;

        let model = normalize_opt_string(params.delegate_model);
        let provider = normalize_opt_string(params.delegate_provider);
        let timeout = normalize_opt_u32(params.timeout_minutes);

        // Parse enabled: "true" -> Some(Some(true)), "false" -> Some(Some(false)), "" -> Some(None), None -> None
        let enabled = match params.enabled {
            Some(ref s) if s.trim().eq_ignore_ascii_case("true") => Some(Some(true)),
            Some(ref s) if s.trim().eq_ignore_ascii_case("false") => Some(Some(false)),
            Some(ref s) if s.trim().is_empty() => Some(None),
            Some(_) => None, // invalid value, skip
            None => None,
        };

        // Parse allowed_models: comma-separated string -> Vec<String>
        let allowed_models_val = match params.allowed_models {
            Some(ref s) if s.trim().is_empty() => Some(None), // clear
            Some(ref s) => {
                let models: Vec<String> = s
                    .split(',')
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect();
                Some(Some(models))
            }
            None => None,
        };

        // Check if we should show current settings (no fields provided)
        let has_updates = model.is_some()
            || provider.is_some()
            || timeout.is_some()
            || enabled.is_some()
            || allowed_models_val.is_some();

        if !has_updates {
            // Show current settings
            let file_cfg = &crate::config::config().delegate;
            let session_cfg = crate::session_delegate_config::session_delegate_config(&ctx.session_id);
            let effective_model = crate::session_delegate_config::effective_delegate_model(&ctx.session_id);
            let effective_timeout = crate::session_delegate_config::effective_delegate_timeout(&ctx.session_id);
            let eff_enabled = effective_enabled(&ctx.session_id);
            let eff_allowed = allowed_models(&ctx.session_id);

            let mut out = String::from("## Current Delegate Configuration\n\n");
            out.push_str(&format!("**Enabled:** {}\n", if eff_enabled { "yes" } else { "no" }));
            out.push_str(&format!("**Delegate model:** {}\n", effective_model.as_deref().unwrap_or("(not set)")));
            out.push_str(&format!("**Timeout:** {} minutes\n", effective_timeout));

            if !eff_allowed.is_empty() {
                out.push_str(&format!("**Allowed models:** {}\n", eff_allowed.join(", ")));
            } else {
                out.push_str("**Allowed models:** (all models — no restriction set)\n");
            }
            if let Some(ref session_cfg) = session_cfg {
                out.push_str("\n**Session overrides active:**\n");
                if let Some(val) = session_cfg.enabled {
                    out.push_str(&format!("- enabled: {}\n", if val { "yes" } else { "no" }));
                }
                if let Some(ref model) = session_cfg.delegate_model {
                    out.push_str(&format!("- model: {}\n", model));
                }
                if let Some(ref provider) = session_cfg.delegate_provider {
                    out.push_str(&format!("- provider: {}\n", provider));
                }
                if let Some(timeout) = session_cfg.timeout_minutes {
                    out.push_str(&format!("- timeout: {} min\n", timeout));
                }
                if let Some(ref models) = session_cfg.allowed_models {
                    if models.is_empty() {
                        out.push_str("- allowed_models: [] (no models allowed)\n");
                    } else {
                        out.push_str(&format!("- allowed_models: [{}]\n", models.join(", ")));
                    }
                }
            } else {
                out.push_str("\n**No session overrides.** Using config file values.\n");
            }
            out.push_str(&format!("\n**Config file:** enabled={}", file_cfg.enabled));
            if let Some(ref model) = file_cfg.delegate_model {
                out.push_str(&format!(", model={}", model));
            }
            if let Some(ref provider) = file_cfg.delegate_provider {
                out.push_str(&format!(", provider={}", provider));
            }
            out.push_str(&format!(", timeout={}min", file_cfg.timeout_minutes));
            if !file_cfg.allowed_models.is_empty() {
                out.push_str(&format!(", allowed_models=[{}]", file_cfg.allowed_models.join(", ")));
            }
            out.push('\n');
            return Ok(ToolOutput::new(out));
        }

        // Validate the model if one was provided
        if let Some(Some(ref model_val)) = model {
            if let Err(msg) = validate_model_allowed(&ctx.session_id, model_val) {
                let eff_allowed = allowed_models(&ctx.session_id);
                return Ok(ToolOutput::new(format!(
                    "Cannot set delegate model: {}\n\nAvailable models: {}",
                    msg,
                    if eff_allowed.is_empty() { "(no restriction — any model)".to_string() } else { eff_allowed.join(", ") }
                )));
            }
        }

        crate::session_delegate_config::update_session_delegate_config(
            &ctx.session_id,
            model,
            provider,
            timeout,
            enabled,
            allowed_models_val,
        );

        let eff_enabled = effective_enabled(&ctx.session_id);
        let effective_model = crate::session_delegate_config::effective_delegate_model(&ctx.session_id);
        let effective_timeout = crate::session_delegate_config::effective_delegate_timeout(&ctx.session_id);
        let eff_allowed = allowed_models(&ctx.session_id);

        let mut out = String::from("Delegate configuration updated for this session.\n\n");
        out.push_str(&format!("**Enabled:** {}\n", if eff_enabled { "yes" } else { "no" }));
        out.push_str(&format!("**Delegate model:** {}\n", effective_model.as_deref().unwrap_or("(not set)")));
        out.push_str(&format!("**Timeout:** {} minutes\n", effective_timeout));
        if !eff_allowed.is_empty() {
            out.push_str(&format!("**Allowed models:** {}\n", eff_allowed.join(", ")));
        } else {
            out.push_str("**Allowed models:** (all models — no restriction)\n");
        }
        out.push_str("\nChanges persist for this session only. Use `configure_delegate` again to change them.");

        Ok(ToolOutput::new(out))
    }
}