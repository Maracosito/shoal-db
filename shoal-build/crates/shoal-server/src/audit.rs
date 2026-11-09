//! Append-only audit log for namespace and write operations.
//!
//! Audit entries are structured JSON lines containing *who* (the API key ID,
//! never the secret), *what* (a dotted action name such as
//! `namespace.branch` or `documents.delete`), *where* (org / project /
//! namespace), the request ID for correlation with access logs and traces,
//! and the outcome.
//!
//! Sinks:
//! * **File** — newline-delimited JSON appended to a configured path. Each
//!   record is flushed so entries survive a crash immediately after the
//!   operation they describe.
//! * **Tracing** — when no file is configured, entries are emitted through
//!   `tracing` under the `shoal::audit` target so they end up in the
//!   structured log stream and can be routed by collectors.
//!
//! Secret safety: this module never receives raw API keys; callers pass only
//! the public key ID. Detail payloads are caller-constructed summaries
//! (counts, target names), never raw document bodies.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;

/// A single audit record. Serialized as one JSON line.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    /// RFC 3339 UTC timestamp.
    pub timestamp: String,
    /// Request ID, for correlation with access logs and traces.
    pub request_id: String,
    /// Public identifier of the API key that performed the action.
    /// Never the key secret.
    pub actor: String,
    /// Role the key held at the time of the action.
    pub role: String,
    /// Dotted action name, e.g. `namespace.create`, `documents.upsert`.
    pub action: String,
    pub org: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Whether the operation succeeded.
    pub success: bool,
    /// HTTP status code returned to the caller.
    pub status: u16,
    /// Optional structured detail (e.g. `{"upserted": 100}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

enum Sink {
    /// Emit through `tracing` under target `shoal::audit`.
    Tracing,
    /// Append JSON lines to a file, flushing each record.
    File(Mutex<File>),
}

/// Audit logger. Cheap to share behind an `Arc`; `record` never blocks the
/// async runtime for longer than a single small buffered write + flush.
pub struct AuditLogger {
    sink: Sink,
}

impl AuditLogger {
    /// Build a logger from an optional file path. `None` routes entries into
    /// the structured log stream instead of a dedicated file.
    pub fn new(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) => Self::to_file(p),
            None => Ok(Self::to_tracing()),
        }
    }

    /// Audit entries become `tracing` events under target `shoal::audit`.
    pub fn to_tracing() -> Self {
        Self { sink: Sink::Tracing }
    }

    /// Audit entries are appended (and flushed) to `path` as JSON lines.
    pub fn to_file(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating audit log directory {}", parent.display()))?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening audit log {}", path.display()))?;
        Ok(Self {
            sink: Sink::File(Mutex::new(file)),
        })
    }

    /// Record an entry. Failures to persist are reported via `tracing::warn`
    /// rather than failing the request — the audit log is best-effort by
    /// design, and the structured access log remains the fallback record.
    pub fn record(&self, entry: AuditEntry) {
        let line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(err) => {
                tracing::warn!(error = %err, "failed to serialize audit entry");
                return;
            }
        };
        match &self.sink {
            Sink::Tracing => {
                tracing::info!(target: "shoal::audit", audit = %line);
            }
            Sink::File(file) => {
                let mut guard = file.lock();
                if let Err(err) = guard
                    .write_all(line.as_bytes())
                    .and_then(|_| guard.write_all(b"\n"))
                    .and_then(|_| guard.flush())
                {
                    tracing::warn!(error = %err, "failed to write audit entry");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(action: &str) -> AuditEntry {
        AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            request_id: "req-1".into(),
            actor: "key_abc".into(),
            role: "Writer".into(),
            action: action.into(),
            org: "acme".into(),
            project: "search".into(),
            namespace: Some("docs".into()),
            success: true,
            status: 200,
            detail: Some(serde_json::json!({"upserted": 3})),
        }
    }

    #[test]
    fn file_sink_appends_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let logger = AuditLogger::to_file(&path).unwrap();
        logger.record(sample("documents.upsert"));
        logger.record(sample("namespace.branch"));

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["action"], "documents.upsert");
        assert_eq!(first["actor"], "key_abc");
        assert_eq!(first["detail"]["upserted"], 3);
        // The secret must never appear; only the key ID is recorded.
        assert!(!contents.contains("sk_"));
    }

    #[test]
    fn file_sink_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dirs/audit.jsonl");
        let logger = AuditLogger::to_file(&path).unwrap();
        logger.record(sample("namespace.create"));
        assert!(path.exists());
    }
}
