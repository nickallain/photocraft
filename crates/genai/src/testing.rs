//! Shared test helpers: a scripted transport and tiny PNGs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::{EditRequest, GenError, HttpRequest, HttpResponse, HttpTransport, ImageSize, Result};

pub const SECRET: &str = "sk-test-secret-0123";

/// Answers requests from a script, in order, and keeps every request it was sent.
pub struct FakeHttp {
    replies: Mutex<VecDeque<Result<HttpResponse>>>,
    sent: Mutex<Vec<HttpRequest>>,
}

impl FakeHttp {
    pub fn new(replies: Vec<Result<HttpResponse>>) -> Arc<FakeHttp> {
        Arc::new(FakeHttp { replies: Mutex::new(replies.into()), sent: Mutex::new(Vec::new()) })
    }
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.sent.lock().unwrap().clone()
    }
}

impl HttpTransport for FakeHttp {
    fn send(&self, r: HttpRequest) -> Result<HttpResponse> {
        self.sent.lock().unwrap().push(r);
        self.replies.lock().unwrap().pop_front().unwrap_or_else(|| Err(GenError::Network("the script ran out of replies".into())))
    }
}

pub fn json_reply(status: u16, v: Value) -> Result<HttpResponse> {
    Ok(HttpResponse { status, headers: vec![("content-type".into(), "application/json".into())], body: v.to_string().into_bytes() })
}

pub fn bytes_reply(status: u16, content_type: &str, body: Vec<u8>) -> Result<HttpResponse> {
    Ok(HttpResponse { status, headers: vec![("content-type".into(), content_type.into())], body })
}

pub fn with_header(r: Result<HttpResponse>, name: &str, value: &str) -> Result<HttpResponse> {
    r.map(|mut r| {
        r.headers.push((name.to_ascii_lowercase(), value.into()));
        r
    })
}

fn encode(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(color);
    e.set_depth(png::BitDepth::Eight);
    let mut wr = e.write_header().unwrap();
    wr.write_image_data(data).unwrap();
    wr.finish().unwrap();
    out
}

/// An opaque mid-grey RGBA PNG.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    encode(w, h, png::ColorType::Rgba, &(0..w * h).flat_map(|_| [128u8, 128, 128, 255]).collect::<Vec<_>>())
}

/// A mask as the engine sends it (RGBA grey, opaque): the left half white (fill), the rest black.
pub fn mask_png(w: u32, h: u32) -> Vec<u8> {
    let px: Vec<u8> = (0..h).flat_map(|_| (0..w).flat_map(move |x| if x < w / 2 { [255u8, 255, 255, 255] } else { [0, 0, 0, 255] })).collect();
    encode(w, h, png::ColorType::Rgba, &px)
}

pub fn edit_request(prompt: &str, w: u32, h: u32) -> EditRequest {
    EditRequest { image: png(w, h), mask: mask_png(w, h), prompt: prompt.into(), size: ImageSize::Small, ..Default::default() }
}

pub fn header<'a>(r: &'a HttpRequest, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Never sleeps; `sleep` moves time on.
#[derive(Default)]
pub struct FakeClock {
    now: Mutex<Duration>,
}

impl crate::poll::Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }
    fn sleep(&self, d: Duration) {
        *self.now.lock().unwrap() += d;
    }
}
