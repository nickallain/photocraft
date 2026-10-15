//! Pieces every provider uses: encodings, error mapping, image sniffing, model choice.

use base64::Engine as _;
use serde_json::Value;

use crate::{Credentials, GenError, GeneratedImage, HttpResponse, MAX_RESPONSE, Result, png_size};

/// Sent to instruction-following models (OpenAI) when the user leaves the prompt empty.
pub const CONTINUE_PROMPT: &str = "Remove the masked content and continue the surrounding image naturally.";

pub fn prompt_or_continue(prompt: &str) -> &str {
    let p = prompt.trim();
    if p.is_empty() { CONTINUE_PROMPT } else { p }
}

/// Sent to diffusion fill models (they draw what the prompt describes) when the prompt is empty:
/// a description of plain continuation, never an instruction they could render as text.
pub const CONTINUE_DESCRIPTION: &str = "seamless continuation of the surrounding background, same style and texture";

pub fn prompt_or_description(prompt: &str) -> &str {
    let p = prompt.trim();
    if p.is_empty() { CONTINUE_DESCRIPTION } else { p }
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", b64(bytes))
}

pub fn decode_b64(s: &str) -> Result<Vec<u8>> {
    if s.len() / 4 * 3 > MAX_RESPONSE {
        return Err(GenError::BadResponse("the result is too large"));
    }
    base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|_| GenError::BadResponse("an unreadable image"))
}

/// A returned image, typed by its signature (PNG, JPEG or WebP).
pub fn image(bytes: Vec<u8>) -> Result<GeneratedImage> {
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        return Err(GenError::BadResponse("the result isn't an image"));
    };
    Ok(GeneratedImage { bytes, mime: mime.into() })
}

pub fn json(r: &HttpResponse) -> Result<Value> {
    serde_json::from_slice(&r.body).map_err(|_| GenError::BadResponse("an unreadable reply"))
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s.get(..i).unwrap_or(s)),
        None => s.to_owned(),
    }
}

/// The most of a non-JSON body that is read; it is scrubbed whole, then clipped, so a secret
/// can't be cut in half by the clip and slip past the scrub.
const SCAN_LIMIT: usize = 64 * 1024;

/// The service's own error message, scrubbed and clipped.
pub fn error_text(body: &[u8], creds: &Credentials) -> String {
    let text = match serde_json::from_slice::<Value>(body) {
        Ok(v) => ["/error/message", "/error", "/message", "/detail", "/errors/0", "/error_message"]
            .iter()
            .find_map(|p| v.pointer(p).and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default(),
        Err(_) => String::from_utf8_lossy(body.get(..SCAN_LIMIT).unwrap_or(body)).into_owned(),
    };
    clip(&creds.scrub(text.trim()), 300)
}

/// The error for a non-2xx reply that no provider-specific rule matched.
pub fn http_error(r: &HttpResponse, creds: &Credentials) -> GenError {
    match r.status {
        401 | 403 => GenError::Auth,
        402 => GenError::Quota,
        429 => GenError::RateLimited(r.header("retry-after").and_then(|v| v.trim().parse().ok())),
        status => GenError::Http { status, message: error_text(&r.body, creds) },
    }
}

/// `wanted`, or the first model when empty; an unknown id is an error.
pub fn pick_model(models: &'static [(&'static str, &'static str)], wanted: &str) -> Result<&'static str> {
    if wanted.is_empty() {
        return models.first().map(|m| m.0).ok_or_else(|| GenError::Invalid("no model".into()));
    }
    models.iter().find(|m| m.0 == wanted).map(|m| m.0).ok_or_else(|| GenError::Invalid(format!("unknown model `{wanted}`")))
}

/// The size both PNGs share.
pub fn same_size(image: &[u8], mask: &[u8]) -> Result<(u32, u32)> {
    let size = png_size(image).ok_or_else(|| GenError::Invalid("the image must be a PNG".into()))?;
    if png_size(mask) != Some(size) {
        return Err(GenError::Invalid("the mask must be a PNG of the image's size".into()));
    }
    Ok(size)
}

/// `application/x-www-form-urlencoded`, percent-encoding everything but unreserved characters.
pub fn form_urlencode(pairs: &[(&str, &str)]) -> String {
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

/// Runs `on` (a best-effort remote cancel) when `r` is `Err(Cancelled)`.
pub fn on_cancel<T>(r: Result<T>, on: impl FnOnce()) -> Result<T> {
    if matches!(r, Err(GenError::Cancelled)) {
        on();
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ApiKey, Credentials};
    use serde_json::json;

    fn creds() -> Credentials {
        Credentials::api_key(ApiKey::new(SECRET).unwrap())
    }

    #[test]
    fn statuses_map_to_actionable_errors_with_the_key_scrubbed() {
        let r = |s: u16, v| json_reply(s, v).unwrap();
        assert_eq!(http_error(&r(401, json!({})), &creds()), GenError::Auth);
        assert_eq!(http_error(&r(402, json!({})), &creds()), GenError::Quota);
        let limited = with_header(json_reply(429, json!({})), "Retry-After", "12").unwrap();
        assert_eq!(http_error(&limited, &creds()), GenError::RateLimited(Some(12)));
        let e = http_error(&r(500, json!({"error": {"message": format!("bad key {SECRET}")}})), &creds());
        assert_eq!(e, GenError::Http { status: 500, message: "bad key [key]".into() });
        assert_eq!(error_text(br#"{"errors": ["too big"]}"#, &creds()), "too big");
        assert_eq!(error_text(br#"{"detail": "nope"}"#, &creds()), "nope");
        assert_eq!(error_text(b"plain text", &creds()), "plain text");
    }

    #[test]
    fn retry_after_that_is_not_seconds_is_ignored() {
        for v in ["Wed, 21 Oct 2026 07:28:00 GMT", "soon", "-3", ""] {
            let r = with_header(json_reply(429, json!({})), "retry-after", v).unwrap();
            assert_eq!(http_error(&r, &creds()), GenError::RateLimited(None), "{v}");
        }
    }

    #[test]
    fn images_are_sniffed_and_junk_is_refused() {
        assert_eq!(image(png(1, 1)).unwrap().mime, "image/png");
        assert_eq!(image(vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0]).unwrap().mime, "image/jpeg");
        assert_eq!(image(b"RIFF\0\0\0\0WEBPVP8 ".to_vec()).unwrap().mime, "image/webp");
        assert!(matches!(image(b"<html>".to_vec()), Err(GenError::BadResponse(_))));
        assert!(matches!(decode_b64("@@@"), Err(GenError::BadResponse(_))));
    }

    #[test]
    fn models_sizes_prompts_and_forms() {
        const M: &[(&str, &str)] = &[("a", "A"), ("b", "B")];
        assert_eq!(pick_model(M, "").unwrap(), "a");
        assert_eq!(pick_model(M, "b").unwrap(), "b");
        assert!(pick_model(M, "../etc").is_err());
        assert_eq!(same_size(&png(4, 3), &mask_png(4, 3)).unwrap(), (4, 3));
        assert!(same_size(&png(4, 3), &mask_png(4, 4)).is_err());
        assert!(same_size(b"jpeg", &mask_png(4, 4)).is_err());
        assert_eq!(prompt_or_continue("  "), CONTINUE_PROMPT);
        assert_eq!(prompt_or_continue(" a boat "), "a boat");
        assert_eq!(prompt_or_description("  "), CONTINUE_DESCRIPTION);
        assert_eq!(prompt_or_description(" a boat "), "a boat");
        assert_eq!(form_urlencode(&[("a", "x y"), ("scope", "openid,AdobeID")]), "a=x%20y&scope=openid%2CAdobeID");
    }

    #[test]
    fn a_long_secret_straddling_the_clip_point_is_still_scrubbed() {
        let secret = "k".repeat(600);
        let long = Credentials::api_key(ApiKey::new(&secret).unwrap());
        let text = format!("{}{secret}{}", "x".repeat(250), "y".repeat(50));
        let json_body = json!({"error": {"message": text}}).to_string();
        for body in [json_body.as_bytes(), text.as_bytes()] {
            let out = error_text(body, &long);
            assert!(!out.contains(&secret[..20]), "a fragment of the secret leaked");
            assert!(out.contains("[key]"));
        }
    }
}
