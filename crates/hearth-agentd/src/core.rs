//! The shared agentd core: config, hearthd client, ledger, ref keys, and the
//! operations the control socket, HTTP leg, and MCP server all funnel through
//! (docs/agent-plane.md §4). Task *content* never lives here; agentd relays to
//! guestds and holds only delegation *authority*.

use crate::config::Config;
use crate::hearthd::Hearthd;
use crate::ledger::{Ledger, LedgerRecord};
use crate::refs::RefKeys;
use crate::relay;
use anyhow::{bail, Result};
use hearth_agent_proto::taskref::TaskRefClaims;
use hearth_agent_proto::AgentVerb;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tracing::info;

pub struct Agentd {
    pub cfg: Config,
    pub hearthd: Hearthd,
    pub ledger: Ledger,
    pub keys: RefKeys,
    pub delegators: Vec<String>,
    /// Injected clock so tests are deterministic; unix seconds.
    now_fn: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl Agentd {
    pub fn new(cfg: Config, keys: RefKeys, ledger: Ledger) -> Arc<Self> {
        let hearthd = Hearthd::new(&cfg.hearthd_socket);
        let delegators = cfg.delegator_list();
        Arc::new(Self {
            cfg,
            hearthd,
            ledger,
            keys,
            delegators,
            now_fn: Box::new(|| chrono::Utc::now().timestamp()),
        })
    }

    pub fn now(&self) -> i64 {
        (self.now_fn)()
    }

    pub fn mint_ref(&self, target: &str, task_id: &str, initiator: &str, thread: Option<&str>) -> String {
        self.keys.mint(target, task_id, initiator, thread, self.now())
    }

    /// Verify a presented ref and confirm the presenter may use it (§7.1).
    pub fn resolve_ref(&self, token: &str, presenter: &str) -> Result<TaskRefClaims> {
        self.keys.verify_presenter(token, presenter, self.now())
    }

    /// List agent-enabled VMs with their guestd-reported adapters and task
    /// counts (`list_agents` MCP tool / `agent-ls` control verb).
    pub async fn list_agents(&self) -> Result<Value> {
        let endpoints = self.hearthd.agent_endpoints().await?;
        let mut agents = Vec::new();
        for endpoint in endpoints {
            let (adapters, task_count) = if endpoint.running && endpoint.ready {
                let adapters = relay::call(
                    &self.hearthd,
                    &endpoint.name,
                    AgentVerb::AgentLs,
                    Map::new(),
                )
                .await
                .ok()
                .and_then(|v| v.get("agents").cloned())
                .unwrap_or(json!([]));
                let count = relay::call(
                    &self.hearthd,
                    &endpoint.name,
                    AgentVerb::TaskList,
                    Map::new(),
                )
                .await
                .ok()
                .and_then(|v| v.get("tasks").and_then(Value::as_array).map(|t| t.len()))
                .unwrap_or(0);
                (adapters, count)
            } else {
                (json!([]), 0)
            };
            agents.push(json!({
                "name": endpoint.name,
                "running": endpoint.running,
                "ready": endpoint.ready,
                "is_agent_in_charge": endpoint.is_agent_in_charge,
                "adapters": adapters,
                "task_count": task_count,
            }));
        }
        Ok(json!({ "agents": agents }))
    }

    /// Delegate a task to `target` on behalf of `initiator` (§7.1). Policy
    /// check → ledger grant → `task.start` on the callee → signed ref.
    /// A denial is ledgered as a rejection and audited (§4.4).
    pub async fn delegate(
        &self,
        initiator: &str,
        initiator_thread: Option<&str>,
        target: &str,
        text: &str,
    ) -> Result<Value> {
        if !self.is_delegator(initiator) {
            self.ledger.append(LedgerRecord::Rejected {
                initiator: initiator.to_string(),
                target: target.to_string(),
                reason: "not in delegators allowlist".to_string(),
                ts: crate::ledger::now(),
            })?;
            self.audit("delegate", initiator, target, "rejected", None);
            bail!("delegation.rejected: {initiator:?} is not permitted to delegate");
        }
        // Confirm the target is agent-enabled before starting anything.
        let endpoints = self.hearthd.agent_endpoints().await?;
        if !endpoints.iter().any(|e| e.name == target) {
            self.ledger.append(LedgerRecord::Rejected {
                initiator: initiator.to_string(),
                target: target.to_string(),
                reason: "target is not agent-enabled".to_string(),
                ts: crate::ledger::now(),
            })?;
            self.audit("delegate", initiator, target, "rejected", None);
            bail!("agent.not_enabled: {target:?} is not an agent-enabled VM");
        }

        // Ledger the grant BEFORE starting the task (§7.1: policy → ledger →
        // start). The task_id is minted here and pinned into task.start, so the
        // wake-up authority is durable before the callee task can possibly
        // upcall — no window where a fast task's first upcall hits `no_grant`
        // and is acked-and-dropped.
        let task_id = ulid::Ulid::new().to_string();
        self.ledger.append(LedgerRecord::Granted {
            task_id: task_id.clone(),
            initiator: initiator.to_string(),
            initiator_thread: initiator_thread.map(str::to_string),
            target: target.to_string(),
            ts: crate::ledger::now(),
        })?;
        self.audit("delegate", initiator, target, "granted", Some(&task_id));

        let mut args = Map::new();
        args.insert("agent".to_string(), json!(default_agent_for(&endpoints, target)));
        args.insert("text".to_string(), json!(text));
        args.insert("detach".to_string(), json!(true));
        args.insert("task_id".to_string(), json!(task_id));
        args.insert(
            "initiator".to_string(),
            json!({
                "kind": "agent",
                "service": initiator,
                "thread_id": initiator_thread,
            }),
        );
        let started = match relay::call(&self.hearthd, target, AgentVerb::TaskStart, args).await {
            Ok(started) => started,
            Err(err) => {
                // The task never started: revoke the grant so a stale entry
                // can't authorize a spurious wake-up later.
                let _ = self.cancel_grant(&task_id);
                return Err(err);
            }
        };

        let task_ref = self.mint_ref(target, &task_id, initiator, initiator_thread);
        Ok(json!({
            "task_ref": task_ref,
            "task_id": task_id,
            "state": started.get("state"),
        }))
    }

    /// Start a task from the operator/UI seat (§4.1). Unlike agent-to-agent
    /// delegation this bypasses the delegators allowlist (the control socket
    /// and the token-guarded HTTP leg *are* the operator), but it still
    /// ledgers a grant and mints a ref so status/attach/cancel and the ref
    /// machinery work uniformly. A UI has no wake thread (it polls), so no
    /// wake-up is ever injected for it.
    pub async fn delegate_from_ui(&self, presenter: &str, target: &str, text: &str) -> Result<Value> {
        let endpoints = self.hearthd.agent_endpoints().await?;
        if !endpoints.iter().any(|e| e.name == target && e.running) {
            bail!("agent.not_enabled: {target:?} is not a running agent-enabled VM");
        }
        // Ledger-before-start (§7.1), same ordering as agent-to-agent delegate.
        let task_id = ulid::Ulid::new().to_string();
        self.ledger.append(LedgerRecord::Granted {
            task_id: task_id.clone(),
            initiator: presenter.to_string(),
            initiator_thread: None,
            target: target.to_string(),
            ts: crate::ledger::now(),
        })?;
        self.audit("task.start", presenter, target, "granted", Some(&task_id));

        let mut args = Map::new();
        args.insert("agent".to_string(), json!(default_agent_for(&endpoints, target)));
        args.insert("text".to_string(), json!(text));
        args.insert("detach".to_string(), json!(true));
        args.insert("task_id".to_string(), json!(task_id));
        args.insert(
            "initiator".to_string(),
            json!({ "kind": "ui", "service": presenter }),
        );
        let started = match relay::call(&self.hearthd, target, AgentVerb::TaskStart, args).await {
            Ok(started) => started,
            Err(err) => {
                let _ = self.cancel_grant(&task_id);
                return Err(err);
            }
        };
        let task_ref = self.mint_ref(target, &task_id, presenter, None);
        Ok(json!({
            "task_ref": task_ref,
            "task_id": task_id,
            "state": started.get("state"),
        }))
    }

    /// Relay a task verb to the guest owning `claims`, forwarding `extra` args.
    pub async fn relay_verb(
        &self,
        claims: &TaskRefClaims,
        verb: AgentVerb,
        extra: Map<String, Value>,
    ) -> Result<Value> {
        let mut args = extra;
        args.insert("task_id".to_string(), json!(claims.task_id));
        relay::call(&self.hearthd, &claims.target, verb, args).await
    }

    pub fn cancel_grant(&self, task_id: &str) -> Result<()> {
        self.ledger.append(LedgerRecord::Revoked {
            task_id: task_id.to_string(),
            ts: crate::ledger::now(),
        })
    }

    pub fn is_delegator(&self, initiator: &str) -> bool {
        self.delegators.iter().any(|d| d == initiator)
    }

    /// Structured audit line to journald-shaped fields (§4.4). Token-level
    /// event content is never audited; it lives in the guest event log.
    pub fn audit(&self, verb: &str, initiator: &str, agent: &str, result: &str, task: Option<&str>) {
        info!(
            hearth_initiator = %initiator,
            hearth_agent = %agent,
            hearth_task = task.unwrap_or(""),
            verb,
            result,
            "audit"
        );
    }
}

/// Pick the adapter to drive on a target for a bare `delegate` — the first
/// adapter the guest reports, falling back to codex (the day-1 vertical).
fn default_agent_for(_endpoints: &[crate::hearthd::AgentEndpoint], _target: &str) -> String {
    "codex".to_string()
}
