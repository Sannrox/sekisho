//! Tool registry: upstream tools, their description versions, and owner state.
//!
//! The registry file is the source of truth. The gateway re-reads it and
//! re-lists upstreams every refresh interval, so owner edits and upstream
//! changes take effect without a restart. Every read-modify-write holds an
//! exclusive lock on `<file>.lock`, so the gateway's refresh and an owner's
//! edit cannot overwrite each other.
//!
//! A newly discovered tool is disabled. A changed description or input schema
//! becomes a pending version; agents keep seeing the approved one until the
//! owner approves the change, and `rollback` restores the previously approved
//! version. Owner edits are appended to the audit file.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing registry {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("unknown tool {0}")]
    UnknownTool(String),
    #[error("{0}")]
    Invalid(String),
}

/// What an upstream lists for one tool, as the registry versions it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolVersion {
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEntry {
    pub upstream: String,
    pub name: String,
    pub enabled: bool,
    /// Still listed by the upstream at the last refresh.
    pub available: bool,
    /// Every distinct version seen, oldest first.
    pub versions: Vec<ToolVersion>,
    /// Index of the version agents see.
    pub approved: usize,
    /// Earlier approved indexes, for rollback.
    #[serde(default)]
    pub approved_history: Vec<usize>,
}

impl ToolEntry {
    pub fn approved_version(&self) -> &ToolVersion {
        &self.versions[self.approved]
    }

    /// The newest version, when it is not the approved one.
    pub fn pending(&self) -> Option<&ToolVersion> {
        (self.approved + 1 < self.versions.len()).then(|| &self.versions[self.versions.len() - 1])
    }

    /// Callable by agents: enabled by the owner and still listed upstream.
    pub fn served(&self) -> bool {
        self.enabled && self.available
    }
}

/// Keyed by `upstream.tool`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub tools: BTreeMap<String, ToolEntry>,
}

pub fn tool_key(upstream: &str, tool: &str) -> String {
    format!("{upstream}.{tool}")
}

impl Registry {
    /// Record one upstream's current listing. New tools arrive disabled;
    /// changed ones get a pending version; missing ones become unavailable.
    pub fn observe(&mut self, upstream: &str, listed: &[(String, ToolVersion)]) {
        for entry in self
            .tools
            .values_mut()
            .filter(|entry| entry.upstream == upstream)
        {
            entry.available = listed.iter().any(|(name, _)| *name == entry.name);
        }
        for (name, version) in listed {
            let entry = self
                .tools
                .entry(tool_key(upstream, name))
                .or_insert_with(|| ToolEntry {
                    upstream: upstream.into(),
                    name: name.clone(),
                    enabled: false,
                    available: true,
                    versions: vec![version.clone()],
                    approved: 0,
                    approved_history: Vec::new(),
                });
            if entry.versions.last() != Some(version) {
                entry.versions.push(version.clone());
            }
        }
    }

    /// Stop serving tools of upstreams that are no longer configured.
    pub fn retain_upstreams(&mut self, configured: &[&str]) {
        for entry in self.tools.values_mut() {
            if !configured.contains(&entry.upstream.as_str()) {
                entry.available = false;
            }
        }
    }

    fn entry(&mut self, key: &str) -> Result<&mut ToolEntry, RegistryError> {
        self.tools
            .get_mut(key)
            .ok_or_else(|| RegistryError::UnknownTool(key.into()))
    }

    pub fn set_enabled(&mut self, key: &str, enabled: bool) -> Result<(), RegistryError> {
        self.entry(key)?.enabled = enabled;
        Ok(())
    }

    /// Approve the pending version.
    pub fn approve(&mut self, key: &str) -> Result<(), RegistryError> {
        let entry = self.entry(key)?;
        if entry.pending().is_none() {
            return Err(RegistryError::Invalid(format!(
                "{key} has no pending change"
            )));
        }
        entry.approved_history.push(entry.approved);
        entry.approved = entry.versions.len() - 1;
        Ok(())
    }

    /// Restore the previously approved version.
    pub fn rollback(&mut self, key: &str) -> Result<(), RegistryError> {
        let entry = self.entry(key)?;
        entry.approved = entry.approved_history.pop().ok_or_else(|| {
            RegistryError::Invalid(format!("{key} has no earlier approved version"))
        })?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, RegistryError> {
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|source| RegistryError::Parse {
                path: path.into(),
                source,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(RegistryError::Io {
                action: "reading",
                path: path.into(),
                source,
            }),
        }
    }
}

/// Load, change, and save the registry at `path` under its lock.
pub fn edit<T>(
    path: &Path,
    change: impl FnOnce(&mut Registry) -> Result<T, RegistryError>,
) -> Result<(T, Registry), RegistryError> {
    let lock_path = path.with_extension("lock");
    let io = |action, path: &Path| {
        let path = path.to_path_buf();
        move |source| RegistryError::Io {
            action,
            path,
            source,
        }
    };
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(io("opening", &lock_path))?;
    lock.lock().map_err(io("locking", &lock_path))?;
    let mut registry = Registry::load(path)?;
    let value = change(&mut registry)?;
    let staged = path.with_extension(format!("{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(&registry).map_err(|source| RegistryError::Parse {
        path: path.into(),
        source,
    })?;
    std::fs::write(&staged, body).map_err(io("writing", &staged))?;
    std::fs::rename(&staged, path).map_err(io("replacing", path))?;
    drop(lock);
    Ok((value, registry))
}

/// One owner edit, as appended to the audit file.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuditRecord {
    pub at_unix_ms: u128,
    pub owner: String,
    pub action: String,
    pub tool: String,
}

/// Apply an owner edit and append its audit record. The record is written
/// under the registry lock before the registry file is replaced, so a change
/// that cannot be audited is never committed.
pub fn owner_edit(
    registry: &Path,
    audit: &Path,
    owner: &str,
    action: &str,
    tool: &str,
    change: impl FnOnce(&mut Registry) -> Result<(), RegistryError>,
) -> Result<Registry, RegistryError> {
    if owner.trim().is_empty() {
        return Err(RegistryError::Invalid(
            "registry edits need an owner".into(),
        ));
    }
    let audit_error = |action: &'static str| {
        let path = audit.to_path_buf();
        move |source| RegistryError::Io {
            action,
            path,
            source,
        }
    };
    let ((), registry) = edit(registry, |registry| {
        change(registry)?;
        let record = AuditRecord {
            at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |at| at.as_millis()),
            owner: owner.into(),
            action: action.into(),
            tool: tool.into(),
        };
        let mut line = serde_json::to_vec(&record).map_err(|source| RegistryError::Parse {
            path: audit.into(),
            source,
        })?;
        line.push(b'\n');
        let mut log: File = OpenOptions::new()
            .create(true)
            .append(true)
            .open(audit)
            .map_err(audit_error("opening"))?;
        log.write_all(&line).map_err(audit_error("appending to"))?;
        log.sync_data().map_err(audit_error("syncing"))
    })?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(description: &str) -> ToolVersion {
        ToolVersion {
            description: Some(description.into()),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn new_tools_are_disabled_and_changes_wait_for_approval() {
        let mut registry = Registry::default();
        registry.observe("ontology", &[("search".into(), version("v1"))]);
        let key = tool_key("ontology", "search");
        assert!(!registry.tools[&key].served());

        registry.observe("ontology", &[("search".into(), version("v2"))]);
        let entry = &registry.tools[&key];
        assert_eq!(entry.approved_version(), &version("v1"));
        assert_eq!(entry.pending(), Some(&version("v2")));

        registry.approve(&key).unwrap();
        assert_eq!(registry.tools[&key].approved_version(), &version("v2"));
        registry.rollback(&key).unwrap();
        assert_eq!(registry.tools[&key].approved_version(), &version("v1"));
        assert!(registry.rollback(&key).is_err());

        registry.set_enabled(&key, true).unwrap();
        assert!(registry.tools[&key].served());
        registry.observe("ontology", &[]);
        assert!(
            !registry.tools[&key].served(),
            "delisted tools are not served"
        );

        registry.observe("ontology", &[("search".into(), version("v2"))]);
        assert!(registry.tools[&key].served());
        registry.retain_upstreams(&["other"]);
        assert!(
            !registry.tools[&key].served(),
            "removed upstreams are not served"
        );
    }
}
