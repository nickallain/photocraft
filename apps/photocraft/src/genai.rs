//! Generative AI services for the desktop app (#41): API keys in the OS credential store and an
//! HTTPS transport for `photocraft-genai` providers. Nothing here runs until the user sets up a
//! provider in Generative AI Settings.

use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use photocraft_genai::{Credentials, GenError, HttpRequest, HttpResponse, HttpTransport, Method};
use photocraft_ui_egui::GenAiServices;

/// The credential store's service name; the account is the provider id (`gemini`).
#[cfg(any(target_os = "macos", target_os = "windows"))]
const SERVICE: &str = "ai.storyteller.photocraft.genai";
/// An image request can take a while (large outputs, busy service).
const TIMEOUT: Duration = Duration::from_secs(180);

/// Blocking HTTPS for provider requests, run from engine background jobs. HTTPS only, no
/// redirects (a redirect could carry the key header to another host), bounded response size.
struct Https {
    agent: ureq::Agent,
}

impl Https {
    fn new() -> Self {
        let config = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).https_only(true).max_redirects(0).http_status_as_error(false).build();
        Self { agent: config.new_agent() }
    }
}

impl HttpTransport for Https {
    fn send(&self, request: HttpRequest) -> photocraft_genai::Result<HttpResponse> {
        let url = request.url.as_str();
        // ureq's error text names the host and the failure, never header values.
        let sent = match request.method {
            Method::Get => {
                let mut call = self.agent.get(url);
                for (name, value) in &request.headers {
                    call = call.header(*name, value.as_str());
                }
                call.call()
            }
            Method::Post => {
                let mut call = self.agent.post(url);
                for (name, value) in &request.headers {
                    call = call.header(*name, value.as_str());
                }
                call.send(&request.body[..])
            }
            Method::Put => {
                let mut call = self.agent.put(url);
                for (name, value) in &request.headers {
                    call = call.header(*name, value.as_str());
                }
                call.send(&request.body[..])
            }
            Method::Delete => {
                let mut call = self.agent.delete(url);
                for (name, value) in &request.headers {
                    call = call.header(*name, value.as_str());
                }
                call.call()
            }
        };
        let mut response = sent.map_err(|e| GenError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_owned()))).collect();
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(photocraft_genai::MAX_RESPONSE as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|e| GenError::Network(format!("reading the response failed: {e}")))?;
        Ok(HttpResponse { status, headers, body })
    }
}

fn build(provider: &str, model: &str, creds: Credentials) -> Result<photocraft_engine::genai_cmds::SharedProvider, String> {
    let spec = photocraft_genai::spec(provider).ok_or_else(|| format!("unknown generative AI provider `{provider}`"))?;
    (spec.build)(model, creds, Arc::new(Https::new())).map_err(|e| e.to_string())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod store {
    fn entry(provider: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(super::SERVICE, provider).map_err(|e| e.to_string())
    }
    pub fn load(provider: &str) -> Result<Option<String>, String> {
        match entry(provider)?.get_password() {
            Ok(k) => Ok(Some(k)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("could not read the API key: {e}")),
        }
    }
    pub fn save(provider: &str, key: &str) -> Result<(), String> {
        entry(provider)?.set_password(key).map_err(|e| format!("could not save the API key: {e}"))
    }
    pub fn delete(provider: &str) -> Result<(), String> {
        match entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("could not remove the API key: {e}")),
        }
    }
    #[cfg(target_os = "macos")]
    pub const NAME: &str = "the macOS Keychain";
    #[cfg(target_os = "windows")]
    pub const NAME: &str = "Windows Credential Manager";
}

/// Elsewhere (Linux, the BSDs) the key lives in process memory only and is asked for again
/// after a restart, rather than written anywhere in plain text.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod store {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn keys() -> &'static Mutex<HashMap<String, String>> {
        static KEYS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        KEYS.get_or_init(Mutex::default)
    }
    pub fn load(provider: &str) -> Result<Option<String>, String> {
        Ok(keys().lock().map_err(|_| "key store unavailable")?.get(provider).cloned())
    }
    pub fn save(provider: &str, key: &str) -> Result<(), String> {
        keys().lock().map_err(|_| "key store unavailable")?.insert(provider.into(), key.into());
        Ok(())
    }
    pub fn delete(provider: &str) -> Result<(), String> {
        keys().lock().map_err(|_| "key store unavailable")?.remove(provider);
        Ok(())
    }
    pub const NAME: &str = "memory until PhotoCraft quits";
}

pub fn services() -> GenAiServices {
    GenAiServices {
        load_key: Box::new(store::load),
        save_key: Box::new(store::save),
        delete_key: Box::new(store::delete),
        provider: Box::new(build),
        key_store_name: store::NAME,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_genai::ApiKey;

    #[test]
    fn builds_registered_providers_and_rejects_unknown_ones() {
        let creds = || photocraft_genai::Credentials::api_key(ApiKey::new("k").unwrap());
        let p = build("gemini", "", creds()).unwrap();
        assert_eq!(p.info().model, photocraft_genai::gemini::DEFAULT_MODEL);
        assert!(build("gemini", "../etc", creds()).is_err());
        assert!(build("other", "", creds()).is_err());
    }

    /// A non-blank `PHOTOCRAFT_LIVE_SECRET` value.
    fn live_secret(env_secret: Option<String>) -> Option<String> {
        env_secret.filter(|v| !v.trim().is_empty())
    }

    /// Credentials for the live harness: `PHOTOCRAFT_LIVE_SECRET` when set, else the saved ones.
    /// Errors never include the secret.
    fn live_credentials(id: &str, env_secret: Option<String>) -> Result<photocraft_genai::Credentials, String> {
        match live_secret(env_secret) {
            Some(secret) => photocraft_genai::Credentials::from_secret(&secret).ok_or_else(|| "PHOTOCRAFT_LIVE_SECRET is not a usable secret".to_string()),
            None => {
                let secret = store::load(id)?.ok_or_else(|| format!("no credentials saved for {id} in Generative AI Settings"))?;
                photocraft_genai::Credentials::from_secret(&secret).ok_or_else(|| "unreadable saved credentials".to_string())
            }
        }
    }

    #[test]
    fn live_credentials_prefer_the_env_secret_and_never_leak_it() {
        // Only the env branch is exercised: the id has nothing saved and the store is not read.
        assert!(live_credentials("no-such-provider", Some("plain-key".into())).is_ok());
        let json = r#"{"client_id":"abc","client_secret":"def"}"#;
        assert!(live_credentials("no-such-provider", Some(json.into())).is_ok());
        // A blank value counts as unset.
        assert_eq!(live_secret(Some("  ".into())), None);
        assert_eq!(live_secret(Some(String::new())), None);
        assert_eq!(live_secret(None), None);
        assert_eq!(live_secret(Some("k".into())).as_deref(), Some("k"));
        // An unusable value is an error that does not repeat it.
        let bad = "{not-json-SECRETVALUE";
        let err = live_credentials("no-such-provider", Some(bad.into())).unwrap_err();
        assert!(!err.contains("SECRETVALUE"), "{err}");
    }

    /// Developer harness for tuning Generative Fill against the real service: runs one fill with
    /// the key saved in Generative AI Settings (or `PHOTOCRAFT_LIVE_SECRET`) and writes what was sent, the raw results and the
    /// finished picture for each variation to a folder. Each run is billed to that key, so it is
    /// ignored unless asked for:
    ///
    /// PHOTOCRAFT_LIVE_IMAGE=photo.png PHOTOCRAFT_LIVE_RECT=x,y,w,h PHOTOCRAFT_GENAI_DUMP=dir
    /// [PHOTOCRAFT_LIVE_PROMPT=…] [PHOTOCRAFT_LIVE_COUNT=1] [PHOTOCRAFT_LIVE_MODEL=…]
    /// [PHOTOCRAFT_LIVE_PROVIDER=<id>]
    /// [PHOTOCRAFT_LIVE_SECRET=<key or JSON> (instead of the saved credentials)]
    /// [PHOTOCRAFT_LIVE_REPLAY=result-1.jpg,… (no request: reuse saved results)]
    /// cargo test -p photocraft --release live_fill -- --ignored --nocapture
    #[test]
    #[ignore = "calls the real provider, billed to the saved key"]
    fn live_fill() {
        use photocraft_engine::genai_cmds::{FILL, SELECT_VARIATION};
        use serde_json::json;
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let image = var("PHOTOCRAFT_LIVE_IMAGE").expect("PHOTOCRAFT_LIVE_IMAGE");
        // The engine's debug dump writes what was sent and the raw results here too.
        let out = std::path::PathBuf::from(var("PHOTOCRAFT_GENAI_DUMP").expect("PHOTOCRAFT_GENAI_DUMP"));
        let rect: Vec<i64> = var("PHOTOCRAFT_LIVE_RECT").expect("PHOTOCRAFT_LIVE_RECT").split(',').map(|v| v.trim().parse().unwrap()).collect();
        let count: u64 = var("PHOTOCRAFT_LIVE_COUNT").map_or(1, |c| c.parse().unwrap());
        let provider: photocraft_engine::genai_cmds::SharedProvider = match var("PHOTOCRAFT_LIVE_REPLAY") {
            // Free: answer with results saved from an earlier run, to tune what happens after.
            Some(files) => Arc::new(Replay { results: files.split(',').map(|f| std::fs::read(f.trim()).unwrap()).collect(), next: Default::default() }),
            None => {
                let id = var("PHOTOCRAFT_LIVE_PROVIDER").unwrap_or_else(|| photocraft_genai::gemini::ID.into());
                let creds = live_credentials(&id, std::env::var("PHOTOCRAFT_LIVE_SECRET").ok()).unwrap();
                build(&id, &var("PHOTOCRAFT_LIVE_MODEL").unwrap_or_default(), creds).unwrap()
            }
        };

        std::fs::create_dir_all(&out).unwrap();
        let mut s = photocraft_engine::Session::new();
        let name = std::path::Path::new(&image).file_name().unwrap().to_string_lossy().into_owned();
        photocraft_engine::file_cmds::open_bytes_as(&mut s, &name, &std::fs::read(&image).unwrap(), None, None).unwrap();
        s.execute("select.rect", json!({"x": rect[0], "y": rect[1], "width": rect[2], "height": rect[3]})).unwrap();
        s.set_image_provider(Some(provider));
        let started = std::time::Instant::now();
        let r = s.execute(FILL, json!({"prompt": var("PHOTOCRAFT_LIVE_PROMPT").unwrap_or_default(), "variations": count})).unwrap();
        eprintln!("fill took {:.1}s", started.elapsed().as_secs_f32());
        let group = r["layer"].as_u64().unwrap();
        let children = s.active().unwrap().doc.layer(photocraft_doc::LayerId(group)).and_then(|l| l.children()).map_or(0, <[_]>::len);
        for i in 0..children {
            s.execute(SELECT_VARIATION, json!({"layer": group, "index": i})).unwrap();
            let flat = photocraft_compose::flatten(&s.active().unwrap().doc);
            let px: Vec<u8> = flat.px.iter().flat_map(|p| p.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)).collect();
            let img =
                photocraft_codecs::Image::from_u8(flat.rect.width() as u32, flat.rect.height() as u32, photocraft_codecs::ChannelLayout::Rgba, px).unwrap();
            let png = photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &Default::default()).unwrap();
            std::fs::write(out.join(format!("final-{}.png", i + 1)), png).unwrap();
        }
        eprintln!("wrote {children} variations to {}", out.display());
    }

    /// Answers edits with saved results in turn.
    struct Replay {
        results: Vec<Vec<u8>>,
        next: std::sync::atomic::AtomicUsize,
    }

    impl photocraft_genai::ImageProvider for Replay {
        fn info(&self) -> photocraft_genai::ProviderInfo {
            photocraft_genai::ProviderInfo { id: "replay", name: "Replay", model: "replay".into(), host: "example.invalid" }
        }
        fn capabilities(&self) -> photocraft_genai::Capabilities {
            photocraft_genai::Capabilities { generate: false, edit: true, variations: false }
        }
        fn edit_aspect(&self, w: u32, h: u32) -> (u32, u32) {
            // The aspect Gemini would be asked for, so replays are placed as the real run was.
            let gemini = photocraft_genai::gemini::GeminiProvider::new(ApiKey::new("k").unwrap(), "", Https::new()).unwrap();
            photocraft_genai::ImageProvider::edit_aspect(&gemini, w, h)
        }
        fn edit(&self, _: &photocraft_genai::EditRequest) -> photocraft_genai::Result<photocraft_genai::GeneratedImage> {
            let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(photocraft_genai::GeneratedImage { bytes: self.results[n % self.results.len()].clone(), mime: "image/jpeg".into() })
        }
    }

    #[test]
    fn the_transport_refuses_plain_http_without_sending() {
        let r = Https::new().send(HttpRequest::post("http://example.invalid/", Vec::new()).header("x-goog-api-key", "secret"));
        let e = r.err().unwrap();
        assert!(matches!(e, GenError::Network(_)), "{e:?}");
        assert!(!e.to_string().contains("secret"));
    }
}
