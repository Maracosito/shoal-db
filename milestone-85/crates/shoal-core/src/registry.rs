//! Global registry: organizations, projects, namespaces, and the segment
//! reference-count table used for copy-on-write branching.
//!
//! Stored as two JSON objects (`registry/registry.json`,
//! `registry/segrefs.json`). All mutations go through the engine's global
//! registry lock and are read-modify-write; this is a deliberate v1
//! single-writer simplification documented in the consistency model.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Result, ShoalError};
use crate::types::now_ms;
use bytes::Bytes;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Org {
    pub id: String,
    pub name: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamespaceEntry {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub version: u64,
    /// keyed by org id
    pub orgs: BTreeMap<String, Org>,
    /// keyed by project id
    pub projects: BTreeMap<String, Project>,
    /// keyed by namespace id
    pub namespaces: BTreeMap<String, NamespaceEntry>,
}

/// Allowed: alphanumerics plus `.`, `_`, `-`; 1..=128 chars.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(ShoalError::InvalidRequest(
            "name must be 1..=128 characters".into(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(ShoalError::InvalidRequest(format!(
            "invalid name '{}': allowed characters are [a-zA-Z0-9._-]",
            name
        )));
    }
    Ok(())
}

impl Registry {
    pub fn encode(&self) -> Result<Bytes> {
        Ok(Bytes::from(serde_json::to_vec_pretty(self)?))
    }

    pub fn decode(data: &[u8]) -> Result<Registry> {
        Ok(serde_json::from_slice(data)?)
    }

    pub fn org_by_name(&self, name: &str) -> Option<&Org> {
        self.orgs.values().find(|o| o.name == name)
    }

    pub fn project_by_name(&self, org_id: &str, name: &str) -> Option<&Project> {
        self.projects
            .values()
            .find(|p| p.org_id == org_id && p.name == name)
    }

    pub fn namespace_by_name(&self, project_id: &str, name: &str) -> Option<&NamespaceEntry> {
        self.namespaces
            .values()
            .find(|n| n.project_id == project_id && n.name == name)
    }

    pub fn add_org(&mut self, id: String, name: &str) -> Result<Org> {
        validate_name(name)?;
        if self.org_by_name(name).is_some() {
            return Err(ShoalError::AlreadyExists(format!("org '{}'", name)));
        }
        let org = Org {
            id: id.clone(),
            name: name.to_string(),
            created_at_ms: now_ms(),
        };
        self.orgs.insert(id, org.clone());
        self.version += 1;
        Ok(org)
    }

    pub fn add_project(&mut self, id: String, org_name: &str, name: &str) -> Result<Project> {
        validate_name(name)?;
        let org_id = self
            .org_by_name(org_name)
            .ok_or_else(|| ShoalError::NotFound(format!("org '{}'", org_name)))?
            .id
            .clone();
        if self.project_by_name(&org_id, name).is_some() {
            return Err(ShoalError::AlreadyExists(format!("project '{}'", name)));
        }
        let project = Project {
            id: id.clone(),
            org_id,
            name: name.to_string(),
            created_at_ms: now_ms(),
        };
        self.projects.insert(id, project.clone());
        self.version += 1;
        Ok(project)
    }

    pub fn resolve_project(&self, org_name: &str, project_name: &str) -> Result<&Project> {
        let org = self
            .org_by_name(org_name)
            .ok_or_else(|| ShoalError::NotFound(format!("org '{}'", org_name)))?;
        self.project_by_name(&org.id, project_name)
            .ok_or_else(|| ShoalError::NotFound(format!("project '{}'", project_name)))
    }

    pub fn add_namespace(
        &mut self,
        id: String,
        org_name: &str,
        project_name: &str,
        name: &str,
    ) -> Result<NamespaceEntry> {
        validate_name(name)?;
        let project_id = self.resolve_project(org_name, project_name)?.id.clone();
        if self.namespace_by_name(&project_id, name).is_some() {
            return Err(ShoalError::AlreadyExists(format!("namespace '{}'", name)));
        }
        let ns = NamespaceEntry {
            id: id.clone(),
            project_id,
            name: name.to_string(),
            created_at_ms: now_ms(),
        };
        self.namespaces.insert(id, ns.clone());
        self.version += 1;
        Ok(ns)
    }

    pub fn resolve_namespace(
        &self,
        org_name: &str,
        project_name: &str,
        name: &str,
    ) -> Result<&NamespaceEntry> {
        let project = self.resolve_project(org_name, project_name)?;
        self.namespace_by_name(&project.id, name)
            .ok_or_else(|| ShoalError::NotFound(format!("namespace '{}'", name)))
    }

    pub fn remove_namespace(
        &mut self,
        org_name: &str,
        project_name: &str,
        name: &str,
    ) -> Result<NamespaceEntry> {
        let id = self.resolve_namespace(org_name, project_name, name)?.id.clone();
        self.version += 1;
        Ok(self.namespaces.remove(&id).expect("resolved namespace"))
    }
}

/// Global segment reference counts. A physical segment object may only be
/// deleted when its count reaches zero. Crash-safety direction: counts are
/// incremented *before* the referencing manifest is written and decremented
/// *after* the dereferencing manifest is written, so a crash can leak a
/// segment (later GC) but never delete still-referenced data.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SegRefCounts {
    pub counts: BTreeMap<String, u32>,
}

impl SegRefCounts {
    pub fn encode(&self) -> Result<Bytes> {
        Ok(Bytes::from(serde_json::to_vec_pretty(self)?))
    }

    pub fn decode(data: &[u8]) -> Result<SegRefCounts> {
        Ok(serde_json::from_slice(data)?)
    }

    pub fn incr(&mut self, key: &str) -> u32 {
        let c = self.counts.entry(key.to_string()).or_insert(0);
        *c += 1;
        *c
    }

    /// Decrement; removes the entry and returns 0 when it reaches zero.
    pub fn decr(&mut self, key: &str) -> u32 {
        match self.counts.get_mut(key) {
            Some(c) => {
                *c = c.saturating_sub(1);
                let v = *c;
                if v == 0 {
                    self.counts.remove(key);
                }
                v
            }
            None => 0,
        }
    }

    pub fn count(&self, key: &str) -> u32 {
        self.counts.get(key).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_hierarchy() {
        let mut r = Registry::default();
        r.add_org("o1".into(), "acme").unwrap();
        r.add_project("p1".into(), "acme", "search").unwrap();
        r.add_namespace("n1".into(), "acme", "search", "docs").unwrap();
        assert!(r.add_namespace("n2".into(), "acme", "search", "docs").is_err());
        assert_eq!(r.resolve_namespace("acme", "search", "docs").unwrap().id, "n1");
        assert!(r.resolve_namespace("acme", "search", "nope").is_err());
        assert!(validate_name("ok-name_1.2").is_ok());
        assert!(validate_name("bad name").is_err());
    }

    #[test]
    fn refcounts() {
        let mut s = SegRefCounts::default();
        assert_eq!(s.incr("k"), 1);
        assert_eq!(s.incr("k"), 2);
        assert_eq!(s.decr("k"), 1);
        assert_eq!(s.decr("k"), 0);
        assert_eq!(s.count("k"), 0);
        assert_eq!(s.decr("missing"), 0);
    }
}
