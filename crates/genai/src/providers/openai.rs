//! OpenAI GPT Image edits (`POST /v1/images/edits`): multipart, the mask is the image with the
//! area to change made fully transparent, the result comes back inline as base64.

use std::sync::Arc;

use crate::providers::common::{self, http_error, json, pick_model, prompt_or_continue, same_size};
use crate::{
    API_KEY, API_KEY_FIELD, Capabilities, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider,
    ProviderInfo, ProviderSpec, Result, check_prompt, mask, multipart::Form, nearest_ratio,
};

pub const ID: &str = "openai";
const NAME: &str = "OpenAI GPT Image";
const HOST: &str = "api.openai.com";
pub(crate) const ENDPOINT: &str = "https://api.openai.com/v1/images/edits";
/// Added to a prompt: GPT Image otherwise changes the image outside the mask too (6/9/3 % of it in
/// one test, 1/3/4 % with this). Never say "transparent": it drew a transparency checkerboard.
const INSIDE_HINT: &str = "Keep all new content entirely inside the masked area and leave everything outside it unchanged.";
/// Added when a reference image is sent as the second image.
pub const REFERENCE_HINT: &str =
    "The second image is a reference: show that same subject — same face, hair and clothing — in the masked area of the first image.";
/// The request when there is a reference but no prompt.
pub const REFERENCE_DEFAULT: &str = "Add the subject from the second image into the masked area.";
const MODELS: &[(&str, &str)] = &[
    ("gpt-image-2", "GPT Image 2"),
    ("gpt-image-2.5-flare", "GPT Image 2.5 Flare"),
    ("gpt-image-1.5", "GPT Image 1.5"),
    ("gpt-image-1", "GPT Image 1"),
    ("gpt-image-1-mini", "GPT Image 1 Mini"),
];
const FIXED: &[(u32, u32)] = &[(1024, 1024), (1536, 1024), (1024, 1536)];
const MAX_LONG: f64 = 2560.0;
const MAX_PIXELS: f64 = 2560.0 * 1440.0;
const MIN_LONG: f64 = 1024.0;

pub struct OpenAi {
    model: &'static str,
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(API_KEY)?;
    Ok(Arc::new(OpenAi { model: pick_model(MODELS, model)?, creds, http }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: &[],
    models: MODELS,
    fields: &[API_KEY_FIELD],
    key_page: "https://platform.openai.com/api-keys",
    note: "",
    build,
};

fn custom_sizes(model: &str) -> bool {
    model.starts_with("gpt-image-2")
}

/// The size an edit of a `w`×`h` image is asked for.
pub fn output_size(model: &str, w: u32, h: u32) -> (u32, u32) {
    if !custom_sizes(model) {
        return nearest_ratio(w, h, FIXED).unwrap_or((1024, 1024));
    }
    let ratio = (f64::from(w.max(1)) / f64::from(h.max(1))).clamp(1.0 / 3.0, 3.0);
    let (mut ow, mut oh) = if ratio >= 1.0 { (f64::from(w.max(h)), f64::from(w.max(h)) / ratio) } else { (f64::from(w.max(h)) * ratio, f64::from(w.max(h))) };
    let long = ow.max(oh);
    let up = (MIN_LONG / long).max(1.0);
    ow *= up;
    oh *= up;
    let down = (MAX_LONG / ow.max(oh)).min((MAX_PIXELS / (ow * oh)).sqrt()).min(1.0);
    ow *= down;
    oh *= down;
    let snap = |v: f64| ((v / 16.0).floor() as u32).max(1) * 16;
    let (mut sw, mut sh) = (snap(ow), snap(oh));
    // Snapping can push a 3:1 shape just past 3:1.
    while sw > sh.saturating_mul(3) && sw > 16 {
        sw -= 16;
    }
    while sh > sw.saturating_mul(3) && sh > 16 {
        sh -= 16;
    }
    (sw, sh)
}

/// The prompt sent: the user's with [`INSIDE_HINT`] after it (and [`REFERENCE_HINT`] before that
/// when there is a reference), or with none the request to continue the surroundings or, with a
/// reference, to add its subject.
fn prompt(p: &str, with_reference: bool) -> String {
    let p = p.trim();
    match (p.is_empty(), with_reference) {
        (true, false) => prompt_or_continue(p).to_owned(),
        (true, true) => format!("{REFERENCE_DEFAULT} {REFERENCE_HINT} {INSIDE_HINT}"),
        (false, true) => format!("{p} {REFERENCE_HINT} {INSIDE_HINT}"),
        (false, false) => format!("{p} {INSIDE_HINT}"),
    }
}

/// The reply: inline base64 PNG, or the service's error.
pub fn parse(r: &HttpResponse, creds: &Credentials) -> Result<GeneratedImage> {
    if !r.is_success() {
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
        if v.pointer("/error/code").and_then(serde_json::Value::as_str) == Some("moderation_blocked") {
            return Err(GenError::Moderated(common::error_text(&r.body, creds)));
        }
        return Err(http_error(r, creds));
    }
    let v = json(r)?;
    let b = v.pointer("/data/0/b64_json").and_then(serde_json::Value::as_str).ok_or(GenError::BadResponse("no image in the reply"))?;
    common::image(common::decode_b64(b)?)
}

impl ImageProvider for OpenAi {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: ID, name: NAME, model: self.model.into(), host: HOST }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { generate: false, edit: true, variations: false }
    }
    fn edit_aspect(&self, w: u32, h: u32) -> (u32, u32) {
        output_size(self.model, w, h)
    }
    fn min_side(&self) -> u32 {
        MIN_LONG as u32
    }
    fn supports_reference(&self) -> bool {
        true
    }
    fn edit(&self, r: &EditRequest) -> Result<GeneratedImage> {
        crate::providers::common::check_reference(self, r)?;
        check_prompt(&r.prompt, false)?;
        let (w, h) = same_size(&r.image, &r.mask)?;
        r.cancel.check()?;
        let hole = mask::alpha_hole(&r.image, &r.mask)?;
        let (ow, oh) = output_size(self.model, w, h);
        let mut form = Form::new();
        form.text("model", self.model)
            .text("prompt", &prompt(&r.prompt, r.reference.is_some()))
            .text("size", &format!("{ow}x{oh}"))
            .text("n", "1")
            .text("output_format", "png");
        if matches!(self.model, "gpt-image-1" | "gpt-image-1.5") {
            form.text("input_fidelity", "high");
        }
        match &r.reference {
            Some(reference) => {
                form.file("image[]", "image.png", "image/png", &r.image).file("image[]", "reference.png", "image/png", reference);
            }
            None => {
                form.file("image", "image.png", "image/png", &r.image);
            }
        }
        form.file("mask", "mask.png", "image/png", &hole);
        let (content_type, body) = form.finish();
        let key = self.creds.require(API_KEY)?;
        let reply = self
            .http
            .send(HttpRequest::post(ENDPOINT, body).header("authorization", format!("Bearer {}", key.expose())).header("content-type", content_type))?;
        r.cancel.check()?;
        parse(&reply, &self.creds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ApiKey, Method};
    use serde_json::json;

    fn provider(model: &str, replies: Vec<Result<HttpResponse>>) -> (std::sync::Arc<dyn ImageProvider>, std::sync::Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        let p = (SPEC.build)(model, Credentials::api_key(ApiKey::new(SECRET).unwrap()), http.clone()).unwrap();
        (p, http)
    }

    fn ok() -> Result<HttpResponse> {
        json_reply(200, json!({"data": [{"b64_json": common::b64(&png(4, 4))}]}))
    }

    fn part(r: &crate::HttpRequest, name: &str) -> Option<Vec<u8>> {
        let parts = crate::multipart::parts(header(r, "content-type").unwrap(), &r.body).unwrap();
        parts.into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    fn parts(r: &crate::HttpRequest) -> Vec<(String, Vec<u8>)> {
        crate::multipart::parts(header(r, "content-type").unwrap(), &r.body).unwrap()
    }

    #[test]
    fn a_reference_goes_second_as_image_array_parts() {
        let (p, http) = provider("", vec![ok()]);
        let mut r = edit_request("add my friend on the left", 64, 32);
        r.reference = Some(png(10, 12));
        p.edit(&r).unwrap();
        let ps = parts(&http.requests()[0]);
        let images: Vec<&Vec<u8>> = ps.iter().filter(|(n, _)| n == "image[]").map(|(_, v)| v).collect();
        assert_eq!(images.len(), 2);
        assert_eq!(crate::png_size(images[0]), Some((64, 32)), "the photo first");
        assert_eq!(crate::png_size(images[1]), Some((10, 12)), "the reference second");
        assert!(ps.iter().all(|(n, _)| n != "image"), "no single `image` part with a reference");
        let prompt = String::from_utf8(part(&http.requests()[0], "prompt").unwrap()).unwrap();
        assert!(prompt.starts_with("add my friend on the left") && prompt.contains(REFERENCE_HINT) && prompt.ends_with(INSIDE_HINT), "{prompt}");
        assert!(!prompt.contains("transparent"));
        assert!(p.supports_reference());
    }

    #[test]
    fn an_empty_prompt_with_a_reference_asks_to_add_the_subject() {
        let (p, http) = provider("", vec![ok()]);
        let mut r = edit_request("", 16, 16);
        r.reference = Some(png(8, 8));
        p.edit(&r).unwrap();
        let prompt = String::from_utf8(part(&http.requests()[0], "prompt").unwrap()).unwrap();
        assert!(prompt.starts_with(REFERENCE_DEFAULT), "{prompt}");
    }

    #[test]
    fn without_a_reference_the_request_is_unchanged() {
        let (p, http) = provider("", vec![ok()]);
        p.edit(&edit_request("a kayak", 16, 16)).unwrap();
        let ps = parts(&http.requests()[0]);
        assert_eq!(ps.iter().filter(|(n, _)| n == "image").count(), 1);
        assert!(ps.iter().all(|(n, _)| n != "image[]"));
    }

    #[test]
    fn input_fidelity_only_where_supported() {
        for (model, sent) in [("gpt-image-1", true), ("gpt-image-1.5", true), ("gpt-image-2", false), ("gpt-image-1-mini", false)] {
            let (p, http) = provider(model, vec![ok()]);
            p.edit(&edit_request("x", 16, 16)).unwrap();
            assert_eq!(part(&http.requests()[0], "input_fidelity").is_some(), sent, "{model}");
        }
    }

    #[test]
    fn sends_the_image_and_a_transparent_hole_mask_with_a_bearer_key() {
        let (p, http) = provider("", vec![ok()]);
        let out = p.edit(&edit_request("a kayak", 64, 32)).unwrap();
        assert_eq!(out.mime, "image/png");
        let r = &http.requests()[0];
        assert_eq!((r.method, r.url.as_str()), (Method::Post, ENDPOINT));
        assert!(!r.url.contains(SECRET));
        assert_eq!(header(r, "authorization"), Some(format!("Bearer {SECRET}").as_str()));
        assert_eq!(part(r, "model").unwrap(), b"gpt-image-2");
        assert_eq!(part(r, "prompt").unwrap(), format!("a kayak {INSIDE_HINT}").as_bytes());
        assert_eq!(part(r, "output_format").unwrap(), b"png");
        assert!(part(r, "input_fidelity").is_none(), "gpt-image-2 rejects input_fidelity");
        let (w, h, px) = crate::mask::decode_rgba8(&part(r, "mask").unwrap()).unwrap();
        assert_eq!((w, h), (64, 32));
        assert_eq!(px[3], 0, "left half (selected): transparent = edit");
        assert_eq!(px[(63 * 4) + 3], 255, "right half: kept");
    }

    #[test]
    fn older_models_get_fixed_sizes_and_high_input_fidelity() {
        let (p, http) = provider("gpt-image-1", vec![ok()]);
        p.edit(&edit_request("x", 300, 200)).unwrap();
        let r = &http.requests()[0];
        assert_eq!(part(r, "size").unwrap(), b"1536x1024");
        assert_eq!(part(r, "input_fidelity").unwrap(), b"high");
        assert_eq!(p.edit_aspect(300, 200), (1536, 1024));
    }

    #[test]
    fn new_models_take_the_shape_in_multiples_of_16_within_limits() {
        assert_eq!(output_size("gpt-image-2", 1024, 1024), (1024, 1024));
        let (w, h) = output_size("gpt-image-2", 4000, 500);
        assert!(w % 16 == 0 && h % 16 == 0, "{w}x{h}");
        assert!(f64::from(w) / f64::from(h) <= 3.0 + 1e-9, "ratio clamped to 3:1");
        assert!(w <= 2560 && u64::from(w) * u64::from(h) <= 2560 * 1440);
        let (w, h) = output_size("gpt-image-2", 40, 30);
        assert!(w.max(h) >= 1024, "small areas are asked for at a usable size: {w}x{h}");
    }

    #[test]
    fn an_empty_prompt_asks_to_continue_the_surroundings() {
        let (p, http) = provider("", vec![ok()]);
        p.edit(&edit_request("  ", 32, 32)).unwrap();
        assert_eq!(part(&http.requests()[0], "prompt").unwrap(), common::CONTINUE_PROMPT.as_bytes());
        assert!(!p.needs_prompt());
        assert_eq!(p.min_side(), 1024);
    }

    #[test]
    fn a_prompt_is_sent_with_the_hint_to_stay_inside_the_mask() {
        let (p, http) = provider("", vec![ok()]);
        p.edit(&edit_request("  a red kite ", 32, 32)).unwrap();
        let sent = String::from_utf8(part(&http.requests()[0], "prompt").unwrap()).unwrap();
        assert!(sent.starts_with("a red kite "), "{sent}");
        assert!(sent.ends_with(INSIDE_HINT), "{sent}");
    }

    #[test]
    fn the_inside_hint_never_says_transparent() {
        // "transparent" made OpenAI draw a transparency checkerboard into the fill.
        assert!(!INSIDE_HINT.to_lowercase().contains("transparent"));
        assert!(!INSIDE_HINT.is_empty());
    }

    #[test]
    fn errors_map_and_never_carry_the_key() {
        let moderated = json_reply(400, json!({"error": {"code": "moderation_blocked", "message": "Your request was rejected"}}));
        let (p, _) = provider("", vec![moderated]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, _) = provider("", vec![json_reply(401, json!({"error": {"message": format!("Incorrect API key {SECRET}")}}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Auth));
        let (p, _) = provider("", vec![json_reply(500, json!({"error": {"message": format!("boom {SECRET}")}}))]);
        let e = p.edit(&edit_request("x", 8, 8)).unwrap_err().to_string();
        assert!(e.contains("boom") && !e.contains(SECRET), "{e}");
        let (p, _) = provider("", vec![json_reply(200, json!({"data": []}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
    }

    #[test]
    fn bad_input_fails_before_sending() {
        let (p, http) = provider("", vec![]);
        let mut r = edit_request("x", 8, 8);
        r.mask = mask_png(9, 8);
        assert!(matches!(p.edit(&r), Err(GenError::Invalid(_))));
        assert!(http.requests().is_empty());
    }
}
