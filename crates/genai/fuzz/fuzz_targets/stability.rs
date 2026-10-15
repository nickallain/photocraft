//! Fuzz Stability's reply parser: any status and body must give `Ok` or `Err`, never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use photocraft_genai::{Credentials, HttpResponse, providers::stability};

fuzz_target!(|data: &[u8]| {
    let status = data.first().map_or(200, |&b| if b % 2 == 0 { 200 } else { 400 + u16::from(b) });
    let r = HttpResponse { status, headers: Vec::new(), body: data.get(1..).unwrap_or_default().to_vec() };
    let _ = stability::parse(&r, &Credentials::default());
});
