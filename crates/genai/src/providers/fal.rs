//! FLUX.1 Fill [pro] on fal.ai's queue: submit (image and mask as data URIs), poll the status
//! URL, read the response URL, download the image from fal's CDN. Cancelling sends the queue's
//! cancel.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::poll::{Clock, LIMIT, Step, SystemClock};
use crate::providers::common::{self, data_uri, http_error, on_cancel, pick_model, same_size};
use crate::providers::flux_fill;
use crate::{
    API_KEY, API_KEY_FIELD, Capabilities, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider,
    ProviderInfo, ProviderSpec, Result, check_prompt, fetch, mask, poll,
};

pub const ID: &str = "flux-fill.fal";
const NAME: &str = "FLUX.1 Fill [pro] · fal";
const HOST: &str = "queue.fal.run";
pub(crate) const SUBMIT: &str = "https://queue.fal.run/fal-ai/flux-pro/v1/fill";
const API_HOSTS: &[&str] = &["queue.fal.run"];
pub const RESULT_HOSTS: &[&str] = &["fal.media", ".fal.media"];

pub struct Fal {
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(API_KEY)?;
    pick_model(flux_fill::MODELS, model)?;
    Ok(Arc::new(Fal { creds, http, clock: Arc::new(SystemClock::new()) }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: RESULT_HOSTS,
    models: flux_fill::MODELS,
    fields: &[API_KEY_FIELD],
    key_page: "https://fal.ai/dashboard/keys",
    note: "",
    build,
};

pub struct Job {
    pub status_url: String,
    pub response_url: String,
    pub cancel_url: Option<String>,
}

pub fn parse_submit(r: &HttpResponse, creds: &Credentials) -> Result<Job> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    Ok(Job {
        status_url: s("status_url").ok_or(GenError::BadResponse("no job in the reply"))?,
        response_url: s("response_url").ok_or(GenError::BadResponse("no job in the reply"))?,
        cancel_url: s("cancel_url"),
    })
}

pub fn parse_status(r: &HttpResponse, creds: &Credentials) -> Result<Step<()>> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    match v.get("status").and_then(Value::as_str) {
        Some("COMPLETED") => match v.get("error").and_then(Value::as_str) {
            Some(e) => Err(GenError::Refused(creds.scrub(e))),
            None => Ok(Step::Done(())),
        },
        _ => Ok(Step::Pending),
    }
}

pub fn parse_result(r: &HttpResponse, creds: &Credentials) -> Result<String> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    if v.pointer("/has_nsfw_concepts/0").and_then(Value::as_bool) == Some(true) {
        return Err(GenError::Moderated("fal flagged the result".into()));
    }
    v.pointer("/images/0/url").and_then(Value::as_str).map(str::to_owned).ok_or(GenError::BadResponse("no image in the reply"))
}

impl ImageProvider for Fal {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: ID, name: NAME, model: "flux-pro-1.0-fill".into(), host: HOST }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { generate: false, edit: true, variations: false }
    }
    fn edit(&self, r: &EditRequest) -> Result<GeneratedImage> {
        check_prompt(&r.prompt, false)?;
        same_size(&r.image, &r.mask)?;
        r.cancel.check()?;
        let fill = mask::white_fill(&r.mask)?;
        let mut body = flux_fill::params(&r.prompt);
        body.insert("image_url".into(), json!(data_uri("image/png", &r.image)));
        body.insert("mask_url".into(), json!(data_uri("image/png", &fill)));
        body.insert("safety_tolerance".into(), json!("2"));
        body.insert("num_images".into(), json!(1));
        let auth = format!("Key {}", self.creds.require(API_KEY)?.expose());
        let submitted = self.http.send(HttpRequest::post(SUBMIT, Vec::new()).json(&Value::Object(body)).header("authorization", auth.clone()))?;
        let job = parse_submit(&submitted, &self.creds)?;
        fetch::check_host(&job.status_url, API_HOSTS)?;
        fetch::check_host(&job.response_url, API_HOSTS)?;
        let cancel_url = job.cancel_url.clone().filter(|u| fetch::host_allowed(u, API_HOSTS));
        let waited = poll::until(
            &*self.http,
            &|| HttpRequest::get(job.status_url.clone()).header("authorization", auth.clone()),
            &mut |r| parse_status(r, &self.creds),
            LIMIT,
            &r.cancel,
            &*self.clock,
        );
        on_cancel(waited, || {
            if let Some(url) = &cancel_url {
                let _ = self.http.send(HttpRequest::put(url.clone()).header("authorization", auth.clone()));
            }
        })?;
        let result = self.http.send(HttpRequest::get(job.response_url.clone()).header("authorization", auth.clone()))?;
        let url = parse_result(&result, &self.creds)?;
        r.cancel.check()?;
        common::image(fetch::result(&*self.http, &url, RESULT_HOSTS, None)?)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ApiKey, Method};
    use serde_json::json;

    const BASE: &str = "https://queue.fal.run/fal-ai/flux-pro/requests/r1";

    fn provider(replies: Vec<Result<HttpResponse>>) -> (Fal, Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        (Fal { creds: Credentials::api_key(ApiKey::new(SECRET).unwrap()), http: http.clone(), clock: Arc::new(FakeClock::default()) }, http)
    }

    fn submitted() -> Result<HttpResponse> {
        json_reply(200, json!({"request_id": "r1", "status_url": format!("{BASE}/status"), "response_url": BASE, "cancel_url": format!("{BASE}/cancel")}))
    }

    #[test]
    fn queues_polls_reads_the_result_and_downloads_it() {
        let (p, http) = provider(vec![
            submitted(),
            json_reply(200, json!({"status": "IN_QUEUE"})),
            json_reply(200, json!({"status": "COMPLETED"})),
            json_reply(200, json!({"images": [{"url": "https://v3.fal.media/files/x.png"}], "has_nsfw_concepts": [false]})),
            bytes_reply(200, "image/png", png(4, 4)),
        ]);
        p.edit(&edit_request("a kite", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[0].url, SUBMIT);
        assert_eq!(header(&sent[0], "authorization"), Some(format!("Key {SECRET}").as_str()));
        let b: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert!(b["image_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert!(b["mask_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(b["safety_tolerance"], "2");
        assert_eq!((sent[1].method, sent[1].url.as_str()), (Method::Get, format!("{BASE}/status").as_str()));
        assert_eq!(sent[3].url, BASE);
        assert_eq!(sent[4].url, "https://v3.fal.media/files/x.png");
        assert!(header(&sent[4], "authorization").is_none());
    }

    #[test]
    fn an_empty_prompt_sends_the_description_not_an_instruction() {
        let (p, http) = provider(vec![json_reply(401, json!({}))]);
        let _ = p.edit(&edit_request("", 8, 8));
        let b: serde_json::Value = serde_json::from_slice(&http.requests()[0].body).unwrap();
        assert_eq!(b["prompt"], crate::providers::common::CONTINUE_DESCRIPTION);
    }

    #[test]
    fn a_foreign_status_url_is_refused_without_sending_the_key() {
        let reply = json_reply(200, json!({"request_id": "r1", "status_url": "https://evil.example/s", "response_url": BASE}));
        let (p, http) = provider(vec![reply]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn cancelling_sends_the_remote_cancel() {
        let (p, http) = provider(vec![submitted(), json_reply(200, json!({"status": "IN_PROGRESS"})), json_reply(202, json!({}))]);
        let sent = http.clone();
        let mut r = edit_request("x", 8, 8);
        r.cancel = crate::CancelToken::new(move || sent.requests().len() >= 2);
        assert_eq!(p.edit(&r), Err(GenError::Cancelled));
        let last = http.requests().pop().unwrap();
        assert_eq!((last.method, last.url.as_str()), (Method::Put, format!("{BASE}/cancel").as_str()));
        assert_eq!(header(&last, "authorization"), Some(format!("Key {SECRET}").as_str()));
    }

    #[test]
    fn nsfw_and_errors() {
        let (p, _) = provider(vec![
            submitted(),
            json_reply(200, json!({"status": "COMPLETED"})),
            json_reply(200, json!({"images": [{"url": "https://v3.fal.media/x.png"}], "has_nsfw_concepts": [true]})),
        ]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, _) = provider(vec![submitted(), json_reply(200, json!({"status": "COMPLETED", "error": "boom", "error_type": "runner"}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Refused(_))));
        let (p, _) = provider(vec![json_reply(401, json!({"detail": "bad key"}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Auth));
    }
}
