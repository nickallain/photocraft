//! Bria v2 Gen Fill (with a prompt) and Erase (without): JSON with base64 image and mask, white
//! = fill. Asked to answer synchronously; if it returns a job instead, its status is polled on
//! Bria's API host.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::poll::{Clock, LIMIT, Step, SystemClock};
use crate::providers::common::{self, b64, http_error, pick_model, same_size};
use crate::{
    API_KEY, API_KEY_FIELD, Capabilities, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider,
    ProviderInfo, ProviderSpec, Result, check_prompt, fetch, mask, poll,
};

pub const ID: &str = "bria";
const NAME: &str = "Bria Gen Fill and Erase";
const HOST: &str = "engine.prod.bria-api.com";
pub(crate) const FILL: &str = "https://engine.prod.bria-api.com/v2/image/edit/gen_fill";
pub(crate) const ERASE: &str = "https://engine.prod.bria-api.com/v2/image/edit/erase";
const API_HOSTS: &[&str] = &["engine.prod.bria-api.com"];
/// Where results are downloaded from. The live check (2026-10-12) downloaded a result within this
/// allowlist.
pub const RESULT_HOSTS: &[&str] = &[".bria-api.com", ".bria.ai"];
const MODELS: &[(&str, &str)] = &[("v2", "Bria v2")];

pub struct Bria {
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(API_KEY)?;
    pick_model(MODELS, model)?;
    Ok(Arc::new(Bria { creds, http, clock: Arc::new(SystemClock::new()) }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: RESULT_HOSTS,
    models: MODELS,
    fields: &[API_KEY_FIELD],
    key_page: "https://platform.bria.ai/console/account/api-keys",
    note: "",
    build,
};

pub enum Reply {
    /// The result's URL.
    Done(String),
    /// A status URL to poll.
    Pending(String),
}

fn errors(r: &HttpResponse, creds: &Credentials) -> GenError {
    if r.status == 422 { GenError::Moderated(common::error_text(&r.body, creds)) } else { http_error(r, creds) }
}

pub fn parse(r: &HttpResponse, creds: &Credentials) -> Result<Reply> {
    if !r.is_success() {
        return Err(errors(r, creds));
    }
    let v = common::json(r)?;
    if let Some(url) = v.pointer("/result/image_url").and_then(Value::as_str) {
        return Ok(Reply::Done(url.to_owned()));
    }
    if let Some(url) = v.get("status_url").and_then(Value::as_str) {
        return Ok(Reply::Pending(url.to_owned()));
    }
    status_step(&v, creds).and_then(|s| match s {
        Step::Done(url) => Ok(Reply::Done(url)),
        Step::Pending => Err(GenError::BadResponse("no image in the reply")),
    })
}

fn status_step(v: &Value, creds: &Credentials) -> Result<Step<String>> {
    match v.get("status").and_then(Value::as_str) {
        Some("COMPLETED") => {
            v.pointer("/result/image_url").and_then(Value::as_str).map(|u| Step::Done(u.to_owned())).ok_or(GenError::BadResponse("no image in the reply"))
        }
        Some("ERROR") => {
            let msg = creds.scrub(v.pointer("/error/message").and_then(Value::as_str).unwrap_or("Bria reported an error"));
            Err(GenError::Refused(msg))
        }
        _ => Ok(Step::Pending),
    }
}

pub fn parse_status(r: &HttpResponse, creds: &Credentials) -> Result<Step<String>> {
    if !r.is_success() {
        return Err(errors(r, creds));
    }
    status_step(&common::json(r)?, creds)
}

impl ImageProvider for Bria {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: ID, name: NAME, model: "v2".into(), host: HOST }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { generate: false, edit: true, variations: false }
    }
    fn edit(&self, r: &EditRequest) -> Result<GeneratedImage> {
        check_prompt(&r.prompt, false)?;
        same_size(&r.image, &r.mask)?;
        r.cancel.check()?;
        let fill = mask::white_fill(&r.mask)?;
        let prompt = r.prompt.trim();
        let (url, body) = if prompt.is_empty() {
            (ERASE, json!({"image": b64(&r.image), "mask": b64(&fill), "mask_type": "manual", "sync": true}))
        } else {
            (FILL, json!({"image": b64(&r.image), "mask": b64(&fill), "prompt": prompt, "sync": true}))
        };
        let key = self.creds.require(API_KEY)?.expose().to_owned();
        let reply = self.http.send(HttpRequest::post(url, Vec::new()).json(&body).header("api_token", key.clone()))?;
        let result = match parse(&reply, &self.creds)? {
            Reply::Done(url) => url,
            Reply::Pending(status) => {
                fetch::check_host(&status, API_HOSTS)?;
                poll::until(
                    &*self.http,
                    &|| HttpRequest::get(status.clone()).header("api_token", key.clone()),
                    &mut |r| parse_status(r, &self.creds),
                    LIMIT,
                    &r.cancel,
                    &*self.clock,
                )?
            }
        };
        r.cancel.check()?;
        common::image(fetch::result(&*self.http, &result, RESULT_HOSTS, None)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ApiKey, Method};
    use serde_json::json;

    fn provider(replies: Vec<Result<HttpResponse>>) -> (Bria, Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        let p = Bria { creds: Credentials::api_key(ApiKey::new(SECRET).unwrap()), http: http.clone(), clock: Arc::new(FakeClock::default()) };
        (p, http)
    }

    fn body(r: &crate::HttpRequest) -> serde_json::Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    const RESULT: &str = "https://cdn.bria-api.com/out/1.png";

    #[test]
    fn a_prompt_goes_to_gen_fill_synchronously() {
        let (p, http) = provider(vec![json_reply(200, json!({"result": {"image_url": RESULT}, "request_id": "r1"})), bytes_reply(200, "image/png", png(4, 4))]);
        p.edit(&edit_request("a vase", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[0].url, FILL);
        assert_eq!(header(&sent[0], "api_token"), Some(SECRET));
        let b = body(&sent[0]);
        assert_eq!((b["prompt"].as_str(), b["sync"].as_bool()), (Some("a vase"), Some(true)));
        let mask = common::decode_b64(b["mask"].as_str().unwrap()).unwrap();
        let (_, _, m) = crate::mask::decode_rgba8(&mask).unwrap();
        assert_eq!((m[0], m[7 * 4]), (255, 0), "white = fill");
        assert_eq!((sent[1].method, sent[1].url.as_str()), (Method::Get, RESULT));
        assert!(header(&sent[1], "api_token").is_none());
    }

    #[test]
    fn no_prompt_goes_to_erase() {
        let (p, http) = provider(vec![json_reply(200, json!({"result": {"image_url": RESULT}})), bytes_reply(200, "image/png", png(4, 4))]);
        p.edit(&edit_request("", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[0].url, ERASE);
        assert!(body(&sent[0]).get("prompt").is_none());
        assert_eq!(body(&sent[0])["mask_type"], "manual");
    }

    #[test]
    fn an_async_reply_is_polled_on_brias_own_host() {
        let replies = vec![
            json_reply(202, json!({"request_id": "r1", "status_url": "https://engine.prod.bria-api.com/v2/status/r1"})),
            json_reply(200, json!({"status": "IN_PROGRESS"})),
            json_reply(200, json!({"status": "COMPLETED", "result": {"image_url": RESULT}})),
            bytes_reply(200, "image/png", png(4, 4)),
        ];
        let (p, http) = provider(replies);
        p.edit(&edit_request("x", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[1].url, "https://engine.prod.bria-api.com/v2/status/r1");
        assert_eq!(header(&sent[1], "api_token"), Some(SECRET));
    }

    #[test]
    fn a_foreign_status_url_is_refused_without_sending_the_key() {
        let (p, http) = provider(vec![json_reply(202, json!({"request_id": "r1", "status_url": "https://evil.example/status"}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn errors_and_moderation() {
        let (p, _) = provider(vec![json_reply(200, json!({"status": "ERROR", "error": {"message": "failed"}}))]);
        assert!(p.edit(&edit_request("x", 8, 8)).is_err());
        let (p, _) = provider(vec![json_reply(422, json!({"message": "Content moderation: prompt rejected"}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, _) = provider(vec![json_reply(401, json!({}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Auth));
    }
}
