//! API-key authentication and the admin/writer/reader role model.
//!
//! Token format: `shoal_<key_id>.<secret>` where both parts are random
//! alphanumeric strings. Only `sha256(secret)` is stored on disk; verification
//! is a constant-time comparison of hex digests. Tokens therefore cannot be
//! recovered from the key file, and the key file itself is written with mode
//! 0600 on Unix.
//!
//! ## Scoping
//!
//! Each key is scoped to one organization (or `*` for a root key) and either a
//! single project or `*` for all projects in the org. A key's role grants:
//!
//! - `reader`: query, export, namespace metadata, cache stats
//! - `writer`: reader + upsert/patch/delete, warm/pin, branch/copy
//! - `admin`: writer + namespace create/delete, key management
//!
//! ## Logging discipline
//!
//! Raw tokens must never reach logs. Use [`token_fingerprint`] when a request
//! credential needs to be correlated in audit/log output.

use std::collections::HashMap;
use std::path::Path;

use parking_lot::RwLock;
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing or malformed credentials")]
    InvalidToken,
    #[error("unknown or invalid API key")]
    UnknownKey,
    #[error("API key is disabled")]
    Disabled,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("key store I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("key store parse error: {0}")]
    Parse(String),
}

/// Roles are ordered: `Reader < Writer < Admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Reader,
    Writer,
    Admin,
}

impl Role {
    pub fn allows(self, required: Role) -> bool {
        self >= required
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Writer => "writer",
            Role::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    /// Hex-encoded SHA-256 of the secret half of the token.
    pub secret_sha256: String,
    /// Organization scope; `*` grants access to all organizations (root key).
    pub org: String,
    /// Project scope within the org; `*` grants access to all projects.
    pub project: String,
    pub role: Role,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct KeyFile {
    #[serde(default)]
    pub keys: Vec<ApiKeyRecord>,
}

/// The authenticated identity attached to a request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub key_id: String,
    /// `*` means all organizations.
    pub org: String,
    /// `None` means all projects within the org.
    pub project: Option<String>,
    pub role: Role,
}

impl AuthContext {
    /// Check that this identity may perform an action requiring `required`
    /// role on `org` / `project`. `project = None` denotes an org-level
    /// operation, which requires an org-wide (project = `*`) key.
    pub fn authorize(
        &self,
        org: &str,
        project: Option<&str>,
        required: Role,
    ) -> Result<(), AuthError> {
        if self.org != "*" && self.org != org {
            return Err(AuthError::Forbidden(format!(
                "key is not scoped to organization '{org}'"
            )));
        }
        match (&self.project, project) {
            (Some(scope), Some(req)) if scope != req => {
                return Err(AuthError::Forbidden(format!(
                    "key is not scoped to project '{req}'"
                )));
            }
            (Some(_), None) => {
                return Err(AuthError::Forbidden(
                    "org-level operation requires an org-wide key".to_string(),
                ));
            }
            _ => {}
        }
        if !self.role.allows(required) {
            return Err(AuthError::Forbidden(format!(
                "requires role '{}', key has role '{}'",
                required.as_str(),
                self.role.as_str()
            )));
        }
        Ok(())
    }
}

/// In-memory view of the key file, safe for concurrent request handling.
pub struct AuthStore {
    keys: RwLock<HashMap<String, ApiKeyRecord>>,
}

impl AuthStore {
    pub fn empty() -> Self {
        Self {
            keys: RwLock::new(HashMap::new()),
        }
    }

    pub fn from_key_file(kf: KeyFile) -> Self {
        let store = Self::empty();
        for k in kf.keys {
            store.insert(k);
        }
        store
    }

    pub fn load(path: &Path) -> Result<Self, AuthError> {
        let body = std::fs::read_to_string(path)?;
        let kf: KeyFile = toml::from_str(&body).map_err(|e| AuthError::Parse(e.to_string()))?;
        Ok(Self::from_key_file(kf))
    }

    /// Load the key file, or — when it does not exist — create it with a
    /// freshly generated bootstrap admin key. Returns the store and the
    /// plaintext bootstrap token (present only on first run; the caller must
    /// surface it exactly once and never log it).
    pub fn load_or_bootstrap(path: &Path, org: &str) -> Result<(Self, Option<String>), AuthError> {
        if path.exists() {
            return Ok((Self::load(path)?, None));
        }
        let store = Self::empty();
        let (record, token) = generate_key(org, "*", Role::Admin, Some("bootstrap admin key".into()));
        store.insert(record);
        store.save(path)?;
        Ok((store, Some(token)))
    }

    pub fn insert(&self, record: ApiKeyRecord) {
        self.keys.write().insert(record.id.clone(), record);
    }

    pub fn remove(&self, key_id: &str) -> bool {
        self.keys.write().remove(key_id).is_some()
    }

    pub fn len(&self) -> usize {
        self.keys.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// List key metadata (never includes secrets — hashes are also withheld).
    pub fn list_public(&self) -> Vec<serde_json::Value> {
        let keys = self.keys.read();
        let mut out: Vec<serde_json::Value> = keys
            .values()
            .map(|k| {
                serde_json::json!({
                    "id": k.id,
                    "org": k.org,
                    "project": k.project,
                    "role": k.role,
                    "disabled": k.disabled,
                    "description": k.description,
                })
            })
            .collect();
        out.sort_by_key(|v| v["id"].as_str().unwrap_or_default().to_string());
        out
    }

    /// Persist all keys to `path` (TOML, mode 0600 on Unix).
    pub fn save(&self, path: &Path) -> Result<(), AuthError> {
        let mut keys: Vec<ApiKeyRecord> = self.keys.read().values().cloned().collect();
        keys.sort_by(|a, b| a.id.cmp(&b.id));
        let body = toml::to_string_pretty(&KeyFile { keys })
            .map_err(|e| AuthError::Parse(e.to_string()))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Verify a bearer token and return the authenticated context.
    pub fn verify_token(&self, token: &str) -> Result<AuthContext, AuthError> {
        let rest = token.strip_prefix("shoal_").ok_or(AuthError::InvalidToken)?;
        let (id, secret) = rest.split_once('.').ok_or(AuthError::InvalidToken)?;
        let keys = self.keys.read();
        let record = keys.get(id).ok_or(AuthError::UnknownKey)?;
        if record.disabled {
            return Err(AuthError::Disabled);
        }
        let digest = hex::encode(Sha256::digest(secret.as_bytes()));
        if !constant_time_eq(digest.as_bytes(), record.secret_sha256.as_bytes()) {
            return Err(AuthError::UnknownKey);
        }
        Ok(AuthContext {
            key_id: record.id.clone(),
            org: record.org.clone(),
            project: if record.project == "*" {
                None
            } else {
                Some(record.project.clone())
            },
            role: record.role,
        })
    }
}

/// Generate a new API key. Returns the storable record (hashed secret) and the
/// plaintext token to hand to the user exactly once.
pub fn generate_key(
    org: &str,
    project: &str,
    role: Role,
    description: Option<String>,
) -> (ApiKeyRecord, String) {
    let id = random_string(12);
    let secret = random_string(40);
    let record = ApiKeyRecord {
        id: id.clone(),
        secret_sha256: hex::encode(Sha256::digest(secret.as_bytes())),
        org: org.to_string(),
        project: project.to_string(),
        role,
        disabled: false,
        description,
    };
    (record, format!("shoal_{id}.{secret}"))
}

fn random_string(n: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

/// Constant-time byte comparison; avoids leaking match length via timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Short, non-reversible identifier for a credential, safe to put in logs.
pub fn token_fingerprint(token: &str) -> String {
    let h = hex::encode(Sha256::digest(token.as_bytes()));
    h[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_verify_roundtrip() {
        let store = AuthStore::empty();
        let (record, token) = generate_key("acme", "web", Role::Writer, None);
        store.insert(record);
        let ctx = store.verify_token(&token).unwrap();
        assert_eq!(ctx.org, "acme");
        assert_eq!(ctx.project.as_deref(), Some("web"));
        assert_eq!(ctx.role, Role::Writer);
    }

    #[test]
    fn tampered_secret_rejected() {
        let store = AuthStore::empty();
        let (record, token) = generate_key("acme", "*", Role::Admin, None);
        store.insert(record);
        let mut bad = token.clone();
        bad.pop();
        bad.push(if token.ends_with('A') { 'B' } else { 'A' });
        assert!(matches!(store.verify_token(&bad), Err(AuthError::UnknownKey)));
        assert!(matches!(store.verify_token("garbage"), Err(AuthError::InvalidToken)));
    }

    #[test]
    fn disabled_key_rejected() {
        let store = AuthStore::empty();
        let (mut record, token) = generate_key("acme", "*", Role::Admin, None);
        record.disabled = true;
        store.insert(record);
        assert!(matches!(store.verify_token(&token), Err(AuthError::Disabled)));
    }

    #[test]
    fn role_ordering() {
        assert!(Role::Admin.allows(Role::Writer));
        assert!(Role::Admin.allows(Role::Reader));
        assert!(Role::Writer.allows(Role::Reader));
        assert!(!Role::Reader.allows(Role::Writer));
        assert!(!Role::Writer.allows(Role::Admin));
    }

    #[test]
    fn scope_enforcement() {
        let ctx = AuthContext {
            key_id: "k".into(),
            org: "acme".into(),
            project: Some("web".into()),
            role: Role::Writer,
        };
        assert!(ctx.authorize("acme", Some("web"), Role::Writer).is_ok());
        assert!(ctx.authorize("acme", Some("web"), Role::Reader).is_ok());
        assert!(ctx.authorize("acme", Some("other"), Role::Reader).is_err());
        assert!(ctx.authorize("evil", Some("web"), Role::Reader).is_err());
        // Org-level operation requires an org-wide key.
        assert!(ctx.authorize("acme", None, Role::Reader).is_err());
        assert!(ctx.authorize("acme", Some("web"), Role::Admin).is_err());
    }

    #[test]
    fn wildcard_org_and_project() {
        let root = AuthContext {
            key_id: "k".into(),
            org: "*".into(),
            project: None,
            role: Role::Admin,
        };
        assert!(root.authorize("anything", Some("p"), Role::Admin).is_ok());
        assert!(root.authorize("anything", None, Role::Admin).is_ok());
    }

    #[test]
    fn key_file_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        let store = AuthStore::empty();
        let (record, token) = generate_key("acme", "web", Role::Reader, Some("ci key".into()));
        store.insert(record);
        store.save(&path).unwrap();

        let loaded = AuthStore::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        let ctx = loaded.verify_token(&token).unwrap();
        assert_eq!(ctx.role, Role::Reader);

        // The file must not contain the plaintext secret.
        let body = std::fs::read_to_string(&path).unwrap();
        let secret_part = token.split('.').nth(1).unwrap();
        assert!(!body.contains(secret_part));
    }

    #[test]
    fn bootstrap_creates_admin_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        let (store, token) = AuthStore::load_or_bootstrap(&path, "default").unwrap();
        let token = token.expect("first run yields a bootstrap token");
        let ctx = store.verify_token(&token).unwrap();
        assert_eq!(ctx.role, Role::Admin);
        assert_eq!(ctx.org, "default");

        let (_store2, token2) = AuthStore::load_or_bootstrap(&path, "default").unwrap();
        assert!(token2.is_none(), "second run must not mint a new token");
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        let f1 = token_fingerprint("shoal_abc.def");
        let f2 = token_fingerprint("shoal_abc.def");
        assert_eq!(f1, f2);
        assert_eq!(f1.len(), 12);
        assert_ne!(f1, token_fingerprint("shoal_abc.xyz"));
    }
}
