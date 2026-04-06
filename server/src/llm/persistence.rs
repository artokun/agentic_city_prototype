//! Durable session persistence — checkpoint and event logging.
//!
//! Layout on disk:
//! ```text
//! runtime/llm-sessions/
//!   agent_alice/
//!     state.json      — SessionCheckpoint (latest snapshot)
//!     events.jsonl    — append-only event log
//!   system-ai/
//!     state.json
//!     events.jsonl
//! ```
//!
//! Provider metadata is an opaque JSON blob inside the checkpoint.
//! The persistence layer never interprets it — only the adapter does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::types::{SessionCheckpoint, SessionEvent, SessionOwner, UsageData};

/// Default base directory for session state.
const DEFAULT_SESSIONS_DIR: &str = "runtime/llm-sessions";

/// Persistent event entry written to events.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedEvent {
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// Event kind tag.
    pub kind: String,
    /// Event payload (varies by kind).
    #[serde(default)]
    pub data: serde_json::Value,
}

/// Manages reading and writing session checkpoints and event logs.
pub struct CheckpointStore {
    base_dir: PathBuf,
}

impl CheckpointStore {
    /// Create a store rooted at the given directory.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Create a store using the default or env-configured directory.
    pub fn from_env() -> Self {
        let dir = std::env::var("LLM_SESSIONS_DIR")
            .unwrap_or_else(|_| DEFAULT_SESSIONS_DIR.to_string());
        Self::new(dir)
    }

    /// Get the directory path for a session owner.
    fn owner_dir(&self, owner: &SessionOwner) -> PathBuf {
        self.base_dir.join(owner_to_dirname(owner))
    }

    /// Path to the state.json file for an owner.
    fn state_path(&self, owner: &SessionOwner) -> PathBuf {
        self.owner_dir(owner).join("state.json")
    }

    /// Path to the events.jsonl file for an owner.
    fn events_path(&self, owner: &SessionOwner) -> PathBuf {
        self.owner_dir(owner).join("events.jsonl")
    }

    // -----------------------------------------------------------------------
    // Checkpoint operations
    // -----------------------------------------------------------------------

    /// Save a checkpoint to disk. Creates the directory if needed.
    pub fn save(&self, checkpoint: &SessionCheckpoint) -> Result<(), String> {
        let dir = self.owner_dir(&checkpoint.owner);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;

        let path = self.state_path(&checkpoint.owner);
        let json = serde_json::to_string_pretty(checkpoint)
            .map_err(|e| format!("failed to serialize checkpoint: {e}"))?;

        // Atomic write: write to .tmp then rename.
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json)
            .map_err(|e| format!("failed to write {}: {e}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .map_err(|e| format!("failed to rename to {}: {e}", path.display()))?;

        tracing::debug!(
            "[persist] saved checkpoint for {} ({} bytes)",
            checkpoint.owner,
            json.len()
        );
        Ok(())
    }

    /// Load a checkpoint from disk, if one exists.
    pub fn load(&self, owner: &SessionOwner) -> Result<Option<SessionCheckpoint>, String> {
        let path = self.state_path(owner);
        if !path.exists() {
            return Ok(None);
        }

        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

        let checkpoint: SessionCheckpoint = serde_json::from_str(&contents)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;

        tracing::debug!(
            "[persist] loaded checkpoint for {} (model={}, tokens_in={}, tokens_out={})",
            owner,
            checkpoint.model,
            checkpoint.total_input_tokens,
            checkpoint.total_output_tokens,
        );
        Ok(Some(checkpoint))
    }

    /// Load all checkpoints that exist on disk.
    pub fn load_all(&self) -> HashMap<SessionOwner, SessionCheckpoint> {
        let mut result = HashMap::new();

        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(e) => e,
            Err(_) => return result, // Directory doesn't exist yet.
        };

        for entry in entries.flatten() {
            let state_path = entry.path().join("state.json");
            if !state_path.exists() {
                continue;
            }

            let contents = match std::fs::read_to_string(&state_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "[persist] skipping {}: {e}",
                        state_path.display()
                    );
                    continue;
                }
            };

            match serde_json::from_str::<SessionCheckpoint>(&contents) {
                Ok(cp) => {
                    tracing::info!(
                        "[persist] loaded checkpoint: {} (model={})",
                        cp.owner,
                        cp.model
                    );
                    result.insert(cp.owner.clone(), cp);
                }
                Err(e) => {
                    tracing::warn!(
                        "[persist] failed to parse {}: {e}",
                        state_path.display()
                    );
                }
            }
        }

        result
    }

    /// Remove a checkpoint from disk.
    pub fn remove(&self, owner: &SessionOwner) -> Result<(), String> {
        let dir = self.owner_dir(owner);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| format!("failed to remove {}: {e}", dir.display()))?;
            tracing::debug!("[persist] removed checkpoint for {}", owner);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event log operations
    // -----------------------------------------------------------------------

    /// Append an event to the session's event log.
    pub fn append_event(
        &self,
        owner: &SessionOwner,
        event: &PersistedEvent,
    ) -> Result<(), String> {
        let dir = self.owner_dir(owner);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;

        let path = self.events_path(owner);
        let line = serde_json::to_string(event)
            .map_err(|e| format!("failed to serialize event: {e}"))?;

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("failed to open {}: {e}", path.display()))?;

        writeln!(file, "{}", line)
            .map_err(|e| format!("failed to write to {}: {e}", path.display()))?;

        Ok(())
    }

    /// Read all events from a session's event log.
    pub fn read_events(
        &self,
        owner: &SessionOwner,
    ) -> Result<Vec<PersistedEvent>, String> {
        let path = self.events_path(owner);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

        let mut events = Vec::new();
        for (i, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<PersistedEvent>(line) {
                Ok(event) => events.push(event),
                Err(e) => {
                    tracing::warn!(
                        "[persist] skipping line {} in {}: {e}",
                        i + 1,
                        path.display()
                    );
                }
            }
        }

        Ok(events)
    }

    /// Truncate the event log (e.g. after compaction).
    pub fn truncate_events(&self, owner: &SessionOwner) -> Result<(), String> {
        let path = self.events_path(owner);
        if path.exists() {
            std::fs::write(&path, "")
                .map_err(|e| format!("failed to truncate {}: {e}", path.display()))?;
        }
        Ok(())
    }

    /// Check if a checkpoint exists for an owner.
    pub fn exists(&self, owner: &SessionOwner) -> bool {
        self.state_path(owner).exists()
    }

    /// Get the base directory path.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a SessionOwner to a filesystem-safe directory name.
/// `Agent("Alice Haiku")` → `agent_alice_haiku`
/// `SystemAi` → `system-ai`
/// `Research("web search")` → `research_web_search`
fn owner_to_dirname(owner: &SessionOwner) -> String {
    match owner {
        SessionOwner::Agent(name) => {
            format!("agent_{}", sanitize_name(name))
        }
        SessionOwner::SystemAi => "system-ai".to_string(),
        SessionOwner::Research(topic) => {
            format!("research_{}", sanitize_name(topic))
        }
    }
}

/// Sanitize a name for use as a directory component.
/// Lowercases, replaces non-alphanumeric with underscores, collapses runs.
fn sanitize_name(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut last_was_underscore = false;

    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            result.push(c.to_ascii_lowercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            result.push('_');
            last_was_underscore = true;
        }
    }

    // Trim trailing underscore.
    result.trim_end_matches('_').to_string()
}

/// Create a PersistedEvent from a SessionEvent for logging.
pub fn session_event_to_persisted(event: &SessionEvent) -> PersistedEvent {
    let now = chrono::Utc::now().to_rfc3339();

    match event {
        SessionEvent::TextDelta(text) => PersistedEvent {
            timestamp: now,
            kind: "text_delta".to_string(),
            data: serde_json::json!({ "text": text }),
        },
        SessionEvent::ToolCallRequested(req) => PersistedEvent {
            timestamp: now,
            kind: "tool_call".to_string(),
            data: serde_json::json!({
                "id": req.id,
                "name": req.name,
                "arguments": req.arguments,
            }),
        },
        SessionEvent::Usage(usage) => PersistedEvent {
            timestamp: now,
            kind: "usage".to_string(),
            data: serde_json::json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cost_usd": usage.cost_usd,
            }),
        },
        SessionEvent::Completed => PersistedEvent {
            timestamp: now,
            kind: "completed".to_string(),
            data: serde_json::Value::Null,
        },
        SessionEvent::Error(msg) => PersistedEvent {
            timestamp: now,
            kind: "error".to_string(),
            data: serde_json::json!({ "message": msg }),
        },
        SessionEvent::CompactCompleted => PersistedEvent {
            timestamp: now,
            kind: "compact_completed".to_string(),
            data: serde_json::Value::Null,
        },
    }
}

/// Build a fresh checkpoint from parts (useful for initial save).
pub fn build_checkpoint(
    owner: SessionOwner,
    provider_id: &str,
    model: &str,
    compact_threshold: u32,
    usage: &UsageData,
    last_turn_marker: &str,
    compacted_context: Option<String>,
    provider_metadata: serde_json::Value,
) -> SessionCheckpoint {
    SessionCheckpoint {
        owner,
        provider_id: provider_id.to_string(),
        model: model.to_string(),
        compact_threshold,
        total_input_tokens: usage.input_tokens,
        total_output_tokens: usage.output_tokens,
        total_cost_usd: usage.cost_usd,
        last_turn_marker: last_turn_marker.to_string(),
        compacted_context,
        provider_metadata,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store() -> (CheckpointStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = CheckpointStore::new(dir.path());
        (store, dir)
    }

    fn sample_checkpoint(owner: SessionOwner) -> SessionCheckpoint {
        SessionCheckpoint {
            owner,
            provider_id: "session-abc123".to_string(),
            model: "opus".to_string(),
            compact_threshold: 50_000,
            total_input_tokens: 12_000,
            total_output_tokens: 3_000,
            total_cost_usd: 0.42,
            last_turn_marker: "turn-7".to_string(),
            compacted_context: Some("Agent explored the city.".to_string()),
            provider_metadata: serde_json::json!({
                "claude_session_id": "sess_xyz",
                "resume_url": "ws://localhost:8080/agent/abc/ws"
            }),
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let (store, _dir) = test_store();
        let owner = SessionOwner::Agent("Alice".to_string());
        let checkpoint = sample_checkpoint(owner.clone());

        store.save(&checkpoint).unwrap();
        let loaded = store.load(&owner).unwrap().expect("should exist");

        assert_eq!(loaded.owner, checkpoint.owner);
        assert_eq!(loaded.provider_id, "session-abc123");
        assert_eq!(loaded.model, "opus");
        assert_eq!(loaded.compact_threshold, 50_000);
        assert_eq!(loaded.total_input_tokens, 12_000);
        assert_eq!(loaded.total_output_tokens, 3_000);
        assert!((loaded.total_cost_usd - 0.42).abs() < f64::EPSILON);
        assert_eq!(loaded.last_turn_marker, "turn-7");
        assert_eq!(
            loaded.compacted_context.as_deref(),
            Some("Agent explored the city.")
        );
        assert_eq!(
            loaded.provider_metadata["claude_session_id"],
            "sess_xyz"
        );
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let (store, _dir) = test_store();
        let result = store
            .load(&SessionOwner::Agent("Nobody".to_string()))
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_all_finds_multiple() {
        let (store, _dir) = test_store();

        let alice = sample_checkpoint(SessionOwner::Agent("Alice".to_string()));
        let system = sample_checkpoint(SessionOwner::SystemAi);

        store.save(&alice).unwrap();
        store.save(&system).unwrap();

        let all = store.load_all();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key(&SessionOwner::Agent("Alice".to_string())));
        assert!(all.contains_key(&SessionOwner::SystemAi));
    }

    #[test]
    fn remove_deletes_directory() {
        let (store, _dir) = test_store();
        let owner = SessionOwner::Agent("Bob".to_string());
        let checkpoint = sample_checkpoint(owner.clone());

        store.save(&checkpoint).unwrap();
        assert!(store.exists(&owner));

        store.remove(&owner).unwrap();
        assert!(!store.exists(&owner));
        assert!(store.load(&owner).unwrap().is_none());
    }

    #[test]
    fn event_log_append_and_read() {
        let (store, _dir) = test_store();
        let owner = SessionOwner::Agent("Carol".to_string());

        let event1 = PersistedEvent {
            timestamp: "2026-04-06T00:00:00Z".to_string(),
            kind: "usage".to_string(),
            data: serde_json::json!({"input_tokens": 100, "output_tokens": 50}),
        };
        let event2 = PersistedEvent {
            timestamp: "2026-04-06T00:01:00Z".to_string(),
            kind: "completed".to_string(),
            data: serde_json::Value::Null,
        };

        store.append_event(&owner, &event1).unwrap();
        store.append_event(&owner, &event2).unwrap();

        let events = store.read_events(&owner).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "usage");
        assert_eq!(events[1].kind, "completed");
    }

    #[test]
    fn event_log_truncate() {
        let (store, _dir) = test_store();
        let owner = SessionOwner::SystemAi;

        let event = PersistedEvent {
            timestamp: "2026-04-06T00:00:00Z".to_string(),
            kind: "compact_completed".to_string(),
            data: serde_json::Value::Null,
        };
        store.append_event(&owner, &event).unwrap();
        assert_eq!(store.read_events(&owner).unwrap().len(), 1);

        store.truncate_events(&owner).unwrap();
        assert_eq!(store.read_events(&owner).unwrap().len(), 0);
    }

    #[test]
    fn read_events_nonexistent_returns_empty() {
        let (store, _dir) = test_store();
        let events = store
            .read_events(&SessionOwner::Agent("Nobody".to_string()))
            .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn owner_dirname_sanitization() {
        assert_eq!(
            owner_to_dirname(&SessionOwner::Agent("Alice Haiku".to_string())),
            "agent_alice_haiku"
        );
        assert_eq!(
            owner_to_dirname(&SessionOwner::SystemAi),
            "system-ai"
        );
        assert_eq!(
            owner_to_dirname(&SessionOwner::Research("web search!!".to_string())),
            "research_web_search"
        );
        assert_eq!(
            owner_to_dirname(&SessionOwner::Agent("Bob--Sonnet".to_string())),
            "agent_bob_sonnet"
        );
    }

    #[test]
    fn session_event_conversion() {
        let usage_event = SessionEvent::Usage(UsageData {
            input_tokens: 500,
            output_tokens: 100,
            cost_usd: 0.01,
        });
        let persisted = session_event_to_persisted(&usage_event);
        assert_eq!(persisted.kind, "usage");
        assert_eq!(persisted.data["input_tokens"], 500);

        let text_event = SessionEvent::TextDelta("thinking...".to_string());
        let persisted = session_event_to_persisted(&text_event);
        assert_eq!(persisted.kind, "text_delta");
        assert_eq!(persisted.data["text"], "thinking...");

        let error_event = SessionEvent::Error("connection lost".to_string());
        let persisted = session_event_to_persisted(&error_event);
        assert_eq!(persisted.kind, "error");
        assert_eq!(persisted.data["message"], "connection lost");
    }

    #[test]
    fn save_overwrite_preserves_latest() {
        let (store, _dir) = test_store();
        let owner = SessionOwner::Agent("Alice".to_string());

        let mut cp = sample_checkpoint(owner.clone());
        store.save(&cp).unwrap();

        // Update and save again.
        cp.total_input_tokens = 99_000;
        cp.last_turn_marker = "turn-42".to_string();
        cp.compacted_context = Some("Updated context.".to_string());
        store.save(&cp).unwrap();

        let loaded = store.load(&owner).unwrap().unwrap();
        assert_eq!(loaded.total_input_tokens, 99_000);
        assert_eq!(loaded.last_turn_marker, "turn-42");
        assert_eq!(
            loaded.compacted_context.as_deref(),
            Some("Updated context.")
        );
    }

    #[test]
    fn provider_metadata_round_trip() {
        let (store, _dir) = test_store();
        let owner = SessionOwner::Agent("Test".to_string());

        let metadata = serde_json::json!({
            "claude_session_id": "sess_abc",
            "nested": { "key": [1, 2, 3] },
            "flag": true,
        });

        let mut cp = sample_checkpoint(owner.clone());
        cp.provider_metadata = metadata.clone();
        store.save(&cp).unwrap();

        let loaded = store.load(&owner).unwrap().unwrap();
        assert_eq!(loaded.provider_metadata, metadata);
    }
}
