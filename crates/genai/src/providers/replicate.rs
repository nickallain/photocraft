//! FLUX.1 Fill [pro] on Replicate: inputs uploaded through the Files API (data URLs are only for
//! files under 256 KB), a prediction created with `Prefer: wait=60`, polled at `urls.get` if not
//! done, and the output downloaded with the token.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::poll::{Clock, LIMIT, Step, SystemClock};
use crate::providers::common::{self, http_error, on_cancel, pick_model, same_size};
use crate::providers::flux_fill;
use crate::{
    API_KEY, API_KEY_FIELD, Capabilities, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider,
    ProviderInfo, ProviderSpec, Result, check_prompt, fetch, mask, multipart::Form, poll,
};

pub const ID: &str = "flux-fill.replicate";
const NAME: &str = "FLUX.1 Fill [pro] · Replicate";
const HOST: &str = "api.replicate.com";
pub(crate) const PREDICT: &str = "https://api.replicate.com/v1/models/black-forest-labs/flux-fill-pro/predictions";
pub(crate) const FILES: &str = "https://api.replicate.com/v1/files";
const API_HOSTS: &[&str] = &["api.replicate.com"];
pub const RESULT_HOSTS: &[&str] = &["replicate.delivery", ".replicate.delivery"];

pub struct Replicate {
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(API_KEY)?;
    pick_model(flux_fill::MODELS, model)?;
    Ok(Arc::new(Replicate { creds, http, clock: Arc::new(SystemClock::new()) }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: RESULT_HOSTS,
    models: flux_fill::MODELS,
    fields: &[API_KEY_FIELD],
    key_page: "https://replicate.com/account/api-tokens",
    note: "",
    build,
};

pub struct Prediction {
    pub output: Option<String>,
    pub get_url: Option<String>,
    pub cancel_url: Option<String>,
}

pub fn parse_upload(r: &HttpResponse, creds: &Credentials) -> Result<String> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    common::json(r)?.pointer("/urls/get").and_then(Value::as_str).map(str::to_owned).ok_or(GenError::BadResponse("no file in the reply"))
}

pub fn prediction_urls(r: &HttpResponse) -> Prediction {
    let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(str::to_owned);
    Prediction { output: s("/output").or_else(|| s("/output/0")), get_url: s("/urls/get"), cancel_url: s("/urls/cancel") }
}

pub fn parse_prediction(r: &HttpResponse, creds: &Credentials) -> Result<Step<String>> {
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = common::json(r)?;
    match v.get("status").and_then(Value::as_str) {
        Some("succeeded") => {
            let out = v.get("output").and_then(|o| o.as_str().or_else(|| o.pointer("/0").and_then(Value::as_str)));
            out.map(|u| Step::Done(u.to_owned())).ok_or(GenError::BadResponse("no image in the reply"))
        }
        Some("failed") => {
            let e = creds.scrub(v.get("error").and_then(Value::as_str).unwrap_or("the prediction failed"));
            let lower = e.to_ascii_lowercase();
            if ["nsfw", "flagged", "sensitive"].iter().any(|w| lower.contains(w)) { Err(GenError::Moderated(e)) } else { Err(GenError::Refused(e)) }
        }
        Some("canceled") => Err(GenError::Cancelled),
        _ => Ok(Step::Pending),
    }
}

impl Replicate {
    fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.creds.require(API_KEY)?.expose()))
    }

    fn upload(&self, png: &[u8], name: &str) -> Result<String> {
        let mut form = Form::new();
        form.file("content", name, "image/png", png);
        let (content_type, body) = form.finish();
        let r = self.http.send(HttpRequest::post(FILES, body).header("authorization", self.bearer()?).header("content-type", content_type))?;
        let url = parse_upload(&r, &self.creds)?;
        fetch::check_host(&url, API_HOSTS)?;
        Ok(url)
    }

    /// Best-effort removal of uploaded inputs (Replicate keeps files until deleted). The URLs
    /// passed [`fetch::check_host`] when uploaded; failures are ignored.
    fn delete_uploads(&self, urls: &[String]) {
        let Ok(bearer) = self.bearer() else { return };
        for url in urls {
            let _ = self.http.send(HttpRequest::delete(url.clone()).header("authorization", bearer.clone()));
        }
    }

    /// Upload, predict and download; each uploaded file's URL goes in `uploaded` as soon as it
    /// exists, so the caller can delete it however this ends.
    fn fill(&self, r: &EditRequest, fill: &[u8], uploaded: &mut Vec<String>) -> Result<GeneratedImage> {
        let image_url = self.upload(&r.image, "image.png")?;
        uploaded.push(image_url.clone());
        let mask_url = self.upload(fill, "mask.png")?;
        uploaded.push(mask_url.clone());
        r.cancel.check()?;
        let mut input = flux_fill::params(&r.prompt);
        input.insert("image".into(), json!(image_url));
        input.insert("mask".into(), json!(mask_url));
        input.insert("safety_tolerance".into(), json!(2));
        let bearer = self.bearer()?;
        let created = self
            .http
            .send(HttpRequest::post(PREDICT, Vec::new()).json(&json!({"input": input})).header("authorization", bearer.clone()).header("prefer", "wait=60"))?;
        let urls = prediction_urls(&created);
        let output = match parse_prediction(&created, &self.creds)? {
            Step::Done(url) => url,
            Step::Pending => {
                let get = urls.get_url.clone().ok_or(GenError::BadResponse("no prediction in the reply"))?;
                fetch::check_host(&get, API_HOSTS)?;
                let cancel = urls.cancel_url.clone().filter(|u| fetch::host_allowed(u, API_HOSTS));
                let waited = poll::until(
                    &*self.http,
                    &|| HttpRequest::get(get.clone()).header("authorization", bearer.clone()),
                    &mut |r| parse_prediction(r, &self.creds),
                    LIMIT,
                    &r.cancel,
                    &*self.clock,
                );
                on_cancel(waited, || {
                    if let Some(url) = &cancel {
                        let _ = self.http.send(HttpRequest::post(url.clone(), Vec::new()).header("authorization", bearer.clone()));
                    }
                })?
            }
        };
        r.cancel.check()?;
        common::image(fetch::result(&*self.http, &output, RESULT_HOSTS, Some(("authorization", bearer)))?)
    }
}

impl ImageProvider for Replicate {
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
        let mut uploaded = Vec::new();
        let result = self.fill(r, &fill, &mut uploaded);
        self.delete_uploads(&uploaded);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ApiKey, Method};
    use serde_json::json;

    const GET: &str = "https://api.replicate.com/v1/predictions/p1";
    const OUT: &str = "https://replicate.delivery/xezq/out.png";

    fn provider(replies: Vec<Result<HttpResponse>>) -> (Replicate, Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        (Replicate { creds: Credentials::api_key(ApiKey::new(SECRET).unwrap()), http: http.clone(), clock: Arc::new(FakeClock::default()) }, http)
    }

    fn uploaded(id: &str) -> Result<HttpResponse> {
        json_reply(201, json!({"id": id, "urls": {"get": format!("https://api.replicate.com/v1/files/{id}")}}))
    }

    #[test]
    fn uploads_inputs_predicts_with_a_sync_wait_and_downloads_with_the_token() {
        let (p, http) = provider(vec![
            uploaded("img"),
            uploaded("msk"),
            json_reply(201, json!({"id": "p1", "status": "succeeded", "output": OUT, "urls": {"get": GET, "cancel": format!("{GET}/cancel")}})),
            bytes_reply(200, "image/png", png(4, 4)),
        ]);
        p.edit(&edit_request("a pier", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!((sent[0].url.as_str(), sent[1].url.as_str()), (FILES, FILES));
        assert_eq!(header(&sent[0], "authorization"), Some(format!("Bearer {SECRET}").as_str()));
        assert_eq!(sent[2].url, PREDICT);
        assert_eq!(header(&sent[2], "prefer"), Some("wait=60"));
        let b: serde_json::Value = serde_json::from_slice(&sent[2].body).unwrap();
        assert_eq!(b["input"]["image"], "https://api.replicate.com/v1/files/img");
        assert_eq!(b["input"]["mask"], "https://api.replicate.com/v1/files/msk");
        assert_eq!(b["input"]["prompt"], "a pier");
        assert_eq!((sent[3].method, sent[3].url.as_str()), (Method::Get, OUT));
        assert_eq!(header(&sent[3], "authorization"), Some(format!("Bearer {SECRET}").as_str()), "Replicate's files need the token");
        assert_deleted(&sent[4..], &["img", "msk"]);
    }

    /// `sent` is exactly one DELETE, with the token, for each uploaded file.
    fn assert_deleted(sent: &[HttpRequest], ids: &[&str]) {
        let urls: Vec<_> = sent.iter().map(|r| (r.method, r.url.clone())).collect();
        let want: Vec<_> = ids.iter().map(|id| (Method::Delete, format!("https://api.replicate.com/v1/files/{id}"))).collect();
        assert_eq!(urls, want);
        for r in sent {
            assert_eq!(header(r, "authorization"), Some(format!("Bearer {SECRET}").as_str()));
        }
    }

    #[test]
    fn uploads_are_deleted_after_a_failure_and_delete_errors_are_ignored() {
        let failed = json_reply(201, json!({"id": "p1", "status": "failed", "error": "CUDA out of memory", "urls": {"get": GET}}));
        let (p, http) = provider(vec![uploaded("img"), uploaded("msk"), failed, json_reply(500, json!({})), json_reply(404, json!({}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Refused(_))), "the prediction's error, not the delete's");
        assert_deleted(&http.requests()[3..], &["img", "msk"]);
        // The mask upload fails: the image already uploaded is still deleted.
        let (p, http) = provider(vec![uploaded("img"), json_reply(401, json!({}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Auth));
        assert_deleted(&http.requests()[2..], &["img"]);
        // A successful fill whose deletes fail (the script has no replies left) still succeeds.
        let (p, http) = provider(vec![
            uploaded("img"),
            uploaded("msk"),
            json_reply(201, json!({"id": "p1", "status": "succeeded", "output": OUT})),
            bytes_reply(200, "image/png", png(4, 4)),
        ]);
        assert!(p.edit(&edit_request("x", 8, 8)).is_ok());
        assert_deleted(&http.requests()[4..], &["img", "msk"]);
    }

    #[test]
    fn an_empty_prompt_sends_the_description_not_an_instruction() {
        let (p, http) = provider(vec![uploaded("img"), uploaded("msk"), json_reply(401, json!({}))]);
        let _ = p.edit(&edit_request("", 8, 8));
        let b: serde_json::Value = serde_json::from_slice(&http.requests()[2].body).unwrap();
        assert_eq!(b["input"]["prompt"], crate::providers::common::CONTINUE_DESCRIPTION);
    }

    #[test]
    fn a_slow_prediction_is_polled_and_can_be_cancelled() {
        let (p, http) = provider(vec![
            uploaded("img"),
            uploaded("msk"),
            json_reply(201, json!({"id": "p1", "status": "starting", "urls": {"get": GET, "cancel": format!("{GET}/cancel")}})),
            json_reply(200, json!({"id": "p1", "status": "processing"})),
            json_reply(200, json!({})),
        ]);
        let sent = http.clone();
        let mut r = edit_request("x", 8, 8);
        r.cancel = crate::CancelToken::new(move || sent.requests().len() >= 4);
        assert_eq!(p.edit(&r), Err(GenError::Cancelled));
        let sent = http.requests();
        let cancel = &sent[sent.len() - 3];
        assert_eq!((cancel.method, cancel.url.as_str()), (Method::Post, format!("{GET}/cancel").as_str()));
        assert_deleted(&sent[sent.len() - 2..], &["img", "msk"]);
    }

    #[test]
    fn a_foreign_prediction_url_is_refused_without_sending_the_key() {
        let (p, http) = provider(vec![
            uploaded("img"),
            uploaded("msk"),
            json_reply(201, json!({"id": "p1", "status": "starting", "urls": {"get": "https://evil.example/p1"}})),
        ]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
        assert_eq!(http.requests().len(), 5);
        assert_deleted(&http.requests()[3..], &["img", "msk"]);
        let (p, http) = provider(vec![json_reply(201, json!({"urls": {"get": "https://evil.example/f"}}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn failures_and_flags() {
        let failed = |e: &str| json_reply(201, json!({"id": "p1", "status": "failed", "error": e, "urls": {"get": GET}}));
        let (p, _) = provider(vec![uploaded("img"), uploaded("msk"), failed("NSFW content detected")]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, _) = provider(vec![uploaded("img"), uploaded("msk"), failed("CUDA out of memory")]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Refused(_))));
        let (p, _) = provider(vec![json_reply(401, json!({"detail": "Unauthenticated"}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Auth));
    }
}
