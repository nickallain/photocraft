//! Black Forest Labs: FLUX.1 Fill [pro] (with a prompt) and FLUX Erase (without). Always
//! asynchronous: submit, poll the returned `polling_url` (on a bfl.ai host) with the key, then
//! download the signed sample (valid 10 minutes) without it.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::poll::{Clock, LIMIT, Step, SystemClock};
use crate::providers::common::{self, b64, http_error, pick_model, same_size};
use crate::providers::flux_fill;
use crate::{
    API_KEY, API_KEY_FIELD, Capabilities, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider,
    ProviderInfo, ProviderSpec, Result, check_prompt, fetch, mask, poll,
};

pub const ID: &str = "flux-fill.bfl";
const NAME: &str = "FLUX.1 Fill [pro] · Black Forest Labs";
const HOST: &str = "api.bfl.ai";
pub(crate) const FILL: &str = "https://api.bfl.ai/v1/flux-pro-1.0-fill";
pub(crate) const ERASE: &str = "https://api.bfl.ai/v1/flux-tools/erase-v1";
const API_HOSTS: &[&str] = &[".bfl.ai"];
pub const RESULT_HOSTS: &[&str] = &[".bfl.ai"];

pub struct Bfl {
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(API_KEY)?;
    pick_model(flux_fill::MODELS, model)?;
    Ok(Arc::new(Bfl { creds, http, clock: Arc::new(SystemClock::new()) }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: RESULT_HOSTS,
    models: flux_fill::MODELS,
    fields: &[API_KEY_FIELD],
    key_page: "https://dashboard.bfl.ai/keys",
    note: "",
    build,
};

pub fn parse_submit(r: &HttpResponse, creds: &Credentials) -> Result<String> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    common::json(r)?.get("polling_url").and_then(Value::as_str).map(str::to_owned).ok_or(GenError::BadResponse("no job in the reply"))
}

pub fn parse_poll(r: &HttpResponse, creds: &Credentials) -> Result<Step<String>> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    match v.get("status").and_then(Value::as_str) {
        Some("Ready") => {
            v.pointer("/result/sample").and_then(Value::as_str).map(|s| Step::Done(s.to_owned())).ok_or(GenError::BadResponse("no image in the reply"))
        }
        Some(s @ ("Request Moderated" | "Content Moderated")) => Err(GenError::Moderated(s.to_owned())),
        Some(s @ ("Error" | "Task not found" | "Failed")) => Err(GenError::Refused(s.to_owned())),
        _ => Ok(Step::Pending),
    }
}

impl ImageProvider for Bfl {
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
        let (url, body) = if r.prompt.trim().is_empty() {
            (ERASE, json!({"image": b64(&r.image), "mask": b64(&fill), "dilate_pixels": 5}))
        } else {
            let mut p = flux_fill::params(&r.prompt);
            p.insert("image".into(), json!(b64(&r.image)));
            p.insert("mask".into(), json!(b64(&fill)));
            p.insert("safety_tolerance".into(), json!(2));
            (FILL, Value::Object(p))
        };
        let key = self.creds.require(API_KEY)?.expose().to_owned();
        let submitted = self.http.send(HttpRequest::post(url, Vec::new()).json(&body).header("x-key", key.clone()))?;
        let polling = parse_submit(&submitted, &self.creds)?;
        fetch::check_host(&polling, API_HOSTS)?;
        let sample = poll::until(
            &*self.http,
            &|| HttpRequest::get(polling.clone()).header("x-key", key.clone()),
            &mut |r| parse_poll(r, &self.creds),
            LIMIT,
            &r.cancel,
            &*self.clock,
        )?;
        r.cancel.check()?;
        common::image(fetch::result(&*self.http, &sample, RESULT_HOSTS, None)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ApiKey, Method};
    use serde_json::json;

    const POLL: &str = "https://api.us1.bfl.ai/v1/get_result?id=t1";
    const SAMPLE: &str = "https://delivery-us1.bfl.ai/results/t1/sample.png?sig=x";

    fn provider(replies: Vec<Result<HttpResponse>>) -> (Bfl, Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        (Bfl { creds: Credentials::api_key(ApiKey::new(SECRET).unwrap()), http: http.clone(), clock: Arc::new(FakeClock::default()) }, http)
    }

    fn body(r: &crate::HttpRequest) -> serde_json::Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    #[test]
    fn fills_submit_poll_and_download() {
        let (p, http) = provider(vec![
            json_reply(200, json!({"id": "t1", "polling_url": POLL})),
            json_reply(200, json!({"status": "Pending"})),
            json_reply(200, json!({"status": "Ready", "result": {"sample": SAMPLE}})),
            bytes_reply(200, "image/png", png(4, 4)),
        ]);
        p.edit(&edit_request("a dock", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[0].url, FILL);
        assert_eq!(header(&sent[0], "x-key"), Some(SECRET));
        let b = body(&sent[0]);
        assert_eq!((b["prompt"].as_str(), b["safety_tolerance"].as_u64()), (Some("a dock"), Some(2)));
        let (_, _, m) = crate::mask::decode_rgba8(&common::decode_b64(b["mask"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!((m[0], m[7 * 4]), (255, 0));
        assert_eq!((sent[1].method, sent[1].url.as_str()), (Method::Get, POLL));
        assert_eq!(header(&sent[1], "x-key"), Some(SECRET));
        assert_eq!(sent[3].url, SAMPLE);
        assert!(header(&sent[3], "x-key").is_none());
    }

    #[test]
    fn no_prompt_uses_flux_erase() {
        let (p, http) = provider(vec![
            json_reply(200, json!({"id": "t1", "polling_url": POLL})),
            json_reply(200, json!({"status": "Ready", "result": {"sample": SAMPLE}})),
            bytes_reply(200, "image/png", png(4, 4)),
        ]);
        p.edit(&edit_request("", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[0].url, ERASE);
        assert!(body(&sent[0]).get("prompt").is_none());
    }

    #[test]
    fn a_foreign_polling_url_is_refused_without_sending_the_key() {
        let (p, http) = provider(vec![json_reply(200, json!({"id": "t1", "polling_url": "https://bfl.ai.evil.example/x"}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn moderation_errors_and_cancel() {
        let (p, _) = provider(vec![json_reply(200, json!({"id": "t1", "polling_url": POLL})), json_reply(200, json!({"status": "Content Moderated"}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, _) = provider(vec![json_reply(200, json!({"id": "t1", "polling_url": POLL})), json_reply(200, json!({"status": "Error"}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Refused(_))));
        let (p, _) = provider(vec![json_reply(402, json!({"detail": "Insufficient credits"}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Quota));
        let (p, http) = provider(vec![json_reply(200, json!({"id": "t1", "polling_url": POLL})), json_reply(200, json!({"status": "Pending"}))]);
        let sent = http.clone();
        let mut r = edit_request("x", 8, 8);
        r.cancel = crate::CancelToken::new(move || sent.requests().len() >= 2);
        assert_eq!(p.edit(&r), Err(GenError::Cancelled));
        assert_eq!(http.requests().len(), 2, "no more polling after cancel");
    }
}
