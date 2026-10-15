//! Ideogram 3.0 Inpaint (`/v1/ideogram-v3/inpaint`): multipart, **black = edit**, the reply
//! carries a temporary URL on ideogram.ai. It needs a prompt.

use std::sync::Arc;

use crate::providers::common::{self, http_error, json, pick_model, same_size};
use crate::{
    API_KEY, API_KEY_FIELD, Capabilities, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider,
    ProviderInfo, ProviderSpec, Result, check_prompt, fetch, mask, multipart::Form,
};

pub const ID: &str = "ideogram-v3";
const NAME: &str = "Ideogram 3.0 Inpaint";
const HOST: &str = "api.ideogram.ai";
pub(crate) const ENDPOINT: &str = "https://api.ideogram.ai/v1/ideogram-v3/inpaint";
pub const RESULT_HOSTS: &[&str] = &["ideogram.ai"];
const MODELS: &[(&str, &str)] = &[("V_3", "Ideogram 3.0")];

pub struct Ideogram {
    model: &'static str,
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(API_KEY)?;
    Ok(Arc::new(Ideogram { model: pick_model(MODELS, model)?, creds, http }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: RESULT_HOSTS,
    models: MODELS,
    fields: &[API_KEY_FIELD],
    key_page: "https://ideogram.ai/manage-api",
    note: "",
    build,
};

/// The result's URL from a reply.
pub fn parse(r: &HttpResponse, creds: &Credentials) -> Result<String> {
    if r.status == 422 {
        return Err(GenError::Moderated(common::error_text(&r.body, creds)));
    }
    if !r.is_success() {
        return Err(http_error(r, creds));
    }
    let v = json(r)?;
    if v.pointer("/data/0/is_image_safe").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err(GenError::Moderated("Ideogram marked the result unsafe".into()));
    }
    v.pointer("/data/0/url")
        .and_then(serde_json::Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
        .ok_or(GenError::BadResponse("no image in the reply"))
}

impl ImageProvider for Ideogram {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: ID, name: NAME, model: self.model.into(), host: HOST }
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
        let fill = mask::black_fill(&r.mask)?;
        let mut form = Form::new();
        form.text("prompt", r.prompt.trim()).file("image", "image.png", "image/png", &r.image).file("mask", "mask.png", "image/png", &fill);
        let (content_type, body) = form.finish();
        let key = self.creds.require(API_KEY)?;
        let reply = self.http.send(HttpRequest::post(ENDPOINT, body).header("api-key", key.expose()).header("content-type", content_type))?;
        let url = parse(&reply, &self.creds)?;
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

    fn provider(replies: Vec<Result<HttpResponse>>) -> (std::sync::Arc<dyn ImageProvider>, std::sync::Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        ((SPEC.build)("", Credentials::api_key(ApiKey::new(SECRET).unwrap()), http.clone()).unwrap(), http)
    }

    fn part(r: &crate::HttpRequest, name: &str) -> Option<Vec<u8>> {
        crate::multipart::parts(header(r, "content-type").unwrap(), &r.body).unwrap().into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    #[test]
    fn sends_a_black_fill_mask_and_downloads_the_result_without_the_key() {
        let reply = json_reply(200, json!({"data": [{"url": "https://ideogram.ai/api/images/ephemeral/abc.png", "is_image_safe": true}]}));
        let (p, http) = provider(vec![reply, bytes_reply(200, "image/png", png(8, 8))]);
        p.edit(&edit_request("a lighthouse", 8, 8)).unwrap();
        let sent = http.requests();
        assert_eq!(sent[0].url, ENDPOINT);
        assert_eq!(header(&sent[0], "api-key"), Some(SECRET));
        assert_eq!(part(&sent[0], "prompt").unwrap(), b"a lighthouse");
        let (_, _, m) = crate::mask::decode_rgba8(&part(&sent[0], "mask").unwrap()).unwrap();
        assert_eq!((m[0], m[7 * 4]), (0, 255), "Ideogram: black = edit");
        assert_eq!((sent[1].method, sent[1].url.as_str()), (Method::Get, "https://ideogram.ai/api/images/ephemeral/abc.png"));
        assert!(header(&sent[1], "api-key").is_none(), "the key isn't sent with the download");
    }

    #[test]
    fn needs_a_prompt_and_refuses_before_sending() {
        let (p, http) = provider(vec![]);
        assert!(p.needs_prompt());
        assert!(matches!(p.edit(&edit_request("  ", 8, 8)), Err(GenError::Invalid(_))));
        assert!(http.requests().is_empty());
    }

    #[test]
    fn unsafe_results_and_422_are_moderation_and_foreign_urls_are_refused() {
        let (p, _) = provider(vec![json_reply(200, json!({"data": [{"url": "", "is_image_safe": false}]}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, _) = provider(vec![json_reply(422, json!({"detail": "prompt failed safety check"}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, http) = provider(vec![json_reply(200, json!({"data": [{"url": "https://evil.example/x.png", "is_image_safe": true}]}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::BadResponse(_))));
        assert_eq!(http.requests().len(), 1, "nothing sent to the foreign host");
    }
}
