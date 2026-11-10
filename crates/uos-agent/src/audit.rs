use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::Risk;

const LOG_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allowed by policy without asking.
    Auto,
    Approved,
    Refused,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry<'a> {
    pub tool: &'a str,
    pub args: &'a Value,
    pub risk: Risk,
    pub decision: Decision,
}

/// Append-only record of every action the agent asked for.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `$XDG_STATE_HOME/uos/audit.jsonl`, falling back to `~/.local/state`.
    pub fn default_path() -> PathBuf {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .unwrap_or_else(std::env::temp_dir);
        state_home.join("uos/audit.jsonl")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, entry: &AuditEntry<'_>) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let unix_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let line = serde_json::json!({ "time": unix_time, "action": entry });
        // The log names every command the agent ran; keep it to this user,
        // and do not let a planted link send the entries somewhere else.
        if self.path.symlink_metadata().is_ok_and(|meta| !meta.is_file()) {
            return Err(io::Error::other("the audit log path is not a regular file"));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(LOG_MODE)
            .open(&self.path)?;
        writeln!(file, "{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_json_line_per_action() {
        let path = std::env::temp_dir().join(format!("uos-audit-{}/log.jsonl", std::process::id()));
        let _ = fs::remove_file(&path);
        let log = AuditLog::new(&path);
        let args = serde_json::json!({ "command": "ls" });
        let entry = |decision| AuditEntry {
            tool: "run_command",
            args: &args,
            risk: Risk::Observe,
            decision,
        };

        log.record(&entry(Decision::Auto)).unwrap();
        log.record(&entry(Decision::Succeeded)).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["action"]["tool"], "run_command");
        assert_eq!(lines[0]["action"]["risk"], "observe");
        assert_eq!(lines[0]["action"]["decision"], "auto");
        assert_eq!(lines[1]["action"]["decision"], "succeeded");
        assert!(lines[0]["time"].as_u64().unwrap() > 0);
    }

    #[test]
    fn creates_the_log_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("uos-audit-{}/mode.jsonl", std::process::id()));
        let _ = fs::remove_file(&path);
        let args = Value::Null;

        AuditLog::new(&path)
            .record(&AuditEntry {
                tool: "t",
                args: &args,
                risk: Risk::Observe,
                decision: Decision::Auto,
            })
            .unwrap();

        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn refuses_to_write_through_a_symlink() {
        let dir = std::env::temp_dir().join(format!("uos-audit-{}-link", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("audit.jsonl");
        std::os::unix::fs::symlink(dir.join("elsewhere"), &link).unwrap();
        let args = Value::Null;

        let result = AuditLog::new(&link).record(&AuditEntry {
            tool: "t",
            args: &args,
            risk: Risk::Observe,
            decision: Decision::Auto,
        });

        assert!(result.is_err());
        assert!(!dir.join("elsewhere").exists());
    }

    #[test]
    fn fails_when_the_log_cannot_be_written() {
        let log = AuditLog::new("/proc/uos-not-writable/audit.jsonl");
        let args = Value::Null;

        let result = log.record(&AuditEntry {
            tool: "t",
            args: &args,
            risk: Risk::Modify,
            decision: Decision::Approved,
        });

        assert!(result.is_err());
    }
}
