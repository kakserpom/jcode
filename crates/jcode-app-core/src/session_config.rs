//! Session-level configuration flags for various features.
//! Uses a global HashMap keyed by session ID, similar to mangle/delegate configs.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{LazyLock, RwLock};

static SESSION_CONFIGS: LazyLock<RwLock<HashMap<String, SessionConfig>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

#[derive(Debug, Clone, Default)]
struct SessionConfig {
    project_auth_enabled: bool,
}

/// Check whether project auth mode is enabled for a session.
pub fn get_project_auth_enabled(session_id: &str) -> bool {
    if let Ok(map) = SESSION_CONFIGS.read() {
        if let Some(cfg) = map.get(session_id) {
            return cfg.project_auth_enabled;
        }
    }
    false
}

/// Set project auth mode for a session.
pub fn set_project_auth_enabled(session_id: &str, enabled: bool) {
    if let Ok(mut map) = SESSION_CONFIGS.write() {
        let cfg = map.entry(session_id.to_string()).or_default();
        cfg.project_auth_enabled = enabled;
    }
}

/// Generate a filesystem-safe slug from a project path.
/// Used to scope credentials to a specific project.
pub fn project_slug_from_path(path: &str) -> String {
    // Normalize: strip trailing slash, take last 2 dir components
    let trimmed = path.trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    let relevant = if parts.len() > 2 {
        parts[parts.len() - 2..].join("_")
    } else {
        trimmed.replace('/', "_")
    };
    let sanitized: String = relevant
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Add a short hash for uniqueness
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{}_{:x}", &sanitized, hash & 0xFFFF)
}