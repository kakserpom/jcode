//! Text mangling: replace sensitive words before sending to the LLM provider
//! and restore them when receiving responses. This hides sensitive data from
//! the provider while keeping conversation context intact.
//!
//! Each session can have its own mangling config (enabled, mappings), with
//! fallback to the file config. Use `/mangle on|off|status` to toggle.

use std::sync::LazyLock;
use std::sync::RwLock;
use std::collections::HashMap;

/// Per-session mangling configuration override.
#[derive(Debug, Clone, Default)]
pub struct MangleSessionConfig {
    /// Whether mangling is enabled for this session.
    pub enabled: Option<bool>,
    /// Optional per-session mapping overrides.
    pub mappings: Option<Vec<crate::config::MangleMapping>>,
}

/// Global session-level mangling config overrides.
static SESSION_MANGLE_CONFIGS: LazyLock<RwLock<HashMap<String, MangleSessionConfig>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Record a session-level mangling config override.
pub fn update_session_mangle_config(
    session_id: &str,
    enabled: Option<Option<bool>>,
    mappings: Option<Option<Vec<crate::config::MangleMapping>>>,
) {
    let Ok(mut map) = SESSION_MANGLE_CONFIGS.write() else {
        return;
    };
    let config = map.entry(session_id.to_string()).or_default();
    if let Some(flag) = enabled {
        config.enabled = flag;
    }
    if let Some(m) = mappings {
        config.mappings = m;
    }
}

/// Look up a session's mangling configuration override, if any.
fn session_mangle_config(session_id: &str) -> Option<MangleSessionConfig> {
    SESSION_MANGLE_CONFIGS
        .read()
        .ok()?
        .get(session_id)
        .cloned()
}

/// Forget a session's mangling config override.
pub fn forget_session_mangle_config(session_id: &str) {
    if let Ok(mut map) = SESSION_MANGLE_CONFIGS.write() {
        map.remove(session_id);
    }
}

/// Check whether mangling is enabled for a session.
/// Checks session config first, then file config.
pub fn effective_mangle_enabled(session_id: &str) -> bool {
    if let Some(cfg) = session_mangle_config(session_id) {
        if let Some(flag) = cfg.enabled {
            return flag;
        }
    }
    crate::config::config().mangle.enabled
}

/// Get the effective mangling mappings for a session.
/// Falls back to file config if no session override.
pub fn effective_mangle_mappings(session_id: &str) -> Vec<crate::config::MangleMapping> {
    if let Some(cfg) = session_mangle_config(session_id) {
        if let Some(mappings) = cfg.mappings {
            return mappings;
        }
    }
    crate::config::config().mangle.mappings.clone()
}

/// Mangle text: replace all sensitive words with their replacements.
/// No-op if mangling is disabled for the session.
pub fn mangle_text(text: &str, session_id: &str) -> String {
    if !effective_mangle_enabled(session_id) {
        return text.to_string();
    }
    let mappings = effective_mangle_mappings(session_id);
    if mappings.is_empty() {
        return text.to_string();
    }
    let mut result = text.to_string();
    for mapping in &mappings {
        if !mapping.sensitive.is_empty() && !mapping.replacement.is_empty() {
            result = result.replace(&mapping.sensitive, &mapping.replacement);
        }
    }
    result
}

/// Demangle text: replace all replacements back with their original sensitive words.
/// No-op if mangling is disabled for the session.
pub fn demangle_text(text: &str, session_id: &str) -> String {
    if !effective_mangle_enabled(session_id) {
        return text.to_string();
    }
    let mappings = effective_mangle_mappings(session_id);
    if mappings.is_empty() {
        return text.to_string();
    }
    let mut result = text.to_string();
    // Reverse order: replace replacements back to sensitive words.
    // We iterate in reverse so that if a replacement is a substring of another,
    // the longer one is replaced first.
    for mapping in mappings.iter().rev() {
        if !mapping.sensitive.is_empty() && !mapping.replacement.is_empty() {
            result = result.replace(&mapping.replacement, &mapping.sensitive);
        }
    }
    result
}

/// Mangle all text content in a provider-bound message.
/// This is called before sending messages to the LLM provider.
pub fn mangle_message(msg: &mut crate::message::Message, session_id: &str) {
    if !effective_mangle_enabled(session_id) {
        return;
    }
    for block in &mut msg.content {
        match block {
            crate::message::ContentBlock::Text { text, .. } => {
                *text = mangle_text(text, session_id);
            }
            crate::message::ContentBlock::Reasoning { text } => {
                *text = mangle_text(text, session_id);
            }
            crate::message::ContentBlock::ReasoningTrace { text } => {
                *text = mangle_text(text, session_id);
            }
            crate::message::ContentBlock::AnthropicThinking { thinking, .. } => {
                *thinking = mangle_text(thinking, session_id);
            }
            crate::message::ContentBlock::ToolUse { input, .. } => {
                if let Some(input_str) = input.as_str() {
                    if let Ok(mangled) = serde_json::from_str::<serde_json::Value>(
                        &mangle_text(input_str, session_id),
                    ) {
                        *input = mangled;
                    }
                }
            }
            crate::message::ContentBlock::ToolResult { content, .. } => {
                *content = mangle_text(content, session_id);
            }
            // Images and OpenAI-specific blocks are not mangled.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_mappings() {
        let mappings = vec![
            crate::config::MangleMapping {
                sensitive: "ProjectX".to_string(),
                replacement: "the project".to_string(),
            },
            crate::config::MangleMapping {
                sensitive: "secret_api_key_42".to_string(),
                replacement: "[REDACTED]".to_string(),
            },
        ];
        update_session_mangle_config(
            "test_session",
            Some(Some(true)),
            Some(Some(mappings)),
        );
    }

    #[test]
    fn test_mangle_text_basic() {
        setup_test_mappings();
        let result = mangle_text("ProjectX is a secret project", "test_session");
        assert_eq!(result, "the project is a secret project");
    }

    #[test]
    fn test_demangle_text_basic() {
        setup_test_mappings();
        let result = demangle_text("the project is a secret project", "test_session");
        assert_eq!(result, "ProjectX is a secret project");
    }

    #[test]
    fn test_mangle_disabled() {
        let result = mangle_text("ProjectX is secret", "no_config_session");
        assert_eq!(result, "ProjectX is secret");
    }

    #[test]
    fn test_mangle_multiple_mappings() {
        setup_test_mappings();
        let result = mangle_text(
            "ProjectX uses secret_api_key_42 for auth",
            "test_session",
        );
        assert_eq!(result, "the project uses [REDACTED] for auth");
    }

    #[test]
    fn test_demangle_multiple_mappings() {
        setup_test_mappings();
        let result = demangle_text(
            "the project uses [REDACTED] for auth",
            "test_session",
        );
        assert_eq!(result, "ProjectX uses secret_api_key_42 for auth");
    }

    #[test]
    fn test_mangle_roundtrip() {
        setup_test_mappings();
        let original = "My project is ProjectX and the key is secret_api_key_42";
        let mangled = mangle_text(original, "test_session");
        let demangled = demangle_text(&mangled, "test_session");
        assert_eq!(demangled.as_str(), original);
    }

    #[test]
    fn test_mangle_no_sensitive_text() {
        setup_test_mappings();
        let result = mangle_text("Hello world, nothing sensitive here", "test_session");
        assert_eq!(result, "Hello world, nothing sensitive here");
    }

    #[test]
    fn test_mangle_empty_mappings() {
        update_session_mangle_config("empty_session", Some(Some(true)), Some(Some(vec![])));
        let result = mangle_text("ProjectX is sensitive", "empty_session");
        assert_eq!(result, "ProjectX is sensitive");
    }

    #[test]
    fn test_mangle_message_text_block() {
        setup_test_mappings();
        let mut msg = crate::message::Message::user("ProjectX is sensitive");
        mangle_message(&mut msg, "test_session");
        let text = msg.content.first().unwrap();
        match text {
            crate::message::ContentBlock::Text { text, .. } => {
                assert_eq!(text, "the project is sensitive");
            }
            _ => panic!("Expected Text block"),
        }
    }

    #[test]
    fn test_mangle_message_tool_result() {
        setup_test_mappings();
        let mut msg = crate::message::Message {
            role: crate::message::Role::User,
            content: vec![crate::message::ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "The result contains secret_api_key_42".to_string(),
                is_error: None,
            }],
            timestamp: Some(chrono::Utc::now()),
            tool_duration_ms: None,
        };
        mangle_message(&mut msg, "test_session");
        match &msg.content[0] {
            crate::message::ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "The result contains [REDACTED]");
            }
            _ => panic!("Expected ToolResult block"),
        }
    }
}