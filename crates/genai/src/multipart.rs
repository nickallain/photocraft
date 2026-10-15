//! `multipart/form-data` bodies (OpenAI, Stability, Ideogram, Replicate uploads).

use std::hash::BuildHasher;

enum Part {
    Text { name: String, value: String },
    File { name: String, filename: String, mime: String, bytes: Vec<u8> },
}

#[derive(Default)]
pub struct Form {
    parts: Vec<Part>,
}

/// A random 64-bit value from the standard library's per-process hasher seed.
fn nonce() -> u64 {
    std::collections::hash_map::RandomState::new().hash_one(0u8)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

impl Form {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&mut self, name: &str, value: &str) -> &mut Self {
        self.parts.push(Part::Text { name: name.into(), value: value.into() });
        self
    }

    pub fn file(&mut self, name: &str, filename: &str, mime: &str, bytes: &[u8]) -> &mut Self {
        self.parts.push(Part::File { name: name.into(), filename: filename.into(), mime: mime.into(), bytes: bytes.to_vec() });
        self
    }

    /// The `content-type` header value and the body. The boundary is chosen to occur in no part.
    pub fn finish(self) -> (String, Vec<u8>) {
        let mut boundary = format!("photocraft-{:016x}{:016x}", nonce(), nonce());
        while self.parts.iter().any(|p| match p {
            Part::Text { value, .. } => contains(value.as_bytes(), boundary.as_bytes()),
            Part::File { bytes, .. } => contains(bytes, boundary.as_bytes()),
        }) {
            boundary = format!("photocraft-{:016x}{:016x}", nonce(), nonce());
        }
        let mut body = Vec::new();
        for p in &self.parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match p {
                Part::Text { name, value } => {
                    body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
                    body.extend_from_slice(value.as_bytes());
                }
                Part::File { name, filename, mime, bytes } => {
                    body.extend_from_slice(
                        format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n").as_bytes(),
                    );
                    body.extend_from_slice(bytes);
                }
            }
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }
}

/// Splits a body made by [`Form::finish`] back into (name, value) pairs, for tests.
#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
pub fn parts(content_type: &str, body: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let boundary = content_type.split("boundary=").nth(1)?;
    let delim = format!("--{boundary}");
    let text = body;
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let start = rest.windows(delim.len()).position(|w| w == delim.as_bytes())?;
        rest = &rest[start + delim.len()..];
        if rest.starts_with(b"--") {
            return Some(out);
        }
        rest = rest.strip_prefix(b"\r\n")?;
        let head_end = rest.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&rest[..head_end]).into_owned();
        let name = head.split("name=\"").nth(1)?.split('"').next()?.to_owned();
        rest = &rest[head_end + 4..];
        let next = rest.windows(delim.len() + 2).position(|w| w == format!("\r\n{delim}").as_bytes())?;
        out.push((name, rest[..next].to_vec()));
        rest = &rest[next + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_round_trip_with_a_boundary_that_is_in_no_part() {
        let mut f = Form::new();
        f.text("prompt", "a red kayak").file("image", "image.png", "image/png", b"\x89PNG-bytes--with--dashes");
        let (ct, body) = f.finish();
        assert!(ct.starts_with("multipart/form-data; boundary=photocraft-"), "{ct}");
        let parts = parts(&ct, &body).unwrap();
        assert_eq!(parts[0], ("prompt".to_owned(), b"a red kayak".to_vec()));
        assert_eq!(parts[1], ("image".to_owned(), b"\x89PNG-bytes--with--dashes".to_vec()));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("Content-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png"));
        assert!(text.ends_with("--\r\n"));
    }
}
