//! Downloading a provider's result from the URL it returned, only over HTTPS, only from the hosts
//! that provider is known to deliver from (the transport follows no redirects, so the list
//! can't be sidestepped), and bounded in size. `data:` URIs are decoded in place.

use base64::Engine as _;

use crate::{GenError, HttpRequest, HttpTransport, MAX_RESPONSE, Result};

/// The lower-case host of an `https://` URL, without port; `None` for anything else.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.get(..8).filter(|s| s.eq_ignore_ascii_case("https://")).and(url.get(8..))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') || authority.starts_with('[') {
        return None;
    }
    let host = authority.split(':').next()?.to_ascii_lowercase();
    let ok = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then_some(host)
}

/// Entries are exact hosts (`replicate.delivery`) or suffixes starting with a dot (`.bfl.ai`
/// matches any subdomain of `bfl.ai`).
pub fn host_allowed(url: &str, allowed: &[&str]) -> bool {
    let Some(host) = host_of(url) else { return false };
    allowed.iter().any(|a| if a.starts_with('.') { host.len() > a.len() && host.ends_with(a) } else { host == *a })
}

pub fn check_host(url: &str, allowed: &[&str]) -> Result<()> {
    if host_allowed(url, allowed) { Ok(()) } else { Err(GenError::BadResponse("the image service pointed to an unexpected location")) }
}

fn data_uri(uri: &str) -> Result<Vec<u8>> {
    let payload = uri.split_once(";base64,").map(|(_, p)| p).ok_or(GenError::BadResponse("an unreadable data URI"))?;
    if payload.len() / 4 * 3 > MAX_RESPONSE {
        return Err(GenError::BadResponse("the result is too large"));
    }
    base64::engine::general_purpose::STANDARD.decode(payload.trim()).map_err(|_| GenError::BadResponse("an unreadable data URI"))
}

/// The bytes at `url`. `auth` is a header to send (Replicate's file URLs need the token).
pub fn result(http: &dyn HttpTransport, url: &str, allowed: &[&str], auth: Option<(&'static str, String)>) -> Result<Vec<u8>> {
    if url.starts_with("data:") {
        return data_uri(url);
    }
    check_host(url, allowed)?;
    let mut request = HttpRequest::get(url);
    if let Some((name, value)) = auth {
        request = request.header(name, value);
    }
    let r = http.send(request)?;
    if !r.is_success() {
        return Err(GenError::Http { status: r.status, message: "downloading the result failed".into() });
    }
    if r.body.len() > MAX_RESPONSE {
        return Err(GenError::BadResponse("the result is too large"));
    }
    Ok(r.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[test]
    fn hosts_parse_and_match_exactly_or_by_suffix() {
        assert_eq!(host_of("https://Delivery-EU1.BFL.ai:443/x?y").as_deref(), Some("delivery-eu1.bfl.ai"));
        assert_eq!(host_of("http://a.example/"), None, "https only");
        assert_eq!(host_of("https://user@evil.example/"), None, "no userinfo");
        assert_eq!(host_of("https:///path"), None);
        assert!(host_allowed("https://delivery-us1.bfl.ai/r.png", &[".bfl.ai"]));
        assert!(!host_allowed("https://bfl.ai.evil.example/r.png", &[".bfl.ai"]));
        assert!(!host_allowed("https://evilbfl.ai/r.png", &[".bfl.ai"]));
        assert!(host_allowed("https://replicate.delivery/x", &["replicate.delivery"]));
        assert!(!host_allowed("https://x.replicate.delivery/x", &["replicate.delivery"]));
    }

    #[test]
    fn results_download_only_from_allowed_https_hosts_with_optional_auth() {
        let http = FakeHttp::new(vec![bytes_reply(200, "image/png", png(2, 2))]);
        let got = result(&*http, "https://cdn.fal.media/a.png", &[".fal.media"], Some(("authorization", format!("Key {SECRET}")))).unwrap();
        assert_eq!(png_size_of(&got), (2, 2));
        let sent = http.requests();
        assert_eq!(sent[0].method, crate::Method::Get);
        assert_eq!(header(&sent[0], "authorization"), Some(format!("Key {SECRET}").as_str()));

        let http = FakeHttp::new(vec![]);
        let e = result(&*http, "https://evil.example/a.png", &[".fal.media"], None).unwrap_err();
        assert!(matches!(e, GenError::BadResponse(_)));
        assert!(http.requests().is_empty(), "nothing is sent to a host off the list");
    }

    #[test]
    fn data_uris_decode_and_failed_downloads_report_the_status() {
        let http = FakeHttp::new(vec![bytes_reply(404, "text/plain", b"gone".to_vec())]);
        let uri = format!("data:image/png;base64,{}", crate::providers::common::b64(&png(1, 1)));
        assert_eq!(png_size_of(&result(&*http, &uri, &[], None).unwrap()), (1, 1));
        assert!(matches!(result(&*http, "https://a.fal.media/x", &[".fal.media"], None), Err(GenError::Http { status: 404, .. })));
        assert!(result(&*http, "data:image/png;base64,!!!", &[], None).is_err());
    }

    fn png_size_of(b: &[u8]) -> (u32, u32) {
        crate::png_size(b).unwrap()
    }
}
