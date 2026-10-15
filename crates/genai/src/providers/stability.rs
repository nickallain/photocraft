//! Stability AI Inpaint (`/v2beta/stable-image/edit/inpaint`): multipart, white = fill, the
//! image comes back as raw bytes (`Accept: image/*`) with a `finish-reason` header.

use std::sync::Arc;

use crate::providers::common::{self, http_error, pick_model, prompt_or_description, same_size};
use crate::{
    API_KEY, API_KEY_FIELD, Capabilities, Credentials, EditRequest, GenError, GeneratedImage, HttpRequest, HttpResponse, HttpTransport, ImageProvider,
    ProviderInfo, ProviderSpec, Result, check_prompt, mask, multipart::Form,
};

pub const ID: &str = "stability";
const NAME: &str = "Stability AI Inpaint";
const HOST: &str = "api.stability.ai";
pub(crate) const ENDPOINT: &str = "https://api.stability.ai/v2beta/stable-image/edit/inpaint";
const MODELS: &[(&str, &str)] = &[("inpaint", "Stable Image Inpaint")];

pub struct Stability {
    model: &'static str,
    creds: Credentials,
    http: Arc<dyn HttpTransport>,
}

fn build(model: &str, creds: Credentials, http: Arc<dyn HttpTransport>) -> Result<Arc<dyn ImageProvider>> {
    creds.require(API_KEY)?;
    Ok(Arc::new(Stability { model: pick_model(MODELS, model)?, creds, http }))
}

pub const SPEC: ProviderSpec = ProviderSpec {
    id: ID,
    name: NAME,
    host: HOST,
    result_hosts: &[],
    models: MODELS,
    fields: &[API_KEY_FIELD],
    key_page: "https://platform.stability.ai/account/keys",
    note: "",
    build,
};

pub fn parse(r: &HttpResponse, creds: &Credentials) -> Result<GeneratedImage> {
    match r.status {
        200..=299 => {
            if r.header("finish-reason").is_some_and(|f| f.eq_ignore_ascii_case("CONTENT_FILTERED")) {
                return Err(GenError::Moderated("Stability filtered the result".into()));
            }
            common::image(r.body.clone())
        }
        403 => Err(GenError::Moderated(common::error_text(&r.body, creds))),
        413 => Err(GenError::Invalid("the image is too large for Stability".into())),
        _ => Err(http_error(r, creds)),
    }
}

/// With no prompt the model only sees a description of the background; this keeps it from
/// painting lettering into the selection.
const NO_TEXT: &str = "text, letters, words, caption, watermark, logo";

impl ImageProvider for Stability {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { id: ID, name: NAME, model: self.model.into(), host: HOST }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { generate: false, edit: true, variations: false }
    }
    fn max_side(&self) -> u32 {
        1536
    }
    fn min_side(&self) -> u32 {
        512
    }
    fn edit(&self, r: &EditRequest) -> Result<GeneratedImage> {
        check_prompt(&r.prompt, false)?;
        same_size(&r.image, &r.mask)?;
        r.cancel.check()?;
        let image = mask::opaque_rgb(&r.image)?;
        let fill = mask::white_fill(&r.mask)?;
        let mut form = Form::new();
        form.text("prompt", prompt_or_description(&r.prompt)).text("output_format", "png");
        if r.prompt.trim().is_empty() {
            form.text("negative_prompt", NO_TEXT);
        }
        form.file("image", "image.png", "image/png", &image).file("mask", "mask.png", "image/png", &fill);
        let (content_type, body) = form.finish();
        let key = self.creds.require(API_KEY)?;
        let reply = self.http.send(
            HttpRequest::post(ENDPOINT, body)
                .header("authorization", format!("Bearer {}", key.expose()))
                .header("accept", "image/*")
                .header("content-type", content_type),
        )?;
        r.cancel.check()?;
        parse(&reply, &self.creds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiKey;
    use crate::testing::*;
    use serde_json::json;

    fn provider(replies: Vec<Result<HttpResponse>>) -> (std::sync::Arc<dyn ImageProvider>, std::sync::Arc<FakeHttp>) {
        let http = FakeHttp::new(replies);
        ((SPEC.build)("", Credentials::api_key(ApiKey::new(SECRET).unwrap()), http.clone()).unwrap(), http)
    }

    fn part(r: &crate::HttpRequest, name: &str) -> Option<Vec<u8>> {
        crate::multipart::parts(header(r, "content-type").unwrap(), &r.body).unwrap().into_iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    #[test]
    fn sends_an_rgb_image_and_a_white_fill_mask_and_reads_raw_bytes() {
        let (p, http) = provider(vec![bytes_reply(200, "image/png", png(8, 8))]);
        let out = p.edit(&edit_request("a fern", 8, 8)).unwrap();
        assert_eq!(out.mime, "image/png");
        let r = &http.requests()[0];
        assert_eq!(r.url, ENDPOINT);
        assert_eq!(header(r, "authorization"), Some(format!("Bearer {SECRET}").as_str()));
        assert_eq!(header(r, "accept"), Some("image/*"));
        assert_eq!(part(r, "prompt").unwrap(), b"a fern");
        assert_eq!(part(r, "output_format").unwrap(), b"png");
        let (_, _, m) = crate::mask::decode_rgba8(&part(r, "mask").unwrap()).unwrap();
        assert_eq!((m[0], m[7 * 4]), (255, 0), "white = fill");
        let image = part(r, "image").unwrap();
        let mut d = png::Decoder::new(std::io::Cursor::new(&image[..]));
        d.set_transformations(png::Transformations::IDENTITY);
        assert_eq!(d.read_info().unwrap().info().color_type, png::ColorType::Rgb, "alpha dropped to stay under 10 MiB");
        assert_eq!((p.max_side(), p.min_side()), (1536, 512));
    }

    #[test]
    fn filtered_results_and_403_are_moderation() {
        let filtered = with_header(bytes_reply(200, "image/png", png(2, 2)), "finish-reason", "CONTENT_FILTERED");
        let (p, _) = provider(vec![filtered]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated(_))));
        let (p, _) = provider(vec![json_reply(403, json!({"name": "content_moderation", "errors": ["flagged"]}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Moderated("flagged".into())));
        let (p, _) = provider(vec![json_reply(413, json!({"errors": ["payload too large"]}))]);
        assert!(matches!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Invalid(_))));
        let (p, _) = provider(vec![json_reply(401, json!({}))]);
        assert_eq!(p.edit(&edit_request("x", 8, 8)), Err(GenError::Auth));
    }

    #[test]
    fn an_empty_prompt_describes_the_background_and_rules_out_text() {
        let (p, http) = provider(vec![bytes_reply(200, "image/png", png(2, 2))]);
        p.edit(&edit_request("", 8, 8)).unwrap();
        let r = &http.requests()[0];
        assert_eq!(part(r, "prompt").unwrap(), common::CONTINUE_DESCRIPTION.as_bytes());
        assert_eq!(part(r, "negative_prompt").unwrap(), b"text, letters, words, caption, watermark, logo");
    }

    #[test]
    fn a_real_prompt_sends_no_negative_prompt() {
        let (p, http) = provider(vec![bytes_reply(200, "image/png", png(2, 2))]);
        p.edit(&edit_request("a fern", 8, 8)).unwrap();
        assert!(part(&http.requests()[0], "negative_prompt").is_none());
    }
}
