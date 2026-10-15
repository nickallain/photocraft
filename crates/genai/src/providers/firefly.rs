//! Adobe Firefly Fill (Firefly Services): an OAuth server-to-server token from Adobe IMS (client
//! id and secret, cached in memory until five minutes before it expires, renewed once on a 401),
//! the image and mask uploaded to Firefly storage, `fill-async` submitted, its status polled on
//! firefly-api.adobe.io, and the pre-signed result downloaded from S3. Firefly needs a prompt.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::poll::{Clock, LIMIT, Step, SystemClock};
use crate::providers::common::{self, form_urlencode, http_error, on_cancel, pick_model, same_size};
use crate::{
    ApiKey, Capabilities, CredentialField, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider, Method,
    ProviderInfo, ProviderSpec, Result, check_prompt, fetch, mask, poll,
};

pub const ID: &str = "firefly";
const NAME: &str = "Adobe Firefly Fill";
const HOST: &str = "firefly-api.adobe.io";
pub(crate) const IMS: &str = "https://ims-na1.adobelogin.com/ims/token/v3";
pub(crate) const UPLOAD: &str = "https://firefly-api.adobe.io/v2/storage/image";
pub(crate) const FILL: &str = "https://firefly-api.adobe.io/v3/images/fill-async";
const API_HOSTS: &[&str] = &["firefly-api.adobe.io"];
pub const RESULT_HOSTS: &[&str] = &[".amazonaws.com"];
const SCOPE: &str = "openid,AdobeID,session,additional_info,read_organizations,firefly_api,ff_apis";
pub const CLIENT_ID: &str = "client_id";
pub const CLIENT_SECRET: &str = "client_secret";
const FIELDS: &[CredentialField] = &[CredentialField { name: CLIENT_ID, label: "Client ID" }, CredentialField { name: CLIENT_SECRET, label: "Client secret" }];
const MODELS: &[(&str, &str)] = &[("firefly-fill", "Firefly Fill")];
/// Renew this long before the token's stated expiry.
const EARLY: u64 = 300;
/// IMS tokens last about a day; never trust a longer lifetime.
const MAX_LIFETIME: u64 = 86_400;

pub struct Firefly {
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    /// The access token and when (clock time) to stop using it.
    token: Mutex<Option<(ApiKey, Duration)>>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(CLIENT_ID)?;
    creds.require(CLIENT_SECRET)?;
    pick_model(MODELS, model)?;
    Ok(Arc::new(Firefly { creds, http, clock: Arc::new(SystemClock::new()), token: Mutex::new(None) }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: RESULT_HOSTS,
    models: MODELS,
    fields: FIELDS,
    key_page: "https://developer.adobe.com/console",
    note: "",
    build,
};

pub fn parse_token(r: &HttpResponse, creds: &Credentials) -> Result<(ApiKey, u64)> {
    if matches!(r.status, 400 | 401 | 403) {
        return Err(GenError::Auth);
    }
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    let token = v.get("access_token").and_then(Value::as_str).and_then(ApiKey::new).ok_or(GenError::BadResponse("no token in the reply"))?;
    Ok((token, v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).min(MAX_LIFETIME)))
}

pub fn parse_upload(r: &HttpResponse, creds: &Credentials) -> Result<String> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    common::json(r)?.pointer("/images/0/id").and_then(Value::as_str).map(str::to_owned).ok_or(GenError::BadResponse("no upload id in the reply"))
}

pub struct Job {
    pub status_url: String,
    pub cancel_url: Option<String>,
}

pub fn parse_submit(r: &HttpResponse, creds: &Credentials) -> Result<Job> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    Ok(Job {
        status_url: v.get("statusUrl").and_then(Value::as_str).map(str::to_owned).ok_or(GenError::BadResponse("no job in the reply"))?,
        cancel_url: v.get("cancelUrl").and_then(Value::as_str).map(str::to_owned),
    })
}

pub fn parse_status(r: &HttpResponse, creds: &Credentials) -> Result<Step<String>> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    match v.get("status").and_then(Value::as_str) {
        Some("succeeded") => v
            .pointer("/result/outputs/0/image/url")
            .and_then(Value::as_str)
            .map(|u| Step::Done(u.to_owned()))
            .ok_or(GenError::BadResponse("no image in the reply")),
        Some("failed") => {
            let msg = creds.scrub(v.get("message").and_then(Value::as_str).unwrap_or("Firefly reported an error"));
            let code = v.get("error_code").and_then(Value::as_str).unwrap_or("");
            if code.contains("policy") || msg.to_ascii_lowercase().contains("policy") { Err(GenError::Moderated(msg)) } else { Err(GenError::Refused(msg)) }
        }
        _ => Ok(Step::Pending),
    }
}

impl Firefly {
    fn token(&self, fresh: bool) -> Result<ApiKey> {
        let now = self.clock.now();
        let mut slot = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        if !fresh
            && let Some((t, until)) = slot.as_ref()
            && now < *until
        {
            return Ok(t.clone());
        }
        let body = form_urlencode(&[
            ("grant_type", "client_credentials"),
            ("client_id", self.creds.require(CLIENT_ID)?.expose()),
            ("client_secret", self.creds.require(CLIENT_SECRET)?.expose()),
            ("scope", SCOPE),
        ]);
        let r = self.http.send(HttpRequest::post(IMS, body.into_bytes()).header("content-type", "application/x-www-form-urlencoded"))?;
        let (token, expires) = parse_token(&r, &self.creds)?;
        *slot = now.checked_add(Duration::from_secs(expires.saturating_sub(EARLY))).map(|until| (token.clone(), until));
        Ok(token)
    }

    fn authed(&self, request: HttpRequest, token: &ApiKey) -> Result<HttpRequest> {
        Ok(request.header("x-api-key", self.creds.require(CLIENT_ID)?.expose()).header("authorization", format!("Bearer {}", token.expose())))
    }

    fn upload(&self, token: &ApiKey, png: &[u8]) -> Result<String> {
        let r = self.http.send(self.authed(HttpRequest::post(UPLOAD, png.to_vec()).header("content-type", "image/png"), token)?)?;
        parse_upload(&r, &self.creds)
    }
}

impl ImageProvider for Firefly {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: ID, name: NAME, model: "firefly-fill".into(), host: HOST }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { generate: false, edit: true, variations: false }
    }
    fn needs_prompt(&self) -> bool {
        true
    }
    fn edit(&self, r: &EditRequest) -> Result<GeneratedImage> {
        check_prompt(&r.prompt, true)?;
        same_size(&r.image, &r.mask)?;
        r.cancel.check()?;
        let fill = mask::white_fill(&r.mask)?;
        let mut token = self.token(false)?;
        let image_id = match self.upload(&token, &r.image) {
            Err(GenError::Auth) => {
                token = self.token(true)?;
                self.upload(&token, &r.image)?
            }
            other => other?,
        };
        let mask_id = self.upload(&token, &fill)?;
        r.cancel.check()?;
        // Adobe's v3 fill tutorial (2026-10-12) nests the mask as `image.mask.uploadId`, beside `image.source`.
        let body = json!({"prompt": r.prompt.trim(), "numVariations": 1, "image": {"source": {"uploadId": image_id}, "mask": {"uploadId": mask_id}}});
        let submitted = self.http.send(self.authed(HttpRequest::new(Method::Post, FILL).json(&body), &token)?)?;
        let job = parse_submit(&submitted, &self.creds)?;
        fetch::check_host(&job.status_url, API_HOSTS)?;
        let cancel = job.cancel_url.clone().filter(|u| fetch::host_allowed(u, API_HOSTS));
        let client_id = self.creds.require(CLIENT_ID)?.expose().to_owned();
        let bearer = format!("Bearer {}", token.expose());
        let waited = poll::until(
            &*self.http,
            &|| HttpRequest::get(job.status_url.clone()).header("x-api-key", client_id.clone()).header("authorization", bearer.clone()),
            &mut |r| parse_status(r, &self.creds),
            LIMIT,
            &r.cancel,
            &*self.clock,
        );
        let url = on_cancel(waited, || {
            if let Some(url) = &cancel {
                let _ = self.http.send(HttpRequest::put(url.clone()).header("x-api-key", client_id.clone()).header("authorization", bearer.clone()));
            }
        })?;
        r.cancel.check()?;
        common::image(fetch::result(&*self.http, &url, RESULT_HOSTS, None)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Method;
    use crate::testing::*;
    use serde_json::json;

    const STATUS: &str = "https://firefly-api.adobe.io/v3/status/j1";
    const CANCEL: &str = "https://firefly-api.adobe.io/v3/cancel/j1";
    const OUT: &str = "https://pre-signed-firefly-prod.s3-accelerate.amazonaws.com/out.png?sig=1";

    fn creds() -> Credentials {
        let mut c = Credentials::default();
        c.insert(CLIENT_ID, ApiKey::new("cid-123").unwrap());
        c.insert(CLIENT_SECRET, ApiKey::new(SECRET).unwrap());
        c
    }

    fn provider(replies: Vec<Result<HttpResponse>>) -> (Firefly, Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        (Firefly { creds: creds(), http: http.clone(), clock: Arc::new(FakeClock::default()), token: Mutex::new(None) }, http)
    }

    fn token() -> Result<HttpResponse> {
        json_reply(200, json!({"access_token": "tok-1", "token_type": "bearer", "expires_in": 86399}))
    }

    fn upload(id: &str) -> Result<HttpResponse> {
        json_reply(200, json!({"images": [{"id": id}]}))
    }

    fn happy() -> Vec<Result<HttpResponse>> {
        vec![
            token(),
            upload("u-img"),
            upload("u-msk"),
            json_reply(202, json!({"jobId": "j1", "statusUrl": STATUS, "cancelUrl": CANCEL})),
            json_reply(200, json!({"status": "running"})),
            json_reply(200, json!({"status": "succeeded", "result": {"outputs": [{"seed": 1, "image": {"url": OUT}}]}})),
            bytes_reply(200, "image/png", png(4, 4)),
        ]
    }

    #[test]
    fn gets_a_token_uploads_fills_polls_and_downloads() {
        let (p, http) = provider(happy());
        p.edit(&edit_request("a hot air balloon", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[0].url, IMS);
        assert_eq!(header(&sent[0], "content-type"), Some("application/x-www-form-urlencoded"));
        let form = String::from_utf8(sent[0].body.clone()).unwrap();
        assert!(form.contains("grant_type=client_credentials") && form.contains("client_id=cid-123") && form.contains("scope=openid%2CAdobeID"), "{form}");
        for r in &sent[1..6] {
            assert_eq!(header(r, "x-api-key"), Some("cid-123"));
            assert_eq!(header(r, "authorization"), Some("Bearer tok-1"));
            assert!(!r.url.contains(SECRET) && !r.url.contains("tok-1"));
        }
        assert_eq!((sent[1].url.as_str(), header(&sent[1], "content-type")), (UPLOAD, Some("image/png")));
        let b: serde_json::Value = serde_json::from_slice(&sent[3].body).unwrap();
        assert_eq!(sent[3].url, FILL);
        assert_eq!(b["prompt"], "a hot air balloon");
        assert_eq!(b["image"]["source"]["uploadId"], "u-img");
        assert_eq!(b["image"]["mask"]["uploadId"], "u-msk");
        assert_eq!(sent[4].url, STATUS);
        assert_eq!(sent[6].url, OUT);
        assert!(header(&sent[6], "authorization").is_none());
    }

    #[test]
    fn the_token_is_reused_until_it_expires_and_renewed_once_on_401() {
        let (p, http) = provider(happy());
        p.edit(&edit_request("x", 8, 8)).unwrap();
        // Second fill: no token request.
        let more: Vec<_> = happy().into_iter().skip(1).collect();
        let http2 = FakeHttp::new(more);
        let p2 = Firefly { http: http2.clone(), ..p };
        p2.edit(&edit_request("x", 8, 8)).unwrap();
        assert_eq!(http2.requests()[0].url, UPLOAD, "cached token");
        assert_eq!(http.requests().iter().filter(|r| r.url == IMS).count(), 1);

        let mut replies = vec![token(), json_reply(401, json!({"error_code": "401013"}))];
        replies.extend(happy());
        let (p, http) = provider(replies);
        p.edit(&edit_request("x", 8, 8)).unwrap();
        assert_eq!(http.requests().iter().filter(|r| r.url == IMS).count(), 2, "renewed once after 401");
    }

    #[test]
    fn needs_a_prompt_and_both_credentials() {
        let (p, http) = provider(vec![]);
        assert!(p.needs_prompt());
        assert!(matches!(p.edit(&edit_request("", 8, 8)), Err(GenError::Invalid(_))));
        assert!(http.requests().is_empty());
        let mut half = Credentials::default();
        half.insert(CLIENT_ID, ApiKey::new("cid").unwrap());
        assert!((SPEC.build)("", half, FakeHttp::new(vec![])).is_err());
    }

    #[test]
    fn a_foreign_status_url_is_refused_without_sending_the_token() {
        let replies = vec![token(), upload("a"), upload("b"), json_reply(202, json!({"jobId": "j1", "statusUrl": "https://evil.example/s"}))];
        let (p, http) = provider(replies);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
        assert_eq!(http.requests().len(), 4);
    }

    #[test]
    fn failures_cancel_and_bad_credentials() {
        let (p, _) = provider(vec![json_reply(400, json!({"error": "invalid_client"}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Auth));
        let mut replies: Vec<_> = happy().into_iter().take(4).collect();
        replies.push(json_reply(200, json!({"status": "failed", "error_code": "content_policy", "message": "violates content policy"})));
        let (p, _) = provider(replies);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let mut replies: Vec<_> = happy().into_iter().take(5).collect();
        replies.push(json_reply(200, json!({})));
        let (p, http) = provider(replies);
        let sent = http.clone();
        let mut r = edit_request("x", 8, 8);
        r.cancel = crate::CancelToken::new(move || sent.requests().len() >= 5);
        assert_eq!(p.edit(&r), Err(GenError::Cancelled));
        let last = http.requests().pop().unwrap();
        assert_eq!((last.method, last.url.as_str()), (Method::Put, CANCEL));
    }

    fn with_clock(replies: Vec<Result<HttpResponse>>) -> (Firefly, Arc<FakeHttp>, Arc<FakeClock>) {
        let http = FakeHttp::new(replies);
        let clock = Arc::new(FakeClock::default());
        (Firefly { creds: creds(), http: http.clone(), clock: clock.clone(), token: Mutex::new(None) }, http, clock)
    }

    #[test]
    fn a_huge_expires_in_cannot_overflow_the_expiry() {
        let mut replies = vec![json_reply(200, json!({"access_token": "tok-1", "expires_in": u64::MAX}))];
        replies.extend(happy().into_iter().skip(1));
        let (p, _, clock) = with_clock(replies);
        crate::poll::Clock::sleep(&*clock, Duration::from_secs(1000));
        p.edit(&edit_request("x", 8, 8)).unwrap();
    }

    #[test]
    fn an_expired_token_is_replaced() {
        let mut replies = happy();
        replies.extend(happy());
        let (p, http, clock) = with_clock(replies);
        p.edit(&edit_request("x", 8, 8)).unwrap();
        crate::poll::Clock::sleep(&*clock, Duration::from_secs(86399 - 300 + 1));
        p.edit(&edit_request("x", 8, 8)).unwrap();
        assert_eq!(http.requests().iter().filter(|r| r.url == IMS).count(), 2);
    }
}
