//! Every provider PhotoCraft can use, for the settings and for building them.

use std::sync::Arc;

use crate::{CredentialField, Credentials, HttpTransport, ImageProvider, Result};

/// Builds a provider from a model id (empty = the first), credentials and the app's transport.
/// Never touches the network.
pub type BuildFn = fn(&str, Credentials, Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>>;

pub struct ProviderSpec {
    /// Stable id kept in preferences and the credential store (`flux-fill.fal`).
    pub id: &'static str,
    /// Display name (`FLUX.1 Fill [pro] · fal`).
    pub name: &'static str,
    /// Host the image, mask and prompt go to (privacy text).
    pub host: &'static str,
    /// Hosts results are downloaded from (privacy text and `fetch::result`).
    pub result_hosts: &'static [&'static str],
    /// (id, label); the first is the default.
    pub models: &'static [(&'static str, &'static str)],
    pub fields: &'static [CredentialField],
    /// Where to get credentials.
    pub key_page: &'static str,
    /// An extra sentence for the privacy text, or "".
    pub note: &'static str,
    pub build: BuildFn,
}

static PROVIDERS: &[ProviderSpec] = &[
    crate::gemini::SPEC,
    crate::providers::openai::SPEC,
    crate::providers::stability::SPEC,
    crate::providers::ideogram::SPEC,
    crate::providers::bria::SPEC,
    crate::providers::bfl::SPEC,
    crate::providers::fal::SPEC,
    crate::providers::replicate::SPEC,
    crate::providers::firefly::SPEC,
];

pub fn providers() -> &'static [ProviderSpec] {
    PROVIDERS
}

pub fn spec(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiKey;
    use crate::testing::{FakeHttp, SECRET};

    /// Credentials with every field the provider asks for.
    fn full(spec: &ProviderSpec) -> Credentials {
        let mut c = Credentials::default();
        for f in spec.fields {
            c.insert(f.name, ApiKey::new(SECRET).unwrap());
        }
        c
    }

    #[test]
    fn ids_are_unique_and_every_spec_builds_as_itself() {
        let mut ids: Vec<&str> = providers().iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), providers().len(), "duplicate provider ids");
        for p in providers() {
            assert!(!p.models.is_empty() && !p.fields.is_empty() && p.key_page.starts_with("https://"), "{}", p.id);
            let built = (p.build)("", full(p), FakeHttp::new(vec![])).unwrap();
            assert_eq!(built.info().id, p.id, "the session keys providers by this id");
            assert!(p.models.iter().any(|m| m.0 == built.info().model), "{}: an empty model picks a listed one", p.id);
            assert!(built.capabilities().edit, "{}", p.id);
            assert!((p.build)("../etc", full(p), FakeHttp::new(vec![])).is_err(), "{}", p.id);
            assert!((p.build)("", Credentials::default(), FakeHttp::new(vec![])).is_err(), "{}: missing credentials", p.id);
            assert_eq!(spec(p.id).map(|s| s.id), Some(p.id));
        }
        assert!(spec("nope").is_none());
    }
}
