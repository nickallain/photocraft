//! Generative image providers (Generative Fill and friends, issue #41).
//!
//! [`ImageProvider`] is the one interface the engine talks to; each backend (Gemini today, others
//! later) is a separate implementation and none is privileged. Providers never open a socket
//! themselves: every request goes through an [`HttpTransport`] the app injects, so this crate (and
//! the engine above it) stays network-free, builds for wasm, and tests run against a fake.
//!
//! Images cross this interface as encoded bytes (PNG in, PNG or JPEG out) in sRGB. Masks are PNGs
//! of the same size as their image: white marks the pixels to change, black the pixels to keep.
//!
//! Credentials: an [`ApiKey`] never prints (its `Debug` is redacted) and no error message carries
//! a key or a response body; a service's own error text is passed through only after any copy of
//! the key is scrubbed from it.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod credentials;
pub mod gemini;
pub use credentials::*;
mod registry;
pub use registry::*;
pub mod fetch;
pub mod mask;
pub mod multipart;
pub mod poll;
pub mod providers;
#[cfg(test)]
pub(crate) mod testing;

/// Largest response body a provider accepts (a 4K PNG is well under this).
pub const MAX_RESPONSE: usize = 48 * 1024 * 1024;
/// Largest prompt, in bytes.
pub const MAX_PROMPT: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GenError {
    #[error("{0} can't do that")]
    Unsupported(&'static str),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("the API key was rejected; check it in Generative AI Settings")]
    Auth,
    #[error("the image service account is out of quota or credits; check your plan")]
    Quota,
    #[error("the image service's rate limit was reached; try again {}", retry_text(.0))]
    RateLimited(Option<u64>),
    #[error("the image service declined this request under its content policy: {0}")]
    Moderated(String),
    #[error("the image service didn't finish in time")]
    Timeout,
    #[error("the image service declined this request: {0}")]
    Refused(String),
    #[error("the image service failed (HTTP {status}): {message}")]
    Http { status: u16, message: String },
    #[error("could not reach the image service: {0}")]
    Network(String),
    #[error("the image service sent a response PhotoCraft can't read: {0}")]
    BadResponse(&'static str),
    #[error("cancelled")]
    Cancelled,
}

fn retry_text(seconds: &Option<u64>) -> String {
    match seconds {
        Some(s) => format!("in {s} s"),
        None => "later".to_owned(),
    }
}

/// Asks a running request to stop. Polling providers check it every tick; a request already on
/// the wire finishes and its result is dropped.
#[derive(Clone, Default)]
pub struct CancelToken(Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>);

impl CancelToken {
    pub fn new(cancelled: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Some(std::sync::Arc::new(cancelled)))
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.as_ref().is_some_and(|f| f())
    }
    /// `Err(Cancelled)` once cancelled, for `?` between steps.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(GenError::Cancelled) } else { Ok(()) }
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CancelToken")
    }
}

pub type Result<T> = std::result::Result<T, GenError>;

/// A secret in process memory. It never prints; read it only to build a request.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// `None` for a blank key. Surrounding whitespace (a pasted newline) is dropped.
    pub fn new(key: &str) -> Option<ApiKey> {
        let k = key.trim();
        (!k.is_empty()).then(|| ApiKey(k.to_owned()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// `text` with every copy of the key replaced, for error messages that echo a request.
    pub fn scrub(&self, text: &str) -> String {
        text.replace(&self.0, "[key]")
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(…)")
    }
}

/// HTTP method of a provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

/// One HTTPS request. Header values may hold a credential, so a transport must not log them (and
/// this type deliberately has no `Debug`).
#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self { method, url: url.into(), headers: Vec::new(), body: Vec::new() }
    }
    pub fn get(url: impl Into<String>) -> Self {
        Self::new(Method::Get, url)
    }
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self { body, ..Self::new(Method::Post, url) }
    }
    pub fn put(url: impl Into<String>) -> Self {
        Self::new(Method::Put, url)
    }
    pub fn delete(url: impl Into<String>) -> Self {
        Self::new(Method::Delete, url)
    }
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    /// Sets a JSON body and its content type.
    pub fn json(self, body: &serde_json::Value) -> Self {
        let mut r = self.header("content-type", "application/json");
        r.body = body.to_string().into_bytes();
        r
    }
}

/// Any HTTP status comes back as a response (the provider reads the service's error body); only a
/// failure to talk to the server at all is an `Err`. Header names are lower-case.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A response header by name, ignoring case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The network seam. The desktop app implements it over HTTPS; tests use a fake; the web build
/// has none, so generative commands are simply unavailable there.
pub trait HttpTransport: Send + Sync {
    /// Blocking: called from an engine background job, never the UI thread.
    fn send(&self, request: HttpRequest) -> Result<HttpResponse>;
}

impl<T: HttpTransport + ?Sized> HttpTransport for std::sync::Arc<T> {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        (**self).send(request)
    }
}

/// What a provider can do; the UI and the engine's `enabled` checks read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub generate: bool,
    pub edit: bool,
    pub variations: bool,
}

/// Who a request goes to, for the privacy notice and error messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Stable id stored in preferences (`gemini`).
    pub id: &'static str,
    /// Display name (`Google Gemini`).
    pub name: &'static str,
    /// Model the requests use.
    pub model: String,
    /// Host the image, mask and prompt are sent to.
    pub host: &'static str,
}

/// Output size class. Providers map it to their nearest supported size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageSize {
    /// About 1024 px on the long side.
    #[default]
    Small,
    /// About 2048 px.
    Medium,
    /// About 4096 px.
    Large,
}

impl ImageSize {
    /// The class that covers `long_side` pixels without upscaling much.
    pub fn for_long_side(long_side: u32) -> ImageSize {
        match long_side {
            0..=1200 => ImageSize::Small,
            1201..=2400 => ImageSize::Medium,
            _ => ImageSize::Large,
        }
    }
}

/// Text to image.
#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub prompt: String,
    /// Wanted output width and height (only their ratio matters).
    pub aspect: (u32, u32),
    pub size: ImageSize,
}

/// Change the masked part of `image` (Generative Fill). The engine sends `image` with the masked
/// area already cleared to flat grey, so the provider fills a hole rather than repainting what
/// was there. An empty prompt asks it to continue the surroundings through the hole.
#[derive(Debug, Clone, Default)]
pub struct EditRequest {
    pub image: Vec<u8>,
    pub mask: Vec<u8>,
    pub prompt: String,
    pub size: ImageSize,
    /// One reference image (PNG, sRGB, long side ≤ 1024) of a subject to put in the fill, for
    /// providers whose `supports_reference()` is true.
    pub reference: Option<Vec<u8>>,
    /// The engine sets it from the job; the default never cancels.
    pub cancel: CancelToken,
}

/// Another take on a whole image, optionally steered by a prompt.
#[derive(Debug, Clone)]
pub struct VariationRequest {
    pub image: Vec<u8>,
    pub prompt: String,
    pub size: ImageSize,
}

/// An encoded image a provider returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    /// `image/png` or `image/jpeg`.
    pub mime: String,
}

/// A generative image backend. Methods block (they run in a background job) and default to
/// [`GenError::Unsupported`], so a backend implements only what its [`Capabilities`] report.
pub trait ImageProvider: Send + Sync {
    fn info(&self) -> ProviderInfo;
    fn capabilities(&self) -> Capabilities;
    /// The width:height an edit of a `w`×`h` image comes back at. Providers with a fixed set of
    /// output shapes return the nearest one, so the caller can grow its context area to match
    /// instead of stretching the result.
    fn edit_aspect(&self, w: u32, h: u32) -> (u32, u32) {
        (w, h)
    }
    /// Longest side an edit's image may have; the engine scales its upload to fit.
    fn max_side(&self) -> u32 {
        2048
    }
    /// Shortest long side an edit's image may have; smaller uploads are scaled up to it.
    fn min_side(&self) -> u32 {
        0
    }
    /// Whether an edit needs a prompt (no promptless "continue the image" mode).
    fn needs_prompt(&self) -> bool {
        false
    }
    /// Whether an edit can use `EditRequest::reference`.
    fn supports_reference(&self) -> bool {
        false
    }
    fn generate(&self, _request: &GenerateRequest) -> Result<GeneratedImage> {
        Err(GenError::Unsupported(self.info().name))
    }
    fn edit(&self, _request: &EditRequest) -> Result<GeneratedImage> {
        Err(GenError::Unsupported(self.info().name))
    }
    fn variations(&self, _request: &VariationRequest) -> Result<GeneratedImage> {
        Err(GenError::Unsupported(self.info().name))
    }
}

/// Checks shared by every provider, run before anything is sent.
pub fn check_prompt(prompt: &str, required: bool) -> Result<()> {
    if prompt.len() > MAX_PROMPT {
        return Err(GenError::Invalid(format!("the prompt is longer than {MAX_PROMPT} bytes")));
    }
    if required && prompt.trim().is_empty() {
        return Err(GenError::Invalid("the prompt is empty".into()));
    }
    Ok(())
}

/// The entry of `ratios` (width, height) closest to `w`×`h`, compared in log space so 2:1 and
/// 1:2 are equally far from 1:1.
pub fn nearest_ratio(w: u32, h: u32, ratios: &[(u32, u32)]) -> Option<(u32, u32)> {
    let target = (f64::from(w.max(1)) / f64::from(h.max(1))).ln();
    ratios.iter().copied().filter(|&(a, b)| a > 0 && b > 0).min_by(|&(a, b), &(c, d)| {
        let da = ((f64::from(a) / f64::from(b)).ln() - target).abs();
        let dc = ((f64::from(c) / f64::from(d)).ln() - target).abs();
        da.total_cmp(&dc)
    })
}

/// Width and height of a PNG from its header, without decoding it.
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || !bytes.starts_with(SIG) || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_build_with_method_headers_and_json() {
        let r = HttpRequest::post("https://h.example/x", Vec::new()).header("x-key", "v").json(&serde_json::json!({"a": 1}));
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.url, "https://h.example/x");
        assert!(r.headers.contains(&("x-key", "v".to_owned())));
        assert!(r.headers.contains(&("content-type", "application/json".to_owned())));
        assert_eq!(r.body, br#"{"a":1}"#);
        assert_eq!(HttpRequest::get("https://h.example/").method, Method::Get);
        assert_eq!(HttpRequest::put("https://h.example/").method, Method::Put);
        let d = HttpRequest::delete("https://h.example/f");
        assert_eq!((d.method, d.url.as_str(), d.body.is_empty()), (Method::Delete, "https://h.example/f", true));
    }

    #[test]
    fn response_headers_are_case_insensitive() {
        let r = HttpResponse { status: 429, headers: vec![("Retry-After".into(), "7".into())], body: Vec::new() };
        assert_eq!(r.header("retry-after"), Some("7"));
        assert_eq!(r.header("x-missing"), None);
        assert!(!r.is_success());
        assert!(HttpResponse { status: 204, headers: Vec::new(), body: Vec::new() }.is_success());
    }

    #[test]
    fn an_arc_of_a_transport_is_a_transport() {
        struct Echo;
        impl HttpTransport for Echo {
            fn send(&self, r: HttpRequest) -> Result<HttpResponse> {
                Ok(HttpResponse { status: 200, headers: Vec::new(), body: r.url.into_bytes() })
            }
        }
        let shared: std::sync::Arc<dyn HttpTransport> = std::sync::Arc::new(Echo);
        assert_eq!(shared.send(HttpRequest::get("https://a/")).unwrap().body, b"https://a/");
    }

    #[test]
    fn api_keys_never_print_and_are_scrubbed() {
        let k = ApiKey::new("  sk-secret-123\n").unwrap();
        assert_eq!(k.expose(), "sk-secret-123");
        assert!(!format!("{k:?}").contains("secret"));
        assert_eq!(k.scrub("bad key sk-secret-123!"), "bad key [key]!");
        assert!(ApiKey::new(" \n").is_none());
    }

    #[test]
    fn nearest_ratio_is_symmetric_in_log_space() {
        let r = [(1, 1), (16, 9), (9, 16), (4, 3), (3, 4)];
        assert_eq!(nearest_ratio(1000, 1000, &r), Some((1, 1)));
        assert_eq!(nearest_ratio(1920, 1080, &r), Some((16, 9)));
        assert_eq!(nearest_ratio(1080, 1920, &r), Some((9, 16)));
        assert_eq!(nearest_ratio(400, 310, &r), Some((4, 3)));
        assert_eq!(nearest_ratio(0, 0, &r), Some((1, 1)));
        assert_eq!(nearest_ratio(5, 5, &[]), None);
    }

    #[test]
    fn prompt_and_size_checks() {
        assert!(check_prompt("", false).is_ok());
        assert!(check_prompt(" ", true).is_err());
        assert!(check_prompt(&"x".repeat(MAX_PROMPT + 1), false).is_err());
        assert_eq!(ImageSize::for_long_side(900), ImageSize::Small);
        assert_eq!(ImageSize::for_long_side(2000), ImageSize::Medium);
        assert_eq!(ImageSize::for_long_side(5000), ImageSize::Large);
    }

    #[test]
    fn png_size_reads_the_header_and_rejects_junk() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((640, 480)));
        assert_eq!(png_size(b"GIF89a"), None);
        assert_eq!(png_size(&png[..20]), None);
    }

    #[test]
    fn cancel_tokens_read_their_source_and_default_to_never() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        let t = CancelToken::new(move || f.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!t.is_cancelled());
        assert!(t.check().is_ok());
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(t.clone().is_cancelled());
        assert_eq!(t.check(), Err(GenError::Cancelled));
        assert!(!CancelToken::default().is_cancelled());
        assert_eq!(format!("{:?}", EditRequest::default().cancel), "CancelToken");
    }

    #[test]
    fn new_errors_read_well() {
        assert!(GenError::RateLimited(Some(12)).to_string().contains("in 12 s"));
        assert!(GenError::RateLimited(None).to_string().contains("later"));
        assert!(GenError::Moderated("nope".into()).to_string().contains("nope"));
        assert!(GenError::Timeout.to_string().contains("in time"));
        let quota = GenError::Quota.to_string();
        assert!(quota.contains("out of quota or credits") && !quota.contains("rate limit"), "429 is RateLimited now: {quota}");
    }
}
