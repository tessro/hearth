//! In-memory guestd state: boot reports, heartbeats, and the pending-restore
//! signal (docs/agent-plane.md §2.1). Derived runtime state, never persisted —
//! guestd re-reports on every (re)connect, so a hearthd restart repopulates
//! this table within one heartbeat interval.

use chrono::Utc;
use hearth_agent_proto::{BootReport, Hello};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct GuestState {
    pub report: BootReport,
    pub component: String,
    pub version: String,
    pub last_seen: String,
    pub connected: bool,
}

impl GuestState {
    pub fn summary(&self) -> Value {
        json!({
            "ready": self.report.ready,
            "component": self.component,
            "version": self.version,
            "hostname": self.report.hostname,
            "addrs": self.report.addrs,
            "agents": self.report.agents,
            "boot_id": self.report.boot_id,
            "last_seen": self.last_seen,
            "connected": self.connected,
        })
    }
}

#[derive(Default)]
struct Inner {
    guests: HashMap<String, GuestState>,
    /// Services whose next boot report follows a `restore` (§3.4): the ack
    /// tells guestd to rotate task incarnations. Set by the restore verb,
    /// consumed when the ack is written.
    pending_restore: HashSet<String>,
}

pub struct GuestTable {
    inner: Mutex<Inner>,
    /// Bumped on every table change so `wait` can sleep between looks.
    changed_tx: watch::Sender<u64>,
}

impl Default for GuestTable {
    fn default() -> Self {
        let (changed_tx, _) = watch::channel(0);
        Self {
            inner: Mutex::new(Inner::default()),
            changed_tx,
        }
    }
}

impl GuestTable {
    pub fn update_report(&self, name: &str, hello: Option<&Hello>, report: BootReport) {
        let mut inner = self.inner.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let existing = inner.guests.get(name);
        let (component, version) = match hello {
            Some(hello) => (hello.component.clone(), hello.version.clone()),
            None => existing
                .map(|g| (g.component.clone(), g.version.clone()))
                .unwrap_or_default(),
        };
        inner.guests.insert(
            name.to_string(),
            GuestState {
                report,
                component,
                version,
                last_seen: now,
                connected: true,
            },
        );
        drop(inner);
        self.bump();
    }

    pub fn heartbeat(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(guest) = inner.guests.get_mut(name) {
            guest.last_seen = Utc::now().to_rfc3339();
        }
        drop(inner);
        self.bump();
    }

    pub fn disconnected(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(guest) = inner.guests.get_mut(name) {
            guest.connected = false;
        }
        drop(inner);
        self.bump();
    }

    /// Forget a guest entirely (service stopped or destroyed): its next report
    /// is a fresh boot, and `status` must not show stale readiness meanwhile.
    pub fn forget(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.guests.remove(name);
        drop(inner);
        self.bump();
    }

    pub fn get(&self, name: &str) -> Option<GuestState> {
        self.inner.lock().unwrap().guests.get(name).cloned()
    }

    pub fn mark_pending_restore(&self, name: &str) {
        self.inner
            .lock()
            .unwrap()
            .pending_restore
            .insert(name.to_string());
    }

    /// Consume the pending-restore flag; the caller must only do this when the
    /// ack that carries `restored: true` was actually delivered.
    pub fn take_pending_restore(&self, name: &str) -> bool {
        self.inner.lock().unwrap().pending_restore.remove(name)
    }

    pub fn restore_pending(&self, name: &str) -> bool {
        self.inner.lock().unwrap().pending_restore.contains(name)
    }

    /// Subscribe to table changes (for `wait`).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed_tx.subscribe()
    }

    fn bump(&self) {
        self.changed_tx.send_modify(|n| *n += 1);
    }
}
