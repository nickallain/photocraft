//! A provider's credentials: one API key for most services, a client id and secret for Adobe
//! Firefly. Stored in the OS credential store as one JSON object (see [`Credentials::to_secret`]);
//! never printed.

use serde_json::{Map, Value};

use crate::{ApiKey, GenError, Result};

/// The field name of a single API key.
pub const API_KEY: &str = "api_key";

/// A credential a provider needs, as the settings ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialField {
    /// Stored name (`api_key`, `client_id`).
    pub name: &'static str,
    /// Settings label (`API key`).
    pub label: &'static str,
}

pub const API_KEY_FIELD: CredentialField = CredentialField { name: API_KEY, label: "API key" };

/// Named secrets, in insertion order.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials(Vec<(String, ApiKey)>);

impl Credentials {
    pub fn api_key(key: ApiKey) -> Self {
        Self(vec![(API_KEY.to_owned(), key)])
    }

    /// Adds `field`, replacing an earlier value.
    pub fn insert(&mut self, field: &str, key: ApiKey) {
        match self.0.iter_mut().find(|(name, _)| name == field) {
            Some(slot) => slot.1 = key,
            None => self.0.push((field.to_owned(), key)),
        }
    }

    pub fn get(&self, field: &str) -> Option<&ApiKey> {
        self.0.iter().find(|(name, _)| name == field).map(|(_, k)| k)
    }

    pub fn require(&self, field: &str) -> Result<&ApiKey> {
        self.get(field).ok_or_else(|| GenError::Invalid(format!("the {field} credential is missing")))
    }

    pub fn has_all(&self, fields: &[CredentialField]) -> bool {
        fields.iter().all(|f| self.get(f.name).is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `text` with every credential value replaced, for error messages that echo a request.
    pub fn scrub(&self, text: &str) -> String {
        self.0.iter().fold(text.to_owned(), |t, (_, k)| k.scrub(&t))
    }

    /// The string kept in the OS credential store: a JSON object of field → value.
    pub fn to_secret(&self) -> String {
        let m: Map<String, Value> = self.0.iter().map(|(n, k)| (n.clone(), Value::String(k.expose().to_owned()))).collect();
        Value::Object(m).to_string()
    }

    /// Reads [`Self::to_secret`]'s output, or a plain key saved by an earlier version (read as
    /// `api_key`). `None` when nothing usable is in it.
    pub fn from_secret(secret: &str) -> Option<Self> {
        let t = secret.trim();
        if t.starts_with('{') {
            let Ok(Value::Object(m)) = serde_json::from_str::<Value>(t) else { return None };
            let mut c = Credentials::default();
            for (name, v) in m {
                if let Some(k) = v.as_str().and_then(ApiKey::new) {
                    c.insert(&name, k);
                }
            }
            return (!c.is_empty()).then_some(c);
        }
        ApiKey::new(t).map(Credentials::api_key)
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.0.iter().map(|(n, _)| n.as_str()).collect();
        write!(f, "Credentials({names:?})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> ApiKey {
        ApiKey::new(s).unwrap()
    }

    #[test]
    fn secrets_round_trip_through_json() {
        let mut c = Credentials::default();
        c.insert("client_id", key("id-123"));
        c.insert("client_secret", key("s3cr3t"));
        let back = Credentials::from_secret(&c.to_secret()).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.get("client_secret").unwrap().expose(), "s3cr3t");
    }

    #[test]
    fn a_plain_legacy_secret_is_an_api_key() {
        let c = Credentials::from_secret("  AIza-legacy\n").unwrap();
        assert_eq!(c.require(API_KEY).unwrap().expose(), "AIza-legacy");
        assert!(Credentials::from_secret("   ").is_none());
        assert!(Credentials::from_secret("{not json").is_none());
        assert!(Credentials::from_secret(r#"{"api_key": ""}"#).is_none(), "blank fields are dropped");
        assert!(Credentials::from_secret(r#"{"api_key": 7}"#).is_none(), "non-text fields are dropped");
    }

    #[test]
    fn debug_never_prints_values_and_scrub_removes_every_field() {
        let mut c = Credentials::api_key(key("k-one"));
        c.insert("other", key("k-two"));
        let dbg = format!("{c:?}");
        assert!(dbg.contains("api_key") && dbg.contains("other"), "{dbg}");
        assert!(!dbg.contains("k-one") && !dbg.contains("k-two"), "{dbg}");
        assert_eq!(c.scrub("x k-one y k-two"), "x [key] y [key]");
    }

    #[test]
    fn insert_replaces_and_has_all_checks_fields() {
        let mut c = Credentials::api_key(key("old"));
        c.insert(API_KEY, key("new"));
        assert_eq!(c.require(API_KEY).unwrap().expose(), "new");
        assert!(c.has_all(&[API_KEY_FIELD]));
        let firefly = [CredentialField { name: "client_id", label: "Client ID" }, CredentialField { name: "client_secret", label: "Client secret" }];
        assert!(!c.has_all(&firefly));
        assert!(matches!(c.require("client_id"), Err(GenError::Invalid(_))));
    }
}
