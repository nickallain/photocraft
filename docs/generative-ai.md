# Generative AI

PhotoCraft can fill a selection from a prompt (Edit › Generative Fill…) through one of nine image
services the user picks and pays for with their own key. It is off by default: until a provider is set
up, the command is hidden, nothing is contacted, and no key is read. This page follows the shape
the maintainers asked for in issue #41.

## Using it

1. Edit › Preferences › AI Integrations… › Generative AI Settings… (or the ▾ next to the selection
   tools' Generative Fill… button). The list shows every provider; set up the ones you use (their
   credentials, and a model where there is a choice), mark one as the default, and read what is
   sent before pressing OK.
2. Make a selection, then Edit › Generative Fill… (or the options bar button). Pick the provider
   for this fill (the default is preselected; only set-up providers are offered). Describe what to
   put there, or leave the prompt empty to continue the surrounding image through it (anything
   that runs into the selection carries on and partly covered objects are completed).
3. Three variations come back, as in Photoshop. They land in a new group named after the prompt
   and masked by the selection, one layer per variation (Variation 1, 2, 3), with the first
   showing. Nothing under it changes; one undo removes it. The requests run side by side as one
   background job that can be cancelled; if some fail and others succeed, the successes are kept.
4. With the group (or one of its variations) active, Properties › Variations shows a thumbnail
   per variation: click one to show it alone (`layer.generative.selectVariation`, one undo step).
   Generate More sends the same image, mask and prompt again and adds three more
   (`layer.generative.more`), to the provider that made the group unless another is chosen.

Photoshop keeps its variations inside one generative layer. PhotoCraft uses a group of ordinary
layers instead, so a PSD round trip needs no format changes and other apps see plain layers. What
each group was generated from is kept in the session only: after reopening a file the group is an
ordinary group (the variations are all still there; show one by hand), and Generate More is off.

## Reference images

Generative Fill can add a particular subject, such as a person, into the selection. In the fill
dialog, press Add reference… in the Reference row and choose one image file (PNG, JPEG, WebP,
HEIC, TIFF, BMP, GIF or PSD). A thumbnail shows the image as the provider will get it, and × removes
it. Leave the prompt empty and the request is simply "add the subject from the reference into the
selection". The reference is remembered for the session only, so the next fill starts with it
until it is removed, and is never saved in the document.

- **Providers.** OpenAI GPT Image and Google Gemini (every model except Nano Banana 2 Lite, whose
  API takes no reference). The other providers have no reference input in their APIs; Ideogram's
  character reference exists only on a legacy endpoint that is being removed, so PhotoCraft uses
  the current one without it. For a provider that can't use a reference, the dialog shows a note
  and Generate stays disabled until the reference is removed or another provider is chosen; the
  Reference row is shown only when some set-up provider supports it.
- **What is sent.** The file is flattened, converted to sRGB and scaled down to at most 1024 px on
  the long side (never enlarged), with transparent areas turned white, then encoded as an opaque
  PNG. OpenAI gets the photo and then the reference as two `image[]` parts, with a sentence in the
  prompt saying the second image is the reference. Gemini gets the reference as the last input
  image, with a matching sentence. Files over 50 MB, images over 100 megapixels, and images whose
  long side is under 64 px are refused with an error.
- **Privacy.** The reference goes to the chosen provider with the request (once per variation, like
  the photo) and nowhere else. It is not stored in the document or written to logs; only its path
  appears in the command's params. With `PHOTOCRAFT_GENAI_DUMP` set, the request folder also holds
  `sent-reference.png`.
- **Results.** Likeness is good but not exact; generating a few variations, or Generate More,
  helps. Generate More reuses the reference of the original fill; passing `reference` replaces it.
- **Policies.** The providers' own policies on real people's likenesses apply. A request they
  decline comes back as an error from the provider, and nothing is added.

## Providers

Nine providers are built in; none is privileged and all are off until their credentials are
entered. Gemini (the Nano Banana image models, default `gemini-nano-banana-2.1`) takes its key from
[Google AI Studio](https://aistudio.google.com/apikey).

| Provider (id) | Credentials | Sent to | Results from | Prompt | Retention (vendor docs, 2026-10) |
|---|---|---|---|---|---|
| Google Gemini (`gemini`) | API key | generativelanguage.googleapis.com | inline | optional | per Google's terms |
| FLUX.1 Fill [pro] · Black Forest Labs (`flux-fill.bfl`) | API key | `api.bfl.ai`, regional `*.bfl.ai` polling hosts | `*.bfl.ai` | optional (FLUX Erase without) | result URLs expire after 10 min |
| FLUX.1 Fill [pro] · fal (`flux-fill.fal`) | API key | queue.fal.run | `fal.media`, `*.fal.media` | optional | per the account's media-expiry setting |
| FLUX.1 Fill [pro] · Replicate (`flux-fill.replicate`) | API token | api.replicate.com | `replicate.delivery`, `*.replicate.delivery` | optional | uploaded inputs deleted by PhotoCraft after each fill; prediction data per Replicate's policy (1 h) |
| Adobe Firefly Fill (`firefly`) | client ID + secret | ims-na1.adobelogin.com, firefly-api.adobe.io | *.amazonaws.com (pre-signed) | required | uploads kept 7 days |
| Ideogram 3.0 Inpaint (`ideogram-v3`) | API key | api.ideogram.ai (`/v2/image/inpaint/ideogram-3`) | ideogram.ai | required | temporary URLs |
| Bria Gen Fill and Erase (`bria`) | API token | engine.prod.bria-api.com | `*.bria-api.com` / `*.bria.ai` | optional (Erase without) | temporary URLs |
| Stability AI Inpaint (`stability`) | API key | api.stability.ai | inline | optional | per Stability's terms |
| OpenAI GPT Image (`openai`) | API key | api.openai.com | inline | optional | per OpenAI's terms |

"Prompt" says what happens when the prompt is empty. Stability, fal and Replicate are sent a
neutral description of a plain continuation of the background (not an instruction, which the
diffusion models draw as text); OpenAI is sent a continue instruction; Black Forest Labs and Bria
switch to their Erase endpoints; Firefly and Ideogram need a prompt, so the dialog asks for one.

Rules every provider follows:

- Credentials are stored per provider in the OS credential store. A provider with several fields
  (Firefly's client ID and secret) keeps them as one JSON secret under one entry.
- A request goes only to the hosts in the "Sent to" column. Where a service answers with a URL to
  poll or cancel, its host is checked against those hosts before any credential is sent to it, and
  a result is downloaded only from hosts matching the "Results from" entries (`*.x` = any
  subdomain of `x`, not `x` itself; an entry without `*` is that exact host). Anything else is an
  error. Replicate also sends its token with the download, to its own delivery hosts.
- Requests run inside the cancellable job: cancelling stops polling and skips the download; a
  request already in flight finishes and its result is discarded.

**Verified live (2026-10-12).** OpenAI, Stability, Bria and Black Forest Labs were exercised
against the real services (a prompt fill and a promptless removal each). Ideogram was only tried
against its older endpoint, where the test key was rejected; the current endpoint it now uses
(`/v2/image/inpaint/ideogram-3`) has not yet been checked live. fal, Replicate, Adobe Firefly and Gemini are covered by
unit tests with recorded responses only.

## What is sent

Only when Generative Fill or Generate More runs, and only to the chosen provider, once per
variation (so three requests by default, each billed to the user's key):

- the selection's bounds plus a margin of context (half the selection's longer side, 64 to
  1024 px), grown toward the provider's output shape and kept inside the canvas, rendered
  from all visible layers in sRGB and scaled to at most 2048 px on the long side, with the
  selection itself replaced by a soft blur of the colours around it. The provider never sees
  what is being replaced, as Photoshop's fill doesn't, so it can't paint it back: an empty prompt
  removes what was selected and continues the surroundings, including anything that crosses the
  selection's edge. (A flat grey hole was tried first; Gemini drew it in as a grey box or a lamp.);
- a black-and-white PNG mask of the selection at the same size (white = fill);
- the prompt.

Providers with a mask parameter get the mask as such (polarity and format adapted per provider);
Gemini takes none, so the mask is a second image and the instruction says what it means. PhotoCraft keeps only the masked pixels of the result, so anything the model changes
outside the selection is discarded. The model redraws the whole image and relights it,
differently each time (Gemini often lifts dark shadows to nearly double), which would leave a
seam at the selection's edge; the area outside the selection should come back unchanged, so
comparing it with the original measures the change (`match_surroundings`). A tone curve on
brightness, hue kept, undoes the relighting; then what is left is carried smoothly inward from
the edge by push-pull. Pixels the model actually changed (different fine detail, as where it
moved an object, or a far different colour) are left out of both. A result that doesn't resemble
the original around the selection (over 25% mean difference) is left as it came.

To debug a provider, start the app with `PHOTOCRAFT_GENAI_DUMP=<folder>`: each request's image,
mask, prompt, raw results and colour-matching figures are written to a new subfolder there. Nothing
is written without it. For tuning, the ignored test `live_fill` in `apps/photocraft/src/genai.rs`
runs one fill from the command line with the saved key, or replays saved results without a
request (`PHOTOCRAFT_LIVE_REPLAY`). Gemini returns JPEG (its only output format) and its results carry Google's SynthID watermark.

## Keys and privacy

- Credentials live in the OS credential store: the macOS Keychain, Windows Credential Manager. On
  Linux and the BSDs they stay in process memory and are asked for again after a restart; they are
  never written to a file. Multi-field credentials are one JSON secret per provider.
- Preferences hold only the default provider id, the list of configured provider ids and
  per-provider model
  (`dialogs["ui.generativeAiSettings"]`).
- Credential fields are not dialog fields, so `ui.inspect` and the control channel never see them.
  An `ApiKey` never prints (`Debug` is redacted), errors never carry a response body, and a
  service's own error text is passed on only after every credential value is scrubbed from it.
- Credentials go in request headers (or a body field the service requires), never in a URL. The
  transport is HTTPS only and follows no redirects. Response URLs are host-checked before
  credentials are sent, and results are downloaded only from each provider's listed hosts (see
  Providers).

## Automation

`edit.generativeFill` is a normal engine command (one undo step, a cancellable job, `prompt`,
`variations` (1 to 4, default 3), `provider` and `reference` params) so scripts and recorded actions can run it locally, but
automation (the control channel and MCP) may not: `authorize_engine_command` refuses it and
`layer.generative.more` (which also takes `provider` and `reference`), because they upload document pixels. `layer.generative.selectVariation`
sends nothing and is allowed. An
explicit opt-in for automation can come later.

## Code

| Where | What |
|---|---|
| `crates/genai` | `ImageProvider` (generate, edit with mask, variations; capabilities; output shape), `HttpTransport` (the network seam), `ApiKey` and `Credentials`, the provider registry and the nine backends. No network code: it builds for wasm and its tests use a fake transport. |
| `crates/engine/src/genai_cmds.rs` | `edit.generativeFill`, `layer.generative.selectVariation`, `layer.generative.more`: context area, pixel interchange (after #730's approach), the job, the masked group of variations. `Session::set_image_provider` installs a provider. |
| `crates/ui-egui/src/genai_ui.rs` | Generative AI Settings, the prompt dialog, the options bar button, Properties › Variations, `restore` at launch. |
| `apps/photocraft/src/genai.rs` | The desktop `GenAiServices`: credential store (`keyring`) and HTTPS transport (`ureq` + rustls). |

### Adding a provider

1. Add a module in `crates/genai/src/providers/` implementing `ImageProvider` over an
   `HttpTransport`. Reuse `providers/common.rs` and the shared helpers `fetch.rs` (host-checked
   result download), `poll.rs` (cancellable polling), `mask.rs` (mask polarity and format) and
   `multipart.rs` (multipart bodies).
2. Register it with a `ProviderSpec` in `crates/genai/src/registry.rs`. The settings dialog, the
   per-fill chooser, the `provider` command param and the credential store read the registry.
3. Test it against a fake transport with recorded responses, as the other providers do, and add a
   fuzz target for its response parsing in `crates/genai/fuzz/fuzz_targets/`.
4. Add a row to the Providers table above.

No backend is privileged; the engine only sees the trait.
