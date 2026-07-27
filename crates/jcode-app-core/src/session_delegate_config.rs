//! Per-session runtime configuration for the delegate tool.
//!
//! Allows the model to manage delegate settings (model, provider, timeout)
//! through tool calls instead of editing the config file.
//!
//! Each session can override the static `[delegate]` config section. The
//! delegate tool reads this override first, falling back to the file config.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Runtime delegate configuration for a single session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DelegateSessionConfig {
    /// Model override for this session (e.g. "claude-opus-4-8").
    /// When None, falls back to the file config's delegate_model.
    pub delegate_model: Option<String>,
    /// Provider override for this session.
    /// When None, falls back to the file config's delegate_provider.
    pub delegate_provider: Option<String>,
    /// Timeout override in minutes for this session.
    /// When None, falls back to the file config's timeout_minutes.
    pub timeout_minutes: Option<u32>,
    /// Whether delegation is enabled for this session.
    /// When None, falls back to the file config's enabled.
    pub enabled: Option<bool>,
    /// List of allowed models for this session.
    /// When None, falls back to the file config's allowed_models.
    /// When Some(empty), no models are allowed (delegation effectively disabled).
    /// When Some(non-empty), only these models can be used for delegation.
    pub allowed_models: Option<Vec<String>>,
}

static SESSION_DELEGATE_CONFIGS: LazyLock<RwLock<HashMap<String, DelegateSessionConfig>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Record (or clear) a session's delegate configuration.
/// Pass `None` for any field to keep the current value (or fall back to file config).
pub fn record_session_delegate_config(
    session_id: &str,
    config: Option<DelegateSessionConfig>,
) {
    let Ok(mut map) = SESSION_DELEGATE_CONFIGS.write() else {
        return;
    };
    match config {
        Some(cfg) => {
            map.insert(session_id.to_string(), cfg);
        }
        None => {
            map.remove(session_id);
        }
    }
}

/// Update specific fields of a session's delegate configuration.
/// `None` fields leave the current value unchanged.
pub fn update_session_delegate_config(
    session_id: &str,
    delegate_model: Option<Option<String>>,
    delegate_provider: Option<Option<String>>,
    timeout_minutes: Option<Option<u32>>,
    enabled: Option<Option<bool>>,
    allowed_models: Option<Option<Vec<String>>>,
) {
    let Ok(mut map) = SESSION_DELEGATE_CONFIGS.write() else {
        return;
    };
    let config = map.entry(session_id.to_string()).or_default();
    if let Some(model) = delegate_model {
        config.delegate_model = model;
    }
    if let Some(provider) = delegate_provider {
        config.delegate_provider = provider;
    }
    if let Some(timeout) = timeout_minutes {
        config.timeout_minutes = timeout;
    }
    if let Some(flag) = enabled {
        config.enabled = flag;
    }
    if let Some(models) = allowed_models {
        config.allowed_models = models;
    }
}

/// Look up a session's delegate configuration override, if any.
pub fn session_delegate_config(session_id: &str) -> Option<DelegateSessionConfig> {
    SESSION_DELEGATE_CONFIGS
        .read()
        .ok()?
        .get(session_id)
        .cloned()
}

/// Drop a session's entry entirely (called on session teardown).
pub fn forget_session_delegate_config(session_id: &str) {
    if let Ok(mut map) = SESSION_DELEGATE_CONFIGS.write() {
        map.remove(session_id);
    }
}

/// Resolve the effective delegate model for a session: check runtime override
/// first, then the file config's delegate_model, then return None.
pub fn effective_delegate_model(session_id: &str) -> Option<String> {
    if let Some(cfg) = session_delegate_config(session_id) {
        if let Some(model) = cfg.delegate_model {
            return Some(model);
        }
    }
    crate::config::config().delegate.delegate_model.clone()
}

/// Resolve the effective delegate provider for a session.
pub fn effective_delegate_provider(session_id: &str) -> Option<String> {
    if let Some(cfg) = session_delegate_config(session_id) {
        if let Some(provider) = cfg.delegate_provider {
            return Some(provider);
        }
    }
    crate::config::config().delegate.delegate_provider.clone()
}

/// Resolve the effective delegate timeout for a session.
pub fn effective_delegate_timeout(session_id: &str) -> u32 {
    if let Some(cfg) = session_delegate_config(session_id) {
        if let Some(timeout) = cfg.timeout_minutes {
            return timeout;
        }
    }
    crate::config::config().delegate.timeout_minutes
}

/// Get the list of allowed models for delegation.
/// Checks session config first, then file config.
pub fn allowed_models(session_id: &str) -> Vec<String> {
    if let Some(cfg) = session_delegate_config(session_id) {
        if let Some(models) = cfg.allowed_models {
            return models;
        }
    }
    crate::config::config().delegate.allowed_models.clone()
}

/// Check whether delegation is enabled for a session.
/// Checks session config first, then file config.
pub fn effective_enabled(session_id: &str) -> bool {
    if let Some(cfg) = session_delegate_config(session_id) {
        if let Some(flag) = cfg.enabled {
            return flag;
        }
    }
    crate::config::config().delegate.enabled
}

/// Validate that a model is in the allowed list.
/// Returns Ok(()) if the model is allowed or the list is empty (no restriction).
/// Also allows the configured delegate_model even if not in the list.
pub fn validate_model_allowed(session_id: &str, model: &str) -> Result<(), String> {
    let allowed = allowed_models(session_id);
    if allowed.is_empty() {
        return Ok(());
    }
    // Also allow the configured delegate_model
    let cfg = &crate::config::config().delegate;
    if let Some(ref default_model) = cfg.delegate_model {
        if model == default_model {
            return Ok(());
        }
    }
    // Also allow the session override
    if let Some(session_cfg) = session_delegate_config(session_id) {
        if let Some(ref session_model) = session_cfg.delegate_model {
            if model == session_model {
                return Ok(());
            }
        }
    }
    if allowed.iter().any(|m| m == model) {
        Ok(())
    } else {
        Err(format!(
            "Model '{}' is not in the allowed list. Allowed models: {}",
            model,
            allowed.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reads_back_config() {
        let sid = "session-delegate-config-roundtrip";
        forget_session_delegate_config(sid);
        assert_eq!(session_delegate_config(sid), None);

        let cfg = DelegateSessionConfig {
            delegate_model: Some("claude-opus-4-8".to_string()),
            delegate_provider: None,
            timeout_minutes: Some(60),
            enabled: None,
            allowed_models: None,
        };
        record_session_delegate_config(sid, Some(cfg.clone()));
        assert_eq!(session_delegate_config(sid).as_ref(), Some(&cfg));

        // Overwrite with a new value.
        let cfg2 = DelegateSessionConfig {
            delegate_model: Some("gpt-5.5".to_string()),
            delegate_provider: Some("openai".to_string()),
            timeout_minutes: None,
            enabled: Some(true),
            allowed_models: Some(vec!["gpt-5.5".to_string()]),
        };
        record_session_delegate_config(sid, Some(cfg2.clone()));
        assert_eq!(session_delegate_config(sid).as_ref(), Some(&cfg2));

        // Clearing forgets it.
        record_session_delegate_config(sid, None);
        assert_eq!(session_delegate_config(sid), None);
    }

    #[test]
    fn update_specific_fields() {
        let sid = "session-delegate-config-update";
        forget_session_delegate_config(sid);

        // Set just the model.
        update_session_delegate_config(
            sid,
            Some(Some("claude-opus-4-8".to_string())),
            None,
            None,
            None,
            None,
        );
        let cfg = session_delegate_config(sid).unwrap();
        assert_eq!(cfg.delegate_model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(cfg.delegate_provider, None);
        assert_eq!(cfg.timeout_minutes, None);

        // Update just the timeout, model stays.
        update_session_delegate_config(sid, None, None, Some(Some(45)), None, None);
        let cfg = session_delegate_config(sid).unwrap();
        assert_eq!(cfg.delegate_model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(cfg.timeout_minutes, Some(45));
    }
}