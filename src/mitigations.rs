//! Mitigations (#42): disable a tool or a whole upstream for a scope, or deny
//! an outbound destination, as a tracked change tied to a decision reference.
//!
//! [`MitigationStore`] keeps them in the state backend ([`crate::state`]).
//! The PostgreSQL backend commits a change and its audit events in one
//! transaction and needs no outbox. The rest of these notes describe the
//! directory backend ([`MitigationFiles`]).
//!
//! The store is one JSON file shared by every replica (`[mitigations] file`,
//! by default in `[cluster] state_dir`). Like the rate-limit buckets it needs
//! a filesystem whose exclusive locks every replica sees.
//! Apply, lift and expiry are read-modify-writes under an exclusive lock on
//! `<file>.lock`, replaced by rename, so concurrent writers from any replica
//! serialize and none loses another's update. Each committed change bumps the
//! store `generation`.
//!
//! Every forwarded call reads the file, so a change applies to the next call
//! on every replica without a restart. A file that cannot be read or parsed
//! refuses every forwarded call; it never means "no mitigations". A missing
//! file is an empty store.
//!
//! Applying or lifting needs a decision reference that a [`DecisionVerifier`]
//! accepts. Without a control plane the standalone verifier trusts a named
//! operator. With `[mitigations.control_plane]` the gateway looks the Action
//! up on the engine ([`crate::control_plane`]): it must be decided, of a
//! mitigating-control kind, and name this mitigation's scope. A rejected
//! decision changes nothing and is audited as `refuse`.
//! Apply, lift and expiry are audited in `[mitigations] audit_file` through an
//! outbox: a change writes the new state and its events (`pending_audit`)
//! in one atomic rename, so both commit or neither does. The events are then
//! appended to the audit log and the outbox is cleared. Every write under the
//! lock first flushes an outbox a crash left behind. The outbox records the
//! log length at commit (`audit_offset`); a flush cuts the log back to it and
//! writes the whole batch, so a crash mid-append or between append and clear
//! never duplicates or tears a record. Refusals change no state and are
//! appended directly, but only under the lock after a flush, so none can
//! sit between an offset and its batch. The audit log therefore only records
//! committed changes. The log's directory is fsynced after it is created
//! and after each flush, also when it lives apart from the store file.
//!
//! Expiry is evaluated from the stored timestamp whenever a call is checked;
//! no timer is needed. The first check that sees an expired mitigation
//! removes it under the lock and audits `expire` once.
//!
//! Outbound destinations are matched on origin: scheme, host and effective
//! port of the upstream URL a call is forwarded to (see [`destination`]).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::control_plane::{ControlPlane, ControlPlaneConfig, ControlPlaneError};
use crate::state::StateBackend;

/// Where the mitigation store and its audit live. Every replica must reach
/// the same files. An explicit path resolves against the config file's
/// directory; an unset one defaults into `[cluster] state_dir` when that is
/// set, so replicas with configs in different directories still share one
/// store, and next to the config otherwise ([`Self::resolve`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MitigationsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// Apply, lift, expiry and refusals, one JSON line each.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_file: Option<PathBuf>,
    /// When set, apply and lift verify the decision against the engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<ControlPlaneConfig>,
}

const FILE: &str = "mitigations.json";
const AUDIT_FILE: &str = "mitigations-audit.jsonl";

impl MitigationsConfig {
    /// Fill in both paths after load: explicit ones join `base` (the config
    /// directory), unset ones go to the already-resolved `state_dir`, or to
    /// `base` without a cluster.
    pub fn resolve(&mut self, base: &Path, state_dir: Option<&Path>) {
        let home = state_dir.unwrap_or(base);
        let pick = |set: &Option<PathBuf>, default: &str| match set {
            Some(path) => base.join(path),
            None => home.join(default),
        };
        self.file = Some(pick(&self.file, FILE));
        self.audit_file = Some(pick(&self.audit_file, AUDIT_FILE));
        if let Some(control_plane) = &mut self.control_plane {
            control_plane.resolve(base);
        }
    }
}

/// What a mitigation stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    /// One registry tool (`<upstream>.<tool>`).
    Tool { tool: String },
    /// Every tool of one upstream.
    Upstream { upstream: String },
    /// Forwarding to an origin, normalized by [`destination`].
    Destination { destination: String },
}

/// Whose calls a mitigation stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    All,
    Tenant { tenant: String },
    Agent { agent: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mitigation {
    /// Decision reference that applied it.
    pub decision: String,
    pub operator: String,
    pub target: Target,
    pub scope: Scope,
    pub applied_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
}

impl Mitigation {
    /// Stable id: decision reference, target and scope. Applying the same
    /// decision to the same target and scope again is the same mitigation.
    pub fn id(&self) -> String {
        let key = serde_json::json!([self.decision, self.target, self.scope]);
        let digest = Sha256::digest(key.to_string().as_bytes());
        digest[..16].iter().map(|b| format!("{b:02x}")).collect()
    }

    fn active(&self, now_ms: u64) -> bool {
        self.expires_at_unix_ms.is_none_or(|at| now_ms < at)
    }

    fn in_scope(&self, call: &Call<'_>) -> bool {
        match &self.scope {
            Scope::All => true,
            Scope::Tenant { tenant } => call.tenant == Some(tenant.as_str()),
            Scope::Agent { agent } => call.agent == agent,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let blank = |value: &str| value.trim().is_empty();
        if blank(&self.operator) {
            return Err("a mitigation needs an operator".into());
        }
        if blank(&self.decision) {
            return Err("a mitigation needs a decision reference".into());
        }
        match &self.target {
            Target::Tool { tool } if !tool.contains('.') || blank(tool) => {
                return Err(format!(
                    "tool {tool:?} is not a registry key <upstream>.<tool>"
                ));
            }
            Target::Upstream { upstream } if blank(upstream) => {
                return Err("upstream must not be empty".into());
            }
            Target::Destination { destination: given } if destination(given, true)? != *given => {
                return Err(format!("destination {given:?} is not normalized"));
            }
            _ => {}
        }
        match &self.scope {
            Scope::Tenant { tenant: id } | Scope::Agent { agent: id } if blank(id) => {
                Err("a tenant or agent scope needs an id".into())
            }
            _ => Ok(()),
        }
    }
}

/// `scheme://host:port` of `url`, as forwarding reaches it: scheme and host
/// lowercased, internationalized names in ASCII form, IP literals in
/// canonical form (IPv6 in brackets), a trailing dot dropped, and the port
/// made explicit (80 for http, 443 for https). Only http and https are
/// accepted. The host is compared as written: no DNS lookup, so a name and
/// an address of the same server are different destinations. `strict`
/// (deny entries) also refuses user info, a path other than `/`, a query and
/// a fragment, since only the origin is matched.
pub fn destination(url: &str, strict: bool) -> Result<String, String> {
    let parsed = url::Url::parse(url.trim()).map_err(|error| format!("{url:?}: {error}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("{url:?}: only http and https destinations"));
    }
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| format!("{url:?}: no host"))?;
    let host = host.strip_suffix('.').unwrap_or(host);
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("{url:?}: no port"))?;
    if strict
        && (!parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some())
    {
        return Err(format!(
            "{url:?}: a destination is scheme://host[:port] without path, query or user info"
        ));
    }
    Ok(format!("{scheme}://{host}:{port}"))
}

/// One forwarded call, as mitigations see it.
pub struct Call<'a> {
    pub tool: &'a str,
    pub upstream: &'a str,
    /// The upstream URL the call is forwarded to.
    pub url: &'a str,
    pub agent: &'a str,
    pub tenant: Option<&'a str>,
}

/// Apply or lift, for the verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Apply,
    Lift,
}

/// What a [`DecisionVerifier`] is asked to accept.
pub struct DecisionCheck<'a> {
    pub change: Change,
    /// The decision reference presented for this change.
    pub decision: &'a str,
    pub operator: &'a str,
    pub mitigation: &'a Mitigation,
}

/// Accepts or rejects the decision behind an apply or lift. A rejection's
/// reason is audited and returned to the operator.
pub trait DecisionVerifier {
    fn verify(&self, check: &DecisionCheck<'_>) -> Result<(), String>;
}

/// Without a control plane the operator is the authority: any well-formed
/// reference (1 to 128 of `A-Z a-z 0-9 . _ : / # -`) from a named operator is
/// accepted. The reference is kept so it can be traced to the operator's
/// own record of the decision.
pub struct Standalone;

impl DecisionVerifier for Standalone {
    fn verify(&self, check: &DecisionCheck<'_>) -> Result<(), String> {
        let well_formed = (1..=128).contains(&check.decision.len())
            && check
                .decision
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/#-".contains(&b));
        if !well_formed {
            return Err("decision reference must be 1-128 of A-Z a-z 0-9 . _ : / # -".into());
        }
        if check.operator.trim().is_empty() {
            return Err("an operator must be named".into());
        }
        Ok(())
    }
}

/// The verifier selected by config: standalone, or the engine.
pub enum DecisionAuthority {
    Standalone,
    ControlPlane(ControlPlane),
}

impl DecisionAuthority {
    pub fn from_config(config: &MitigationsConfig) -> Result<Self, ControlPlaneError> {
        match &config.control_plane {
            Some(control_plane) => Ok(Self::ControlPlane(ControlPlane::from_config(
                control_plane,
            )?)),
            None => Ok(Self::Standalone),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::ControlPlane(_) => "control_plane",
        }
    }
}

impl DecisionVerifier for DecisionAuthority {
    fn verify(&self, check: &DecisionCheck<'_>) -> Result<(), String> {
        match self {
            Self::Standalone => Standalone.verify(check),
            Self::ControlPlane(plane) => plane.verify(check),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MitigationError {
    #[error("{0}")]
    Invalid(String),
    #[error("decision refused: {0}")]
    Refused(String),
    #[error("mitigation store {path}: {reason}")]
    Store { path: PathBuf, reason: String },
    #[error("mitigations: {0}")]
    Backend(#[from] crate::state::StoreError),
}

/// What an apply or lift did. A repeat of a committed change is `Unchanged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Lifted,
    Unchanged,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// Bumped by every committed change.
    pub generation: u64,
    /// By [`Mitigation::id`].
    pub mitigations: BTreeMap<String, Mitigation>,
    /// Events of committed changes not yet appended to the audit log.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_audit: Vec<MitigationEvent>,
    /// Audit log length when `pending_audit` was committed: the batch is
    /// appended at exactly this offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_offset: Option<u64>,
}

impl State {
    /// Every stored entry validates and is stored under its own id; any
    /// that does not makes the whole store unreadable.
    pub(crate) fn validate(&self) -> Result<(), String> {
        for (id, mitigation) in &self.mitigations {
            mitigation
                .validate()
                .map_err(|reason| format!("entry {id}: {reason}"))?;
            if mitigation.id() != *id {
                return Err(format!("entry {id} is stored under the wrong id"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Apply,
    Lift,
    Expire,
    /// The verifier rejected an apply or lift; nothing changed.
    Refuse,
}

/// One line of the mitigation audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MitigationEvent {
    /// `<action>:<mitigation>:<generation>`; a refusal gets a random id.
    pub event_id: String,
    pub at_unix_ms: u64,
    pub action: Action,
    pub mitigation: String,
    /// The decision that applied the mitigation.
    pub decision: String,
    /// The decision presented to lift it (lift, or a refused lift).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lift_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    pub target: Target,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl MitigationEvent {
    fn new(
        action: Action,
        id: &str,
        mitigation: &Mitigation,
        generation: u64,
        now_ms: u64,
    ) -> Self {
        let tag = match action {
            Action::Apply => "apply",
            Action::Lift => "lift",
            Action::Expire => "expire",
            Action::Refuse => "refuse",
        };
        Self {
            event_id: format!("{tag}:{id}:{generation}"),
            at_unix_ms: now_ms,
            action,
            mitigation: id.into(),
            decision: mitigation.decision.clone(),
            lift_decision: None,
            operator: None,
            target: mitigation.target.clone(),
            scope: mitigation.scope.clone(),
            expires_at_unix_ms: mitigation.expires_at_unix_ms,
            reason: None,
        }
    }
}

/// What one locked edit of the mitigation state commits.
pub enum Commit {
    /// The state as changed, with the events that audit it. No events
    /// means nothing changed and nothing is written.
    Changes(Vec<MitigationEvent>),
    /// A refused decision: audited, and the state is left as it was.
    Refusal(Box<MitigationEvent>),
}

/// Mitigations in the state backend ([`crate::state`]), shared by every
/// replica.
#[derive(Clone)]
pub struct MitigationStore {
    store: Arc<dyn StateBackend>,
}

impl MitigationStore {
    pub fn new(store: Arc<dyn StateBackend>) -> Self {
        Self { store }
    }

    /// The current state. Any stored entry that does not validate makes the
    /// whole store unreadable.
    pub fn load(&self) -> Result<State, MitigationError> {
        self.store.mitigations()
    }

    /// The active mitigation that refuses `call`, if any. Expired entries do
    /// not count; seeing one prunes and audits it. An unreadable store, or an
    /// unparsable upstream URL while a destination deny is active, is an
    /// error, which callers treat as a refusal.
    pub fn check(
        &self,
        call: &Call<'_>,
        now_ms: u64,
    ) -> Result<Option<Mitigation>, MitigationError> {
        let state = self.load()?;
        if state.mitigations.values().any(|m| !m.active(now_ms))
            && let Err(error) = self.expire(now_ms)
        {
            // The entry is inactive either way; the next check retries.
            eprintln!("expiring mitigations failed: {error}");
        }
        // Only a destination deny needs the origin; an unparsable upstream
        // URL fails closed then, and only then.
        let origin = destination(call.url, false);
        for mitigation in state.mitigations.values() {
            if !mitigation.active(now_ms) || !mitigation.in_scope(call) {
                continue;
            }
            let hit = match &mitigation.target {
                Target::Tool { tool } => call.tool == tool,
                Target::Upstream { upstream } => call.upstream == upstream,
                Target::Destination {
                    destination: denied,
                } => {
                    origin.as_ref().map_err(|reason| {
                        MitigationError::Invalid(format!("upstream destination: {reason}"))
                    })? == denied
                }
            };
            if hit {
                return Ok(Some(mitigation.clone()));
            }
        }
        Ok(None)
    }

    /// Apply `mitigation` once `verifier` accepts its decision. While the
    /// same decision, target and scope is active, applying it again is
    /// `Unchanged`, whatever its expiry: a retry never re-applies, and a new
    /// expiry needs a new decision.
    pub fn apply(
        &self,
        verifier: &impl DecisionVerifier,
        mut mitigation: Mitigation,
        now_ms: u64,
    ) -> Result<(String, Outcome), MitigationError> {
        if let Target::Destination { destination: given } = &mut mitigation.target {
            *given = destination(given, true).map_err(MitigationError::Invalid)?;
        }
        mitigation.validate().map_err(MitigationError::Invalid)?;
        mitigation.applied_at_unix_ms = now_ms;
        if mitigation.expires_at_unix_ms.is_some_and(|at| at <= now_ms) {
            return Err(MitigationError::Invalid("expiry is in the past".into()));
        }
        let id = mitigation.id();
        let check = DecisionCheck {
            change: Change::Apply,
            decision: &mitigation.decision,
            operator: &mitigation.operator,
            mitigation: &mitigation,
        };
        if let Err(reason) = verifier.verify(&check) {
            let mut event = MitigationEvent::new(Action::Refuse, &id, &mitigation, 0, now_ms);
            event.operator = Some(mitigation.operator.clone());
            return Err(self.refuse(event, reason));
        }
        let mut outcome = Outcome::Unchanged;
        self.store.edit_mitigations(&mut |state| {
            if state.mitigations.get(&id).is_some_and(|m| m.active(now_ms)) {
                outcome = Outcome::Unchanged;
                return Ok(Commit::Changes(Vec::new()));
            }
            let mut event = MitigationEvent::new(
                Action::Apply,
                &id,
                &mitigation,
                state.generation + 1,
                now_ms,
            );
            event.operator = Some(mitigation.operator.clone());
            state.mitigations.insert(id.clone(), mitigation.clone());
            outcome = Outcome::Applied;
            Ok(Commit::Changes(vec![event]))
        })?;
        Ok((id, outcome))
    }

    /// Lift mitigation `id` once `verifier` accepts `decision`. Lifting one
    /// that is not stored (already lifted or expired) is `Unchanged`.
    /// The stored entry is snapshotted and verified without the store lock,
    /// then removed only if the same entry is still present, so a slow
    /// control-plane lookup cannot stall other writers and a swap cannot
    /// lift a different mitigation.
    pub fn lift(
        &self,
        verifier: &impl DecisionVerifier,
        id: &str,
        decision: &str,
        operator: &str,
        now_ms: u64,
    ) -> Result<Outcome, MitigationError> {
        let Some(mitigation) = self.load()?.mitigations.get(id).cloned() else {
            return Ok(Outcome::Unchanged);
        };
        let check = DecisionCheck {
            change: Change::Lift,
            decision,
            operator,
            mitigation: &mitigation,
        };
        if let Err(reason) = verifier.verify(&check) {
            let mut event = MitigationEvent::new(Action::Refuse, id, &mitigation, 0, now_ms);
            event.lift_decision = Some(decision.into());
            event.operator = Some(operator.into());
            return Err(self.refuse(event, reason));
        }
        let mut outcome = Outcome::Unchanged;
        self.store.edit_mitigations(&mut |state| {
            match state.mitigations.get(id) {
                Some(current) if current == &mitigation => {}
                _ => {
                    outcome = Outcome::Unchanged;
                    return Ok(Commit::Changes(Vec::new()));
                }
            }
            state.mitigations.remove(id);
            let mut event =
                MitigationEvent::new(Action::Lift, id, &mitigation, state.generation + 1, now_ms);
            event.lift_decision = Some(decision.into());
            event.operator = Some(operator.into());
            outcome = Outcome::Lifted;
            Ok(Commit::Changes(vec![event]))
        })?;
        Ok(outcome)
    }

    /// Remove and audit every mitigation expired at `now_ms`.
    pub fn expire(&self, now_ms: u64) -> Result<(), MitigationError> {
        self.store.edit_mitigations(&mut |state| {
            let generation = state.generation + 1;
            let expired: Vec<String> = state
                .mitigations
                .iter()
                .filter(|(_, m)| !m.active(now_ms))
                .map(|(id, _)| id.clone())
                .collect();
            let events = expired
                .iter()
                .filter_map(|id| state.mitigations.remove(id).map(|m| (id, m)))
                .map(|(id, m)| MitigationEvent::new(Action::Expire, id, &m, generation, now_ms))
                .collect();
            Ok(Commit::Changes(events))
        })
    }

    /// Audit a refused decision; the store is not touched.
    fn refuse(&self, event: MitigationEvent, reason: String) -> MitigationError {
        let event = refusal(event, &reason);
        if let Err(error) = self
            .store
            .edit_mitigations(&mut |_| Ok(Commit::Refusal(Box::new(event.clone()))))
        {
            eprintln!("auditing a refused mitigation decision failed: {error}");
        }
        MitigationError::Refused(reason)
    }
}

/// The audit events of importing `source`'s mitigations as the change to
/// `generation`: one `apply` each, by `operator`, with the reason `imported`.
#[cfg(feature = "postgres")]
pub(crate) fn imported(
    source: &State,
    operator: &str,
    generation: u64,
    now_ms: u64,
) -> Vec<MitigationEvent> {
    source
        .mitigations
        .iter()
        .map(|(id, mitigation)| {
            let mut event = MitigationEvent::new(Action::Apply, id, mitigation, generation, now_ms);
            event.operator = Some(operator.into());
            event.reason = Some("imported".into());
            event
        })
        .collect()
}

/// `event` as a refusal audit line: its own random id and the reason.
fn refusal(mut event: MitigationEvent, reason: &str) -> MitigationEvent {
    event.event_id = format!("refuse:{}", uuid::Uuid::new_v4().simple());
    event.reason = Some(reason.into());
    event
}

/// The directory backend's mitigation store: the shared JSON file, its
/// audit log, and the outbox between them (see the module docs).
#[derive(Debug, Clone)]
pub(crate) struct MitigationFiles {
    file: PathBuf,
    pub(crate) audit_file: PathBuf,
    /// Test hook: fail as if the process died just before this step of a commit.
    #[cfg(test)]
    crash: Option<Crash>,
    /// Test hook: directories fsynced for the audit log.
    #[cfg(test)]
    pub(crate) synced_dirs: std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crash {
    Rename,
    Append,
    Clear,
}

impl MitigationFiles {
    pub(crate) fn new(config: &MitigationsConfig) -> Self {
        Self {
            file: config.file.clone().unwrap_or_else(|| FILE.into()),
            audit_file: config
                .audit_file
                .clone()
                .unwrap_or_else(|| AUDIT_FILE.into()),
            #[cfg(test)]
            crash: None,
            #[cfg(test)]
            synced_dirs: Default::default(),
        }
    }

    #[cfg(test)]
    fn crash_at(&self, at: Crash) -> Result<(), MitigationError> {
        match self.crash == Some(at) {
            true => Err(self.error(format!("injected crash {at:?}"))),
            false => Ok(()),
        }
    }

    fn error(&self, reason: impl std::fmt::Display) -> MitigationError {
        MitigationError::Store {
            path: self.file.clone(),
            reason: reason.to_string(),
        }
    }

    /// The current state, without locking (writers replace the file by
    /// rename). Any stored entry that does not validate, such as a
    /// destination that is not normalized, makes the whole store unreadable.
    pub(crate) fn load(&self) -> Result<State, MitigationError> {
        let state: State = match std::fs::read(&self.file) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|error| self.error(error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(self.error(error)),
        };
        state.validate().map_err(|reason| self.error(reason))?;
        Ok(state)
    }

    fn locked(&self) -> Result<File, MitigationError> {
        if let Some(parent) = self.file.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| self.error(error))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.file.with_extension("lock"))
            .map_err(|error| self.error(error))?;
        lock.lock().map_err(|error| self.error(error))?;
        Ok(lock)
    }

    /// Whether [`Self::edit`] could run: under the store lock, the state
    /// parses, a scratch file can be written and renamed next to it, and the
    /// audit log opens as [`Self::open_log`] opens it.
    pub(crate) fn probe(&self) -> Result<(), MitigationError> {
        // `locked` creates the store's directory; the audit log's must exist
        // unless it is that same directory.
        let parent = crate::state::Parent::Created;
        let audit_parent = if self.audit_file.parent() == self.file.parent() {
            parent
        } else {
            crate::state::Parent::MustExist
        };
        let _lock = crate::state::probe_lock(&self.file.with_extension("lock"), false, parent)
            .map_err(|error| self.error(error))?;
        let state = self.load()?;
        // A pending outbox must still fit the log, as `flush` requires; a
        // missing log is created empty by `flush`.
        if !state.pending_audit.is_empty() {
            let len = match std::fs::metadata(&self.audit_file) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(self.audit_error(error)),
            };
            self.outbox_offset(&state, len)?;
        }
        crate::state::probe_writable(&self.file, parent).map_err(|error| self.error(error))?;
        crate::state::probe_append(&self.audit_file, true, audit_parent)
            .map_err(|error| self.audit_error(error))
    }

    /// Where a pending outbox is appended, given the audit log's length:
    /// its `audit_offset`, which the log must reach. A log shorter than the
    /// offset was truncated or replaced outside Sekisho; that fails visibly
    /// instead of guessing. Only meaningful while events are pending.
    fn outbox_offset(&self, state: &State, len: u64) -> Result<u64, MitigationError> {
        let offset = state
            .audit_offset
            .ok_or_else(|| self.error("pending audit events without an audit offset"))?;
        if len < offset {
            return Err(self.audit_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("audit log is {len} bytes, shorter than its recorded {offset}"),
            )));
        }
        Ok(offset)
    }

    fn audit_error(&self, error: std::io::Error) -> MitigationError {
        MitigationError::Store {
            path: self.audit_file.clone(),
            reason: error.to_string(),
        }
    }

    /// The audit log, created if missing (its directory entry synced), cut
    /// back to its last complete line. Only a refusal can leave a torn line
    /// there: a batch's own tail is cut by its `audit_offset` instead.
    fn open_log(&self) -> Result<File, MitigationError> {
        let created = !self.audit_file.exists();
        let mut log = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.audit_file)
            .map_err(|error| self.audit_error(error))?;
        let len = log
            .metadata()
            .map_err(|error| self.audit_error(error))?
            .len();
        let complete = complete_len(&mut log, len).map_err(|error| self.audit_error(error))?;
        if complete < len {
            log.set_len(complete)
                .and_then(|()| log.sync_all())
                .map_err(|error| self.audit_error(error))?;
        }
        if created {
            self.sync_audit_dir()?;
        }
        Ok(log)
    }

    /// fsync the audit log's directory, so its entry survives a crash even
    /// when it lives apart from the store file.
    fn sync_audit_dir(&self) -> Result<(), MitigationError> {
        let parent = match self.audit_file.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| self.audit_error(error))?;
        #[cfg(test)]
        self.synced_dirs.lock().unwrap().push(parent.to_path_buf());
        Ok(())
    }

    fn write_events(
        &self,
        log: &mut File,
        events: &[MitigationEvent],
    ) -> Result<(), MitigationError> {
        let mut lines = Vec::new();
        for event in events {
            serde_json::to_writer(&mut lines, event).map_err(|error| self.error(error))?;
            lines.push(b'\n');
        }
        log.write_all(&lines)
            .and_then(|()| log.sync_data())
            .map_err(|error| self.audit_error(error))
    }

    /// Append a refusal. Lock held, outbox flushed.
    fn append(&self, events: &[MitigationEvent]) -> Result<(), MitigationError> {
        let mut log = self.open_log()?;
        self.write_events(&mut log, events)
    }

    /// Replace the store file atomically: temp file, fsync, rename, and
    /// fsync of the directory.
    fn write_state(&self, state: &State) -> Result<(), MitigationError> {
        let staged = self
            .file
            .with_extension(format!("{}.tmp", std::process::id()));
        let body = serde_json::to_vec_pretty(state).map_err(|error| self.error(error))?;
        let write = || -> std::io::Result<()> {
            let mut file = File::create(&staged)?;
            file.write_all(&body)?;
            file.sync_all()?;
            std::fs::rename(&staged, &self.file)?;
            // A bare filename lives in the working directory, which must be
            // synced too, or the rename is not durable.
            let parent = match self.file.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            File::open(parent)?.sync_all()?;
            Ok(())
        };
        write().map_err(|error| self.error(error))
    }

    /// Append the outbox to the audit log at its `audit_offset`, then clear
    /// it. Lock held. Every writer flushes before it appends anything, so
    /// whatever lies past the offset is a partial or complete copy of this
    /// batch from a crashed flush, and is cut before the batch is written.
    /// A log shorter than the offset was truncated or replaced outside
    /// Sekisho; that fails visibly instead of guessing.
    fn flush(&self, state: &mut State) -> Result<(), MitigationError> {
        if state.pending_audit.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        self.crash_at(Crash::Append)?;
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_file)
            .map_err(|error| self.audit_error(error))?;
        let len = log
            .metadata()
            .map_err(|error| self.audit_error(error))?
            .len();
        let offset = self.outbox_offset(state, len)?;
        if len > offset {
            log.set_len(offset)
                .and_then(|()| log.sync_all())
                .map_err(|error| self.audit_error(error))?;
        }
        self.write_events(&mut log, &state.pending_audit)?;
        self.sync_audit_dir()?;
        #[cfg(test)]
        self.crash_at(Crash::Clear)?;
        state.pending_audit.clear();
        state.audit_offset = None;
        self.write_state(state)
    }

    /// Load, change and replace the store under its lock, first flushing an
    /// outbox left by a crash, so a refusal never lands between a pending
    /// batch's `audit_offset` and the batch itself. A refusal is appended to
    /// the log and the state is not written. Changes and their events commit
    /// in one rename; appending them afterwards is retried by the next write
    /// if it fails, so the change still succeeds.
    pub(crate) fn edit(
        &self,
        change: &mut dyn FnMut(&mut State) -> Result<Commit, MitigationError>,
    ) -> Result<(), MitigationError> {
        let _lock = self.locked()?;
        let mut state = self.load()?;
        self.flush(&mut state)?;
        let events = match change(&mut state)? {
            Commit::Refusal(event) => return self.append(std::slice::from_ref(&*event)),
            Commit::Changes(events) if events.is_empty() => return Ok(()),
            Commit::Changes(events) => events,
        };
        let log = self.open_log()?;
        let offset = log
            .metadata()
            .map_err(|error| self.audit_error(error))?
            .len();
        state.generation += 1;
        state.pending_audit = events;
        state.audit_offset = Some(offset);
        #[cfg(test)]
        self.crash_at(Crash::Rename)?;
        self.write_state(&state)?;
        if let Err(error) = self.flush(&mut state) {
            eprintln!("mitigation audit pending, retried on the next write: {error}");
        }
        Ok(())
    }
}

/// Length of `log` up to and including its last newline: where a torn
/// trailing line (a refusal cut short by a crash) starts. Reads backwards in
/// chunks, so it stops at the first newline whatever the file's size.
fn complete_len(log: &mut File, len: u64) -> std::io::Result<u64> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    const CHUNK: u64 = 64 * 1024;
    let mut end = len;
    let mut buffer = vec![0; CHUNK as usize];
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let chunk = &mut buffer[..(end - start) as usize];
        log.seek(SeekFrom::Start(start))?;
        log.read_exact(chunk)?;
        if let Some(at) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(start + at as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |at| at.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rejecting;
    impl DecisionVerifier for Rejecting {
        fn verify(&self, _: &DecisionCheck<'_>) -> Result<(), String> {
            Err("decision not found".into())
        }
    }

    /// The directory backend's mitigation files in `dir`.
    fn files(dir: &tempfile::TempDir) -> MitigationFiles {
        crate::state::Directory::in_dir(dir.path()).mitigations
    }

    /// A store over the directory backend in `dir`, with `files` for its
    /// mitigations.
    fn store_on(dir: &tempfile::TempDir, files: MitigationFiles) -> MitigationStore {
        let mut backend = crate::state::Directory::in_dir(dir.path());
        backend.mitigations = files;
        MitigationStore::new(Arc::new(backend))
    }

    fn store(dir: &tempfile::TempDir) -> MitigationStore {
        store_on(dir, files(dir))
    }

    fn events(dir: &tempfile::TempDir) -> Vec<MitigationEvent> {
        read_events(&files(dir).audit_file)
    }

    fn read_events(audit_file: &Path) -> Vec<MitigationEvent> {
        std::fs::read_to_string(audit_file)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn disable(tool: &str, scope: Scope, expires: Option<u64>) -> Mitigation {
        Mitigation {
            decision: "DEC-1".into(),
            operator: "ops-1".into(),
            target: Target::Tool { tool: tool.into() },
            scope,
            applied_at_unix_ms: 0,
            expires_at_unix_ms: expires,
        }
    }

    fn call<'a>(tool: &'a str, agent: &'a str) -> Call<'a> {
        Call {
            tool,
            upstream: "tools",
            url: "http://127.0.0.1:9100/mcp",
            agent,
            tenant: Some("t1"),
        }
    }

    #[test]
    fn apply_and_lift_are_idempotent_and_audited_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mitigation = disable("tools.echo", Scope::All, None);
        let (id, outcome) = store.apply(&Standalone, mitigation.clone(), 10).unwrap();
        assert_eq!(outcome, Outcome::Applied);
        // A retry, even one recomputing a relative expiry, is the same apply.
        let retry = Mitigation {
            expires_at_unix_ms: Some(500),
            ..mitigation
        };
        assert_eq!(
            store.apply(&Standalone, retry, 20).unwrap(),
            (id.clone(), Outcome::Unchanged)
        );
        assert_eq!(store.load().unwrap().generation, 1);
        let lift = |at| store.lift(&Standalone, &id, "DEC-2", "ops-1", at).unwrap();
        assert_eq!(lift(30), Outcome::Lifted);
        assert_eq!(lift(40), Outcome::Unchanged);
        let audit = events(&dir);
        let actions: Vec<Action> = audit.iter().map(|e| e.action).collect();
        assert_eq!(actions, [Action::Apply, Action::Lift]);
        assert_eq!(audit[1].decision, "DEC-1");
        assert_eq!(audit[1].lift_decision.as_deref(), Some("DEC-2"));
        assert_eq!(audit[1].event_id, format!("lift:{id}:2"));
    }

    fn crashing(dir: &tempfile::TempDir, at: Crash) -> MitigationStore {
        store_on(
            dir,
            MitigationFiles {
                crash: Some(at),
                ..files(dir)
            },
        )
    }

    fn actions(dir: &tempfile::TempDir) -> Vec<Action> {
        events(dir).iter().map(|e| e.action).collect()
    }

    #[test]
    fn a_crash_before_the_rename_leaves_neither_state_nor_event() {
        let dir = tempfile::tempdir().unwrap();
        let crashed = crashing(&dir, Crash::Rename);
        assert!(
            crashed
                .apply(&Standalone, disable("tools.a", Scope::All, None), 10)
                .is_err()
        );
        let store = store(&dir);
        assert_eq!(store.load().unwrap(), State::default());
        assert!(actions(&dir).is_empty());
    }

    #[test]
    fn a_commit_whose_append_crashed_is_audited_once_by_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        // The change commits; its event stays in the outbox.
        crashing(&dir, Crash::Append)
            .apply(&Standalone, disable("tools.a", Scope::All, None), 10)
            .unwrap();
        let store = store(&dir);
        assert!(actions(&dir).is_empty());
        assert_eq!(store.load().unwrap().pending_audit.len(), 1);
        store
            .apply(&Standalone, disable("tools.b", Scope::All, None), 20)
            .unwrap();
        assert_eq!(actions(&dir), [Action::Apply, Action::Apply]);
        assert!(store.load().unwrap().pending_audit.is_empty());
    }

    #[test]
    fn a_pending_outbox_over_a_truncated_log_fails_the_probe_until_restored() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store
            .apply(&Standalone, disable("tools.a", Scope::All, None), 10)
            .unwrap();
        // The next commit stays in the outbox, past the first apply's line.
        crashing(&dir, Crash::Append)
            .apply(&Standalone, disable("tools.b", Scope::All, None), 20)
            .unwrap();
        let log = files(&dir).audit_file;
        let saved = std::fs::read(&log).unwrap();
        std::fs::write(&log, b"").unwrap();
        let before = std::fs::read(dir.path().join("mitigations.json")).unwrap();
        assert!(files(&dir).probe().is_err());
        assert_eq!(
            std::fs::read(dir.path().join("mitigations.json")).unwrap(),
            before,
            "the probe modifies nothing"
        );
        assert!(std::fs::read(&log).unwrap().is_empty());
        assert!(
            store
                .apply(&Standalone, disable("tools.c", Scope::All, None), 30)
                .is_err(),
            "edits fail as before"
        );
        std::fs::write(&log, saved).unwrap();
        files(&dir).probe().unwrap();
        store
            .apply(&Standalone, disable("tools.c", Scope::All, None), 30)
            .unwrap();
        assert_eq!(actions(&dir), [Action::Apply, Action::Apply, Action::Apply]);
    }

    #[test]
    fn a_crash_before_clearing_the_outbox_does_not_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        crashing(&dir, Crash::Clear)
            .apply(&Standalone, disable("tools.a", Scope::All, None), 10)
            .unwrap();
        let store = store(&dir);
        assert_eq!(actions(&dir), [Action::Apply]);
        assert_eq!(store.load().unwrap().pending_audit.len(), 1);
        store
            .apply(&Standalone, disable("tools.b", Scope::All, None), 20)
            .unwrap();
        let ids: Vec<String> = events(&dir).into_iter().map(|e| e.event_id).collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn a_batch_over_a_mebibyte_is_recovered_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let crashed = crashing(&dir, Crash::Clear);
        // Two committed events of 600 KiB each: appended, never cleared.
        let big = |n: u64| {
            let mut event = MitigationEvent::new(
                Action::Apply,
                &format!("m{n}"),
                &disable("tools.a", Scope::All, None),
                n,
                1,
            );
            event.reason = Some("x".repeat(600 * 1024));
            event
        };
        crashed
            .store
            .edit_mitigations(&mut |_| Ok(Commit::Changes(vec![big(1), big(2)])))
            .unwrap();
        let store = store(&dir);
        assert_eq!(store.load().unwrap().pending_audit.len(), 2);
        store
            .apply(&Standalone, disable("tools.b", Scope::All, None), 20)
            .unwrap();
        let ids: Vec<String> = events(&dir).into_iter().map(|e| e.event_id).collect();
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert_eq!(&ids[..2], ["apply:m1:1", "apply:m2:2"]);
        let state = store.load().unwrap();
        assert!(state.pending_audit.is_empty() && state.audit_offset.is_none());
    }

    #[test]
    fn an_audit_log_in_its_own_directory_is_created_and_synced() {
        let dir = tempfile::tempdir().unwrap();
        let audit_dir = dir.path().join("audit");
        std::fs::create_dir(&audit_dir).unwrap();
        let files = MitigationFiles {
            audit_file: audit_dir.join("mitigations-audit.jsonl"),
            ..files(&dir)
        };
        let synced_dirs = Arc::clone(&files.synced_dirs);
        let audit_file = files.audit_file.clone();
        let store = store_on(&dir, files);
        store
            .apply(&Standalone, disable("tools.a", Scope::All, None), 10)
            .unwrap();
        let actions: Vec<Action> = read_events(&audit_file).iter().map(|e| e.action).collect();
        assert_eq!(actions, [Action::Apply]);
        let synced = synced_dirs.lock().unwrap().clone();
        assert!(synced.contains(&audit_dir), "{synced:?}");
    }

    #[test]
    fn a_torn_audit_tail_is_dropped_before_the_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store
            .apply(&Standalone, disable("tools.a", Scope::All, None), 10)
            .unwrap();
        let mut log = OpenOptions::new()
            .append(true)
            .open(files(&dir).audit_file)
            .unwrap();
        log.write_all(b"{\"event_id\":\"apply:torn").unwrap();
        store
            .apply(&Standalone, disable("tools.b", Scope::All, None), 20)
            .unwrap();
        // Every line parses: the first apply, then the second, nothing torn.
        let tools: Vec<Target> = events(&dir).into_iter().map(|e| e.target).collect();
        assert_eq!(
            tools,
            [
                Target::Tool {
                    tool: "tools.a".into()
                },
                Target::Tool {
                    tool: "tools.b".into()
                }
            ]
        );
    }

    /// Records the entry it was asked about and, while it decides, has
    /// another replica try to replace that entry.
    struct Racing {
        other: MitigationStore,
        replacement: Mitigation,
        inspected: std::cell::RefCell<Option<Mitigation>>,
    }

    impl DecisionVerifier for Racing {
        fn verify(&self, check: &DecisionCheck<'_>) -> Result<(), String> {
            *self.inspected.borrow_mut() = Some(check.mitigation.clone());
            let (other, replacement) = (self.other.clone(), self.replacement.clone());
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = done.send(other.apply(&Standalone, replacement, 200));
            });
            // Verify is unlocked, so the replacement can land before this
            // lift re-acquires the store.
            let raced = finished
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("replacement should apply while the lift is deciding");
            raced.expect("replacement apply");
            Ok(())
        }
    }

    #[test]
    fn a_lift_removes_only_the_entry_it_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let (id, _) = store
            .apply(
                &Standalone,
                disable("tools.echo", Scope::All, Some(100)),
                10,
            )
            .unwrap();
        // Same decision, target and scope, so the same id, applied anew by
        // another operator once the first has expired.
        let replacement = Mitigation {
            operator: "ops-2".into(),
            ..disable("tools.echo", Scope::All, Some(10_000))
        };
        let racing = Racing {
            other: store.clone(),
            replacement: replacement.clone(),
            inspected: Default::default(),
        };
        assert_eq!(
            store.lift(&racing, &id, "DEC-2", "ops-1", 200).unwrap(),
            Outcome::Unchanged
        );
        let inspected = racing.inspected.into_inner().unwrap();
        assert_eq!(inspected.expires_at_unix_ms, Some(100));
        assert!(
            events(&dir).into_iter().all(|e| e.action != Action::Lift),
            "lifted a replacement that landed while verifying"
        );
        let stored = store.load().unwrap().mitigations.remove(&id).unwrap();
        assert_eq!(stored.operator, "ops-2");
    }

    #[test]
    fn a_rejected_decision_changes_nothing_and_is_audited() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let error = store
            .apply(&Rejecting, disable("tools.echo", Scope::All, None), 10)
            .unwrap_err();
        assert!(matches!(error, MitigationError::Refused(_)), "{error}");
        assert!(!dir.path().join("mitigations.json").exists());
        let (id, _) = store
            .apply(&Standalone, disable("tools.echo", Scope::All, None), 10)
            .unwrap();
        let before = store.load().unwrap();
        assert!(store.lift(&Rejecting, &id, "DEC-2", "ops-1", 20).is_err());
        assert_eq!(store.load().unwrap(), before);
        let refused: Vec<_> = events(&dir)
            .into_iter()
            .filter(|e| e.action == Action::Refuse)
            .collect();
        assert_eq!(refused.len(), 2);
        assert_eq!(refused[1].reason.as_deref(), Some("decision not found"));
        assert_eq!(refused[1].lift_decision.as_deref(), Some("DEC-2"));
    }

    #[test]
    fn the_standalone_verifier_needs_a_well_formed_reference() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut mitigation = disable("tools.echo", Scope::All, None);
        mitigation.decision = "two words".into();
        assert!(matches!(
            store.apply(&Standalone, mitigation, 10),
            Err(MitigationError::Refused(_))
        ));
    }

    #[test]
    fn scope_and_target_select_the_calls_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let agent = Scope::Agent {
            agent: "agent-7".into(),
        };
        store
            .apply(&Standalone, disable("tools.echo", agent, None), 10)
            .unwrap();
        assert!(
            store
                .check(&call("tools.echo", "agent-7"), 20)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .check(&call("tools.echo", "agent-8"), 20)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .check(&call("tools.other", "agent-7"), 20)
                .unwrap()
                .is_none()
        );
        let tenant = Mitigation {
            target: Target::Upstream {
                upstream: "tools".into(),
            },
            scope: Scope::Tenant {
                tenant: "t1".into(),
            },
            ..disable("unused.unused", Scope::All, None)
        };
        store.apply(&Standalone, tenant, 10).unwrap();
        assert!(
            store
                .check(&call("tools.other", "agent-8"), 20)
                .unwrap()
                .is_some()
        );
        let mut other_tenant = call("tools.other", "agent-8");
        other_tenant.tenant = Some("t2");
        assert!(store.check(&other_tenant, 20).unwrap().is_none());
    }

    #[test]
    fn expiry_is_evaluated_on_check_and_audited_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store
            .apply(
                &Standalone,
                disable("tools.echo", Scope::All, Some(100)),
                10,
            )
            .unwrap();
        let echo = call("tools.echo", "agent-7");
        assert!(store.check(&echo, 99).unwrap().is_some());
        let second = self::store(&dir);
        assert!(store.check(&echo, 100).unwrap().is_none());
        assert!(second.check(&echo, 101).unwrap().is_none());
        let expired = events(&dir)
            .into_iter()
            .filter(|e| e.action == Action::Expire)
            .count();
        assert_eq!(expired, 1);
        assert!(store.load().unwrap().mitigations.is_empty());
    }

    #[test]
    fn concurrent_applies_do_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        let tools: Vec<String> = (0..8).map(|n| format!("tools.t{n}")).collect();
        std::thread::scope(|scope| {
            for tool in &tools {
                let store = store(&dir);
                scope.spawn(move || {
                    store
                        .apply(&Standalone, disable(tool, Scope::All, None), 10)
                        .unwrap()
                });
            }
        });
        let state = store(&dir).load().unwrap();
        assert_eq!(state.mitigations.len(), tools.len());
        assert_eq!(state.generation, tools.len() as u64);
    }

    #[test]
    fn destinations_normalize_to_scheme_host_and_port() {
        let same = [
            "http://127.0.0.1:9100",
            "HTTP://127.0.0.1:9100/",
            "http://127.1:9100",
        ];
        for url in same {
            assert_eq!(
                destination(url, true).unwrap(),
                "http://127.0.0.1:9100",
                "{url}"
            );
        }
        assert_eq!(
            destination("https://API.Example.test./", true).unwrap(),
            "https://api.example.test:443"
        );
        assert_eq!(
            destination("http://[::1]/mcp", false).unwrap(),
            "http://[::1]:80"
        );
        for invalid in [
            "not a url",
            "ftp://example.test",
            "http://example.test/mcp",
            "http://user@example.test",
            "http://example.test/?q=1",
        ] {
            assert!(destination(invalid, true).is_err(), "{invalid}");
        }
    }

    #[test]
    fn destination_deny_matches_the_forwarding_origin_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let deny = Mitigation {
            target: Target::Destination {
                destination: "HTTP://127.0.0.1:9100/".into(),
            },
            ..disable("unused.unused", Scope::All, None)
        };
        store.apply(&Standalone, deny, 10).unwrap();
        assert!(
            store
                .check(&call("tools.echo", "agent-7"), 20)
                .unwrap()
                .is_some()
        );
        let mut elsewhere = call("tools.echo", "agent-7");
        elsewhere.url = "http://127.0.0.1:9101/mcp";
        assert!(store.check(&elsewhere, 20).unwrap().is_none());
        elsewhere.url = "::not a url";
        assert!(
            store.check(&elsewhere, 20).is_err(),
            "unparsable fails closed"
        );
    }

    #[test]
    fn an_unreadable_store_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let echo = call("tools.echo", "agent-7");
        std::fs::write(dir.path().join("mitigations.json"), "not-json").unwrap();
        assert!(store.check(&echo, 10).is_err());
        let raw = serde_json::json!({
            "generation": 1,
            "mitigations": { "x": {
                "decision": "D", "operator": "o", "applied_at_unix_ms": 0,
                "target": { "kind": "destination", "destination": "http://Example.test" },
                "scope": { "kind": "all" },
            }},
        });
        std::fs::write(dir.path().join("mitigations.json"), raw.to_string()).unwrap();
        assert!(store.check(&echo, 10).is_err(), "unnormalized destination");
        assert!(
            store
                .apply(&Standalone, disable("tools.echo", Scope::All, None), 10)
                .is_err(),
            "no write over an unreadable store"
        );
    }
}
