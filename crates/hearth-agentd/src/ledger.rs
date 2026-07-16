//! The delegation ledger (docs/agent-plane.md §4.4). Append-only records that
//! are authoritative for **wake-up authority**: when a callee upcalls a state
//! change, the initiator to wake is looked up here — never reconstructed from
//! callee-controlled `meta.toml`. Refs route; the ledger authorizes.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerRecord {
    /// A delegation was authorized and started on a callee.
    Granted {
        task_id: String,
        initiator: String,
        initiator_thread: Option<String>,
        target: String,
        ts: String,
    },
    /// A delegation was denied by policy (the A2A `rejected` state, §3.2).
    Rejected {
        initiator: String,
        target: String,
        reason: String,
        ts: String,
    },
    /// A delegation was canceled/revoked (task.cancel also revokes here).
    Revoked { task_id: String, ts: String },
}

/// One granted delegation's authority: who to wake and where.
#[derive(Debug, Clone)]
pub struct Grant {
    pub task_id: String,
    pub initiator: String,
    pub initiator_thread: Option<String>,
    pub target: String,
    pub revoked: bool,
}

pub struct Ledger {
    path: Utf8PathBuf,
    file: Mutex<std::fs::File>,
    grants: Mutex<HashMap<String, Grant>>,
}

impl Ledger {
    pub fn open(dir: &Utf8Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("delegations.log");
        // Replay existing records to rebuild the grant index (§9: authority
        // persists across agentd restarts).
        let mut grants: HashMap<String, Grant> = HashMap::new();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(record) = serde_json::from_str::<LedgerRecord>(line) {
                    apply(&mut grants, &record);
                }
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open ledger {path}"))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            grants: Mutex::new(grants),
        })
    }

    pub fn append(&self, record: LedgerRecord) -> Result<()> {
        let line = serde_json::to_string(&record)? + "\n";
        {
            let mut file = self.file.lock().unwrap();
            file.write_all(line.as_bytes())?;
            // fsync, not just flush: the ledger is the wake-up authority, so a
            // Granted record must survive host power loss — otherwise replay
            // after an unclean crash rebuilds a grant map missing recent grants,
            // and their tasks' wake-ups get acked-and-dropped via no_grant.
            file.sync_all()?;
        }
        apply(&mut self.grants.lock().unwrap(), &record);
        Ok(())
    }

    /// Wake-up authority for a task: the initiator to wake, if the grant
    /// exists and is not revoked.
    pub fn grant(&self, task_id: &str) -> Option<Grant> {
        self.grants
            .lock()
            .unwrap()
            .get(task_id)
            .filter(|g| !g.revoked)
            .cloned()
    }

    pub fn all_grants(&self) -> Vec<Grant> {
        self.grants.lock().unwrap().values().cloned().collect()
    }

    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
}

fn apply(grants: &mut HashMap<String, Grant>, record: &LedgerRecord) {
    match record {
        LedgerRecord::Granted {
            task_id,
            initiator,
            initiator_thread,
            target,
            ..
        } => {
            grants.insert(
                task_id.clone(),
                Grant {
                    task_id: task_id.clone(),
                    initiator: initiator.clone(),
                    initiator_thread: initiator_thread.clone(),
                    target: target.clone(),
                    revoked: false,
                },
            );
        }
        LedgerRecord::Revoked { task_id, .. } => {
            if let Some(grant) = grants.get_mut(task_id) {
                grant.revoked = true;
            }
        }
        LedgerRecord::Rejected { .. } => {}
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_is_the_wakeup_authority_and_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let ledger = Ledger::open(&dir).unwrap();
        ledger
            .append(LedgerRecord::Granted {
                task_id: "t1".into(),
                initiator: "boss".into(),
                initiator_thread: Some("th-boss".into()),
                target: "worker".into(),
                ts: now(),
            })
            .unwrap();
        let grant = ledger.grant("t1").unwrap();
        assert_eq!(grant.initiator, "boss");
        assert_eq!(grant.initiator_thread.as_deref(), Some("th-boss"));

        // Reopen: authority persists (agentd restart).
        drop(ledger);
        let reopened = Ledger::open(&dir).unwrap();
        assert_eq!(reopened.grant("t1").unwrap().initiator, "boss");

        // Revocation removes authority.
        reopened
            .append(LedgerRecord::Revoked {
                task_id: "t1".into(),
                ts: now(),
            })
            .unwrap();
        assert!(reopened.grant("t1").is_none());
    }
}
