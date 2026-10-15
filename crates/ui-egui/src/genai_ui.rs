//! Generative AI in the shell (#41): Generative AI Settings (every provider, each set up in its
//! own dialog with its model and credentials, and the default), the Edit › Generative Fill
//! prompt, and installing the configured providers on the session.
//!
//! Off by default: until the user sets up a provider, none exists, the Generative Fill menu item
//! is hidden and nothing is ever contacted. Which providers are set up, their models and the
//! default live in preferences (shell-owned `dialogs` entry); credentials live only in the OS
//! credential store ([`crate::GenAiServices`]) and in process memory. Credentials are typed into
//! buffers outside the dialog's fields, so `ui.inspect` and the control channel never see them.

use photocraft_doc::{Layer, LayerId};
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::state::DialogKind;

/// Opens Generative AI Settings.
pub const SETTINGS: &str = "ui.generativeAiSettings";
const FILL: &str = photocraft_engine::genai_cmds::FILL;
const SELECT_VARIATION: &str = photocraft_engine::genai_cmds::SELECT_VARIATION;
const MORE: &str = photocraft_engine::genai_cmds::MORE;
/// Long side of a variation thumbnail, in texels.
const THUMB: u32 = 96;
const SETTINGS_MARK: &str = "__genaiSettings";
const PROMPT_MARK: &str = "__genaiPrompt";
/// `prefs.dialogs` entry holding [`Saved`].
const PREF: &str = "ui.generativeAiSettings";
const PROVIDER_MARK: &str = "__genaiProvider";

/// Shell state that must not be serialised or inspected.
#[derive(Default)]
pub struct Runtime {
    /// What is typed in a provider dialog's credential fields, by field name. Cleared when the
    /// dialog opens, closes or saves.
    key_inputs: std::collections::HashMap<&'static str, String>,
    /// Variation thumbnails for the Properties panel, by variation layer: (pixels fingerprint,
    /// texture).
    thumbs: std::collections::HashMap<LayerId, (u64, egui::TextureHandle)>,
    /// The provider last chosen in the prompt dialog, this session.
    last_provider: Option<String>,
    /// The reference image last used in the prompt dialog, this session.
    pub(crate) last_reference: Option<String>,
    /// The reference row's thumbnail: (path, texture).
    reference_thumb: Option<(String, egui::TextureHandle)>,
    /// The reference as the pick read it (path, PNG bytes), so the thumbnail doesn't read it again.
    reference_png: Option<(String, Vec<u8>)>,
    /// A reference path whose thumbnail couldn't be made, so it isn't retried every frame.
    reference_thumb_failed: Option<String>,
}

/// The file types Add reference… offers.
pub(crate) const REFERENCE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "heic", "heif", "tif", "tiff", "bmp", "gif", "psd"];

/// What Generative AI Settings keeps in `prefs.dialogs[PREF]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Saved {
    pub default: String,
    pub models: std::collections::BTreeMap<String, String>,
    pub configured: Vec<String>,
}

impl Saved {
    pub(crate) fn to_json(&self) -> Value {
        json!({"default": self.default, "models": self.models, "configured": self.configured})
    }
}

/// Reads the saved settings, including the single-provider `{"provider", "model"}` form.
pub(crate) fn parse_saved(v: Option<&Value>) -> Saved {
    let Some(v) = v else { return Saved::default() };
    if let Some(p) = v.get("provider").and_then(Value::as_str) {
        if p.is_empty() || photocraft_genai::spec(p).is_none() {
            return Saved::default();
        }
        let model = v.get("model").and_then(Value::as_str).unwrap_or("").to_owned();
        return Saved { default: p.into(), models: [(p.to_owned(), model)].into(), configured: vec![p.into()] };
    }
    let configured: Vec<String> = v
        .get("configured")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|id| photocraft_genai::spec(id).is_some()).map(str::to_owned).collect())
        .unwrap_or_default();
    let models = v
        .get("models")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
        .unwrap_or_default();
    let mut default = v.get("default").and_then(Value::as_str).unwrap_or("").to_owned();
    if !configured.contains(&default) {
        default = configured.first().cloned().unwrap_or_default();
    }
    Saved { default, models, configured }
}

fn saved(app: &PhotocraftApp) -> Saved {
    parse_saved(app.session.prefs().dialogs.get(PREF))
}

fn store_saved(app: &mut PhotocraftApp, saved: &Saved) {
    let v = saved.to_json();
    app.session.prefs.edit(|p| p.dialogs.insert(PREF.into(), v));
}

fn provider_name(id: &str) -> &str {
    photocraft_genai::spec(id).map_or(id, |s| s.name)
}

/// Build every configured provider and install them (at launch and after any settings change).
/// A provider whose credentials are missing or unreadable is skipped and named in the error; the
/// rest still work. Reads the credential store only for configured providers; never touches the
/// network.
pub fn restore(app: &mut PhotocraftApp) -> Result<(), String> {
    let saved = saved(app);
    let Some(services) = app.services.genai.as_ref() else {
        return Ok(());
    };
    let mut built = std::collections::BTreeMap::new();
    let mut problems = Vec::new();
    for id in &saved.configured {
        let model = saved.models.get(id).map_or("", String::as_str);
        let creds = match (services.load_key)(id) {
            Ok(secret) => secret.as_deref().and_then(photocraft_genai::Credentials::from_secret),
            Err(e) => {
                problems.push(format!("{id}: {e}"));
                continue;
            }
        };
        let Some(creds) = creds else {
            problems.push(crate::i18n::fmt(tl!("No API key is saved for {provider}."), &[("provider", id)]));
            continue;
        };
        match (services.provider)(id, model, creds) {
            Ok(p) => {
                built.insert(id.clone(), p);
            }
            Err(e) => problems.push(format!("{id}: {e}")),
        }
    }
    let default = Some(saved.default.clone()).filter(|d| built.contains_key(d));
    app.session.set_image_providers(built, default);
    if problems.is_empty() { Ok(()) } else { Err(problems.join(" ")) }
}

/// [`restore`] after a settings change that itself succeeded: the providers that can't be
/// installed (another provider's credentials unreadable, say) are reported in the status line
/// rather than turning the save into an error.
fn restore_after_save(app: &mut PhotocraftApp) {
    if let Err(e) = restore(app) {
        app.ui.status = e;
    }
}

/// A provider's status in Generative AI Settings: installed, configured but not installed (its
/// credentials are missing or unreadable), or not configured.
pub(crate) fn row_status(app: &PhotocraftApp, saved: &Saved, id: &str) -> &'static str {
    if !saved.configured.iter().any(|c| c == id) {
        tl!("Not set up")
    } else if app.session.image_provider_by_id(id).is_some() {
        tl!("Set up")
    } else {
        tl!("Needs credentials")
    }
}

/// Menu, palette and shortcut entry points. Clicking Generative Fill asks for a prompt; with
/// `prompt` already in the params (scripted use) it runs directly.
pub fn menu(app: &mut PhotocraftApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    match id {
        SETTINGS => Some(Ok(json!({"dialog": open_settings(app)}))),
        FILL if params.get("prompt").is_none() => {
            if app.session.image_provider().is_none() {
                return Some(Ok(json!({"dialog": open_settings(app)})));
            }
            if let Some(why) = app.session.disabled_reason(FILL) {
                return Some(Err(why));
            }
            Some(Ok(json!({"dialog": open_prompt(app)})))
        }
        _ => None,
    }
}

/// Is the Edit › Generative Fill item shown? Only once a provider can run it (#41: no
/// "coming soon" items).
pub fn menu_visible(app: &PhotocraftApp) -> bool {
    app.session.image_provider().is_some_and(|p| p.capabilities().edit)
}

pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.contains_key(SETTINGS_MARK) || fields.contains_key(PROMPT_MARK) || fields.contains_key(PROVIDER_MARK)
}

pub fn ok_label(fields: &Map<String, Value>) -> Option<&'static str> {
    if fields.contains_key(PROMPT_MARK) {
        Some(tl!("Generate"))
    } else if fields.contains_key(PROVIDER_MARK) {
        Some(tl!("Save"))
    } else {
        None
    }
}

pub fn open_settings(app: &mut PhotocraftApp) -> u64 {
    if let Some(d) = app.ui.dialogs.iter().find(|d| d.fields.contains_key(SETTINGS_MARK)) {
        return d.id;
    }
    let saved = saved(app);
    let mut f = Map::new();
    f.insert(SETTINGS_MARK.into(), json!(true));
    f.insert("__label".into(), json!("Generative AI Settings"));
    f.insert("default".into(), json!(saved.default));
    app.ui.open_dialog(DialogKind::Command, f)
}

/// The Set Up / Edit dialog for one provider. `None` for an unknown id.
pub(crate) fn open_provider(app: &mut PhotocraftApp, id: &str) -> Option<u64> {
    let spec = photocraft_genai::spec(id)?;
    if let Some(d) = app.ui.dialogs.iter().find(|d| d.fields.get(PROVIDER_MARK).and_then(Value::as_str) == Some(spec.id)) {
        return Some(d.id);
    }
    app.genai.key_inputs.clear();
    let saved = saved(app);
    let model = saved.models.get(spec.id).filter(|m| !m.is_empty()).cloned().or_else(|| spec.models.first().map(|m| m.0.to_owned())).unwrap_or_default();
    let has_key = app.services.genai.as_ref().and_then(|s| (s.load_key)(spec.id).ok().flatten()).is_some();
    let mut f = Map::new();
    f.insert(PROVIDER_MARK.into(), json!(spec.id));
    f.insert("__label".into(), json!(spec.name));
    f.insert("model".into(), json!(model));
    f.insert("hasKey".into(), json!(has_key));
    Some(app.ui.open_dialog(DialogKind::Command, f))
}

pub(crate) fn remove_provider(app: &mut PhotocraftApp, id: &str) -> Result<(), String> {
    if let Some(s) = app.services.genai.as_ref() {
        (s.delete_key)(id)?;
    }
    let mut saved = saved(app);
    saved.configured.retain(|c| c != id);
    saved.models.remove(id);
    if saved.default == id {
        saved.default = saved.configured.first().cloned().unwrap_or_default();
    }
    store_saved(app, &saved);
    restore(app)
}

fn open_prompt(app: &mut PhotocraftApp) -> u64 {
    if let Some(d) = app.ui.dialogs.iter().find(|d| d.fields.contains_key(PROMPT_MARK)) {
        return d.id;
    }
    let last = app.session.prefs().dialogs.get(FILL).and_then(|v| v.get("prompt")).and_then(Value::as_str).unwrap_or("").to_owned();
    let mut f = Map::new();
    f.insert(PROMPT_MARK.into(), json!(true));
    f.insert("__label".into(), json!("Generative Fill"));
    f.insert("prompt".into(), json!(last));
    let ids = app.session.image_provider_ids();
    let chosen = app
        .genai
        .last_provider
        .clone()
        .filter(|p| ids.contains(p))
        .or_else(|| app.session.image_provider().map(|p| p.info().id.to_owned()))
        .unwrap_or_default();
    f.insert("provider".into(), json!(chosen));
    // Only when the row to see and remove it is shown.
    let reference = app.genai.last_reference.clone().filter(|_| reference_row_visible(app)).unwrap_or_default();
    f.insert("reference".into(), json!(reference));
    app.ui.open_dialog(DialogKind::Command, f)
}

/// Is the reference row shown? Only when a configured provider can use a reference.
pub(crate) fn reference_row_visible(app: &PhotocraftApp) -> bool {
    app.session.image_provider_ids().iter().any(|id| app.session.image_provider_by_id(id).is_some_and(|p| p.supports_reference()))
}

/// Add reference…: a file picker; the chosen path goes into the prompt dialog's `reference` field.
pub(crate) fn pick_reference(app: &mut PhotocraftApp, dialog: u64) -> Result<Value, String> {
    let request = crate::file_dialog::FileDialogRequest::Open {
        multiple: false,
        initial_dir: None,
        extensions: Some(REFERENCE_EXTENSIONS.iter().map(|e| (*e).to_owned()).collect()),
    };
    app.ask_file(request, move |app, answer| {
        let path = match answer {
            crate::file_dialog::FileDialogAnswer::Paths(p) => p.into_iter().next().ok_or_else(|| tl!("No file was chosen.").to_string())?,
            _ => return Err(tl!("Reference images need the desktop app.").to_string()),
        };
        // Load once now so a bad file is reported at once (and the thumbnail can be made).
        let png = photocraft_engine::genai_cmds::load_reference(&path)?;
        app.genai.reference_png = Some((path.clone(), png));
        // A re-pick of the same path may be a different file now.
        app.genai.reference_thumb = None;
        app.genai.reference_thumb_failed = None;
        if let Some(d) = app.ui.dialogs.iter_mut().find(|d| d.id == dialog) {
            d.fields.insert("reference".into(), json!(path));
        }
        Ok(json!({"reference": path}))
    })
}

/// Remove the reference: from the dialog and from what the next prompt dialog starts with.
pub(crate) fn clear_reference(app: &mut PhotocraftApp, f: &mut Map<String, Value>) {
    f.insert("reference".into(), json!(""));
    app.genai.last_reference = None;
    app.genai.reference_thumb = None;
    app.genai.reference_png = None;
    app.genai.reference_thumb_failed = None;
}

/// Add reference… clicked: open the picker for the prompt dialog. A cancel is not reported.
pub(crate) fn add_reference_clicked(app: &mut PhotocraftApp) {
    let Some(id) = app.ui.dialogs.iter().find(|d| d.fields.contains_key(PROMPT_MARK)).map(|d| d.id) else { return };
    if let Err(e) = pick_reference(app, id)
        && e != crate::file_dialog::CANCELLED
    {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

fn chosen_reference(f: &Map<String, Value>) -> Option<&str> {
    f.get("reference").and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The reference row's thumbnail: the image as the provider gets it. `None` when it can't be read
/// (the fill then reports why).
fn reference_thumb(app: &mut PhotocraftApp, ctx: &egui::Context, path: &str) -> Option<egui::TextureHandle> {
    if let Some((p, tex)) = &app.genai.reference_thumb
        && p == path
    {
        return Some(tex.clone());
    }
    if app.genai.reference_thumb_failed.as_deref() == Some(path) {
        return None;
    }
    // The bytes the pick read when they are for this path; otherwise (a remembered reference) read it.
    let png = match app.genai.reference_png.take() {
        Some((p, png)) if p == path => Some(png),
        _ => photocraft_engine::genai_cmds::load_reference(path).ok(),
    };
    let img = png.and_then(|png| photocraft_codecs::decode(&png).ok()).and_then(|img| {
        let size = [usize::try_from(img.width()).ok()?, usize::try_from(img.height()).ok()?];
        let rgba = img.to_rgba8();
        (rgba.len() == size[0] * size[1] * 4).then(|| egui::ColorImage::from_rgba_unmultiplied(size, &rgba))
    });
    let Some(img) = img else {
        app.genai.reference_thumb_failed = Some(path.to_owned());
        return None;
    };
    let tex = ctx.load_texture("genai-reference", img, egui::TextureOptions::LINEAR);
    app.genai.reference_thumb = Some((path.to_owned(), tex.clone()));
    app.genai.reference_thumb_failed = None;
    Some(tex)
}

pub fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    if f.contains_key(PROMPT_MARK) {
        prompt_body(app, ui, f);
    } else if f.contains_key(PROVIDER_MARK) {
        provider_body(app, ui, f);
    } else {
        settings_body(app, ui, f);
    }
}

fn faint(ui: &mut egui::Ui, text: &str) {
    let t = crate::theme::Tokens::get(ui.ctx());
    ui.add(egui::Label::new(egui::RichText::new(text).color(t.text_faint).size(12.0)).wrap());
}

fn prompt_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let mut prompt = f.get("prompt").and_then(Value::as_str).unwrap_or("").to_owned();
    ui.label(tl!("Describe what to generate in the selection, or leave it empty to continue the image through it."));
    ui.add_space(6.0);
    let r = ui
        .add(egui::TextEdit::singleline(&mut prompt).hint_text(tl!("Prompt (optional)")).desired_width(f32::INFINITY).char_limit(photocraft_genai::MAX_PROMPT));
    crate::field_tab::register(ui.ctx(), r.id);
    if !f.contains_key("__focused") {
        f.insert("__focused".into(), json!(true));
        crate::field_tab::focus(ui.ctx(), r.id);
    }
    if r.gained_focus() {
        crate::field_tab::select_all(ui.ctx(), r.id, &prompt);
    }
    if r.changed() {
        f.insert("prompt".into(), json!(prompt));
    }
    ui.add_space(6.0);
    let ids = app.session.image_provider_ids();
    let mut chosen = f.get("provider").and_then(Value::as_str).unwrap_or("").to_owned();
    if ids.len() > 1 {
        let opts: Vec<(String, &str)> = ids.iter().map(|id| (id.clone(), provider_name(id))).collect();
        ui.horizontal(|ui| {
            ui.label(tl!("Provider"));
            if crate::widgets::dropdown(ui, "genai-fill-provider", &mut chosen, &opts, 260.0) {
                f.insert("provider".into(), json!(chosen));
            }
        });
        ui.add_space(6.0);
    }
    if reference_row_visible(app) || chosen_reference(f).is_some() {
        ui.horizontal(|ui| {
            ui.label(tl!("Reference"));
            match chosen_reference(f).map(str::to_owned) {
                None => {
                    if crate::widgets::secondary_button(ui, tl!("Add reference…"), 0.0).clicked() {
                        add_reference_clicked(app);
                    }
                }
                Some(path) => {
                    if let Some(tex) = reference_thumb(app, ui.ctx(), &path) {
                        // Fit in 96 × 48 (a panorama would otherwise widen the dialog).
                        let size = tex.size_vec2();
                        let k = (96.0 / size.x.max(1.0)).min(48.0 / size.y.max(1.0));
                        ui.image((tex.id(), size * k));
                    }
                    let name = std::path::Path::new(&path).file_name().map_or(path.clone(), |n| n.to_string_lossy().into_owned());
                    ui.label(name).on_hover_text(&path);
                    if crate::icons::button(ui, "x", 18.0, false, tl!("Remove the reference")).clicked() {
                        clear_reference(app, f);
                    }
                }
            }
        });
        ui.add_space(6.0);
    }
    let reference = chosen_reference(f).is_some();
    if let Some(p) = app.session.image_provider_by_id(&chosen).or_else(|| app.session.image_provider()) {
        let info = p.info();
        if reference && !p.supports_reference() {
            faint(
                ui,
                &crate::i18n::fmt(
                    tl!("{provider} can't use a reference image. Choose OpenAI or Gemini, or remove the reference."),
                    &[("provider", &format!("{} ({})", info.name, info.model))],
                ),
            );
        }
        if p.needs_prompt() && prompt.trim().is_empty() {
            faint(ui, &crate::i18n::fmt(tl!("{provider} needs a prompt."), &[("provider", info.name)]));
        }
        let sent = if reference {
            tl!("The selected area, the image around it, the reference image and the prompt are sent to {name} ({model}).")
        } else {
            tl!("The selected area, the image around it and the prompt are sent to {name} ({model}).")
        };
        faint(ui, &crate::i18n::fmt(sent, &[("name", info.name), ("model", &info.model)]));
    }
}

/// The settings' Default dropdown value: the dialog's choice, or the saved default when there is
/// none (the dialog was opened before any provider was set up, so its choice is empty).
pub(crate) fn default_choice(f: &Map<String, Value>, saved: &Saved) -> String {
    f.get("default").and_then(Value::as_str).filter(|d| !d.is_empty()).unwrap_or(saved.default.as_str()).to_owned()
}

fn settings_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = crate::theme::Tokens::get(ui.ctx());
    if app.services.genai.is_none() {
        faint(ui, tl!("Generative AI needs the desktop app."));
        return;
    }
    let saved = saved(app);
    if saved.configured.len() > 1 {
        let opts: Vec<(String, &str)> = saved.configured.iter().map(|id| (id.clone(), provider_name(id))).collect();
        let mut chosen = default_choice(f, &saved);
        ui.horizontal(|ui| {
            ui.label(tl!("Default for Generative Fill"));
            if crate::widgets::dropdown(ui, "genai-default", &mut chosen, &opts, 260.0) {
                f.insert("default".into(), json!(chosen));
            }
        });
        ui.add_space(8.0);
    }
    let mut open = None;
    let mut remove = None;
    egui::Grid::new("genai-providers").num_columns(3).spacing([14.0, 8.0]).show(ui, |ui| {
        for spec in photocraft_genai::providers() {
            let configured = saved.configured.iter().any(|c| c == spec.id);
            ui.vertical(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(spec.name).color(t.text)).extend());
                ui.add(egui::Label::new(egui::RichText::new(spec.host).color(t.text_faint).size(12.0)).extend());
            });
            let status = row_status(app, &saved, spec.id);
            ui.add(egui::Label::new(egui::RichText::new(status).color(t.text_faint).size(12.0)).extend());
            ui.horizontal(|ui| {
                let label = if configured { tl!("Edit…") } else { tl!("Set Up…") };
                if crate::widgets::secondary_button(ui, label, 0.0).clicked() {
                    open = Some(spec.id);
                }
                if configured && crate::widgets::secondary_button(ui, tl!("Remove"), 0.0).clicked() {
                    remove = Some(spec.id);
                }
            });
            ui.end_row();
        }
    });
    if let Some(id) = open {
        open_provider(app, id);
    }
    if let Some(id) = remove
        && let Err(e) = remove_provider(app, id)
    {
        app.ui.status = e;
    }
    ui.add_space(8.0);
    crate::widgets::hairline(ui);
    ui.add_space(6.0);
    faint(ui, tl!("Each Generative Fill makes three variations, so it sends three requests, each billed to your account."));
    let store = app.services.genai.as_ref().map_or("", |s| s.key_store_name);
    faint(ui, &crate::i18n::fmt(tl!("Your credentials are kept in {store}, never in PhotoCraft's settings files."), &[("store", tl!(store))]));
}

fn provider_body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = crate::theme::Tokens::get(ui.ctx());
    let Some(spec) = f.get(PROVIDER_MARK).and_then(Value::as_str).and_then(photocraft_genai::spec) else { return };
    let has_key = f.get("hasKey").and_then(Value::as_bool) == Some(true);
    egui::Grid::new("genai-provider").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        if spec.models.len() > 1 {
            ui.label(tl!("Model"));
            let mut model = f.get("model").and_then(Value::as_str).unwrap_or("").to_owned();
            let models: Vec<(String, &str)> = spec.models.iter().map(|(id, name)| ((*id).to_owned(), *name)).collect();
            if crate::widgets::dropdown(ui, "genai-model", &mut model, &models, 240.0) {
                f.insert("model".into(), json!(model));
            }
            ui.end_row();
        }
        for field in spec.fields {
            ui.label(tl!(field.label));
            let hint = if has_key { tl!("Saved (leave empty to keep)") } else { tl!("Paste here") };
            let buf = app.genai.key_inputs.entry(field.name).or_default();
            let r = ui.add(egui::TextEdit::singleline(buf).password(true).hint_text(hint).desired_width(240.0));
            crate::field_tab::register(ui.ctx(), r.id);
            ui.end_row();
        }
    });
    ui.add_space(4.0);
    if ui.link(crate::i18n::fmt(tl!("Get credentials from {name}"), &[("name", spec.name)])).clicked() {
        crate::links::open(app, &ui.ctx().clone(), spec.key_page);
    }
    ui.add_space(8.0);
    crate::widgets::hairline(ui);
    ui.add_space(6.0);
    ui.label(egui::RichText::new(tl!("What is sent, and to whom")).font(crate::theme::semibold(12.5)).color(t.text));
    for line in privacy_lines(spec) {
        faint(ui, &line);
    }
}

/// The provider dialog's "What is sent, and to whom" text. Mentions the reference image for a
/// provider that can use one, whether or not it is set up yet (the text is read before saving).
pub(crate) fn privacy_lines(spec: &photocraft_genai::ProviderSpec) -> Vec<String> {
    let mut lines = vec![crate::i18n::fmt(
        tl!(
            "Nothing is sent until you run Generative Fill. Each time you do, PhotoCraft sends the selected area and the image around it, a mask of the selection, and your prompt to {name} ({host}) with your credentials. Its terms and privacy policy apply, and its usage is billed to your account."
        ),
        &[("name", spec.name), ("host", spec.host)],
    )];
    if spec.supports_reference {
        lines.push(tl!("If you add a reference image to a fill, it is sent too.").to_owned());
    }
    if !spec.result_hosts.is_empty() {
        let hosts = spec.result_hosts.iter().map(|h| h.trim_start_matches('.')).collect::<Vec<_>>().join(", ");
        lines.push(crate::i18n::fmt(tl!("Results are downloaded from {hosts}."), &[("hosts", &hosts)]));
    }
    if !spec.note.is_empty() {
        lines.push(tl!(spec.note).to_owned());
    }
    lines
}

/// OK in the provider dialog: merge typed fields into the saved credentials, check the provider
/// builds, store, configure and install.
fn confirm_provider(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    let spec = f.get(PROVIDER_MARK).and_then(Value::as_str).and_then(photocraft_genai::spec).ok_or("unknown provider")?;
    let model = f.get("model").and_then(Value::as_str).unwrap_or("").to_owned();
    let typed = std::mem::take(&mut app.genai.key_inputs);
    let services = app.services.genai.as_ref().ok_or_else(|| tl!("Generative AI needs the desktop app.").to_string())?;
    let mut creds = (services.load_key)(spec.id)?.as_deref().and_then(photocraft_genai::Credentials::from_secret).unwrap_or_default();
    for field in spec.fields {
        if let Some(k) = typed.get(field.name).and_then(|t| photocraft_genai::ApiKey::new(t)) {
            creds.insert(field.name, k);
        }
    }
    if !creds.has_all(spec.fields) {
        return Err(crate::i18n::fmt(tl!("Enter every field for {provider}."), &[("provider", spec.name)]));
    }
    (services.provider)(spec.id, &model, creds.clone())?;
    (services.save_key)(spec.id, &creds.to_secret())?;
    let mut saved = saved(app);
    if !saved.configured.iter().any(|c| c == spec.id) {
        saved.configured.push(spec.id.to_owned());
    }
    saved.models.insert(spec.id.to_owned(), model.clone());
    if saved.default.is_empty() {
        saved.default = spec.id.to_owned();
    }
    store_saved(app, &saved);
    restore_after_save(app);
    Ok(json!({"provider": spec.id, "model": model, "ready": app.session.image_provider_by_id(spec.id).is_some()}))
}

/// Whether OK is available: the prompt dialog needs a prompt for providers without a promptless
/// mode. Other dialogs are unaffected.
pub fn can_confirm(app: &PhotocraftApp, fields: &Map<String, Value>) -> bool {
    if !fields.contains_key(PROMPT_MARK) {
        return true;
    }
    let prompt = fields.get("prompt").and_then(Value::as_str).unwrap_or("");
    let chosen = fields.get("provider").and_then(Value::as_str).unwrap_or("");
    let provider = app.session.image_provider_by_id(chosen).or_else(|| app.session.image_provider());
    if chosen_reference(fields).is_some() && provider.is_some_and(|p| !p.supports_reference()) {
        return false;
    }
    !(provider.is_some_and(|p| p.needs_prompt()) && prompt.trim().is_empty())
}

/// OK in the prompt dialog: remember the prompt and the provider, and run the fill.
fn confirm_prompt(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    let prompt = f.get("prompt").and_then(Value::as_str).unwrap_or("").trim().to_owned();
    app.session.prefs.edit(|p| p.dialogs.insert(FILL.into(), json!({"prompt": prompt})));
    let chosen = f.get("provider").and_then(Value::as_str).filter(|id| app.session.image_provider_by_id(id).is_some()).map(str::to_owned);
    let mut params = json!({"prompt": prompt});
    if let Some(id) = &chosen {
        params["provider"] = json!(id);
    }
    if let Some(path) = chosen_reference(f) {
        params["reference"] = json!(path);
        app.genai.last_reference = Some(path.to_owned());
    }
    app.genai.last_provider = chosen;
    app.run(FILL, params)
}

pub fn confirm(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    if f.contains_key(PROMPT_MARK) {
        return confirm_prompt(app, f);
    }
    if f.contains_key(PROVIDER_MARK) {
        return confirm_provider(app, f);
    }
    let mut saved = saved(app);
    if let Some(d) = f.get("default").and_then(Value::as_str)
        && saved.configured.iter().any(|c| c == d)
    {
        saved.default = d.to_owned();
    }
    store_saved(app, &saved);
    restore_after_save(app);
    Ok(json!({"default": saved.default, "configured": saved.configured}))
}

/// Dialog closed without OK: drop whatever was typed in credential fields.
pub fn cancelled(app: &mut PhotocraftApp) {
    app.genai.key_inputs.clear();
}

/// The Generative Fill button (and its settings arrow) for the selection tools' options bar,
/// next to Select and Mask…. The button needs a selection; with no provider set up it opens the
/// settings instead.
pub fn options_bar_button(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if app.services.genai.is_none() {
        return;
    }
    let ready = app.session.image_provider().is_some();
    let can_fill = !ready || app.session.is_enabled(FILL);
    ui.spacing_mut().item_spacing.x = 2.0;
    let tip = if ready { tl!("Fill the selection from a prompt") } else { tl!("Set up Generative Fill with your own API key") };
    let clicked = ui.add_enabled_ui(can_fill, |ui| crate::widgets::secondary_button(ui, tl!("Generative Fill…"), 0.0).on_hover_text(tip)).inner.clicked();
    if clicked {
        let _ = crate::menus::invoke(app, &ui.ctx().clone(), FILL, json!({}));
    }
    if crate::icons::button(ui, "chevron-down", 22.0, false, tl!("Generative AI Settings…")).clicked() {
        open_settings(app);
    }
    ui.spacing_mut().item_spacing.x = 8.0;
}

/// Preferences › Integrations: where Generative AI is set up.
pub fn prefs_rows(app: &PhotocraftApp, ui: &mut egui::Ui) -> bool {
    let t = crate::theme::Tokens::get(ui.ctx());
    if app.services.genai.is_none() {
        return false;
    }
    ui.label(egui::RichText::new(tl!("Generative AI")).font(crate::theme::semibold(12.5)).color(t.text));
    let count = app.session.image_provider_ids().len();
    let status = match app.session.image_provider().map(|p| p.info()) {
        Some(info) if count > 1 => crate::i18n::fmt(
            tl!("Generative Fill uses {name} ({model}) by default; {count} providers are set up."),
            &[("name", info.name), ("model", &info.model), ("count", &count.to_string())],
        ),
        Some(info) => crate::i18n::fmt(tl!("Generative Fill uses {name} ({model})."), &[("name", info.name), ("model", &info.model)]),
        None => tl!("Off. Set up a provider with your own credentials to use Generative Fill.").to_string(),
    };
    faint(ui, &status);
    ui.add_space(4.0);
    crate::widgets::secondary_button(ui, tl!("Generative AI Settings…"), 0.0).clicked()
}

/// The selection-shaped crop of a variation: its pixels inside the group mask's bounds, faded
/// where the mask hides them.
fn variation_image(variation: &Layer, mask: Option<&photocraft_doc::LayerMask>) -> Option<egui::ColorImage> {
    let s = variation.surface()?;
    let area = mask.map_or_else(|| s.content_bounds(), |m| m.surface.content_bounds().intersect(&s.content_bounds()));
    if area.is_empty() {
        return None;
    }
    let k = f64::from(THUMB) / f64::from(area.width().max(area.height()));
    let (w, h) = (((f64::from(area.width()) * k).round() as usize).max(1), ((f64::from(area.height()) * k).round() as usize).max(1));
    let mut px = [0.0f32; 8];
    let mut m = [0.0f32; 1];
    let n = s.channels();
    let mut out = Vec::with_capacity(w * h);
    for ty in 0..h {
        for tx in 0..w {
            let x = area.x0 + ((tx as f64 + 0.5) / k) as i32;
            let y = area.y0 + ((ty as f64 + 0.5) / k) as i32;
            s.read_pixel(x, y, &mut px[..n]);
            let c = photocraft_raster::to_rgba(&s.format(), &px[..n]);
            let a = mask.map_or(1.0, |mask| {
                mask.surface.read_pixel(x, y, &mut m);
                m[0]
            });
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            out.push(egui::Color32::from_rgba_unmultiplied(q(c[0]), q(c[1]), q(c[2]), q(c[3] * a.max(0.25))));
        }
    }
    Some(egui::ColorImage::new([w, h], out))
}

fn variation_thumb(app: &mut PhotocraftApp, ctx: &egui::Context, variation: &Layer, mask: Option<&photocraft_doc::LayerMask>) -> Option<egui::TextureId> {
    let rev = variation.surface().map_or(0, crate::surface_fingerprint) ^ mask.map_or(0, |m| crate::surface_fingerprint(&m.surface));
    if let Some((r, tex)) = app.genai.thumbs.get(&variation.id)
        && *r == rev
    {
        return Some(tex.id());
    }
    let img = variation_image(variation, mask)?;
    let tex = ctx.load_texture(format!("genai-variation-{}", variation.id.0), img, egui::TextureOptions::LINEAR);
    let id = tex.id();
    app.genai.thumbs.insert(variation.id, (rev, tex));
    Some(id)
}

/// Generate More's tooltip: the provider that made `group` (which Generate More sends to), why
/// it can't run, or that the provider has since been removed.
pub(crate) fn more_tooltip(app: &PhotocraftApp, group: LayerId) -> String {
    if let Some(why) = app.session.disabled_reason(MORE) {
        return why;
    }
    let Some(id) = app.session.generative_provider(group) else { return String::new() };
    match app.session.image_provider_by_id(id) {
        Some(p) => crate::i18n::fmt(tl!("Sends the same area and prompt to {name} again for three more."), &[("name", p.info().name)]),
        None => crate::i18n::fmt(tl!("{name}, which made this layer, is no longer set up."), &[("name", provider_name(id))]),
    }
}

/// The Variations header's tooltip: the prompt the layer was made with, and its reference image.
pub(crate) fn variations_tooltip(app: &PhotocraftApp, group: LayerId) -> String {
    let prompt = app.session.generative_prompt(group).unwrap_or("");
    let mut tip = if prompt.is_empty() { tl!("No prompt: continues the image").to_owned() } else { prompt.to_owned() };
    if let Some(n) = app.session.generative_reference_name(group) {
        tip.push('\n');
        tip.push_str(&crate::i18n::fmt(tl!("Reference: {name}"), &[("name", n)]));
    }
    tip
}

/// Properties › Variations, for the active generative layer (or one of its variations): a row of
/// thumbnails, one per variation (click one to show it), and Generate More, as Photoshop's
/// Properties panel offers for a generative layer. Compact, since the dock can be short.
pub fn variations_section(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let Some(group_id) = app.session.generative_layer() else { return };
    let Some(group) = app.session.active().and_then(|st| st.doc.layer(group_id).cloned()) else { return };
    let Some(children) = group.children() else { return };
    let t = crate::theme::Tokens::get(ui.ctx());
    if let Some(st) = app.session.active() {
        app.genai.thumbs.retain(|id, _| st.doc.layer(*id).is_some());
    }
    let head_tip = variations_tooltip(app, group_id);
    let tip = more_tooltip(app, group_id);
    let enabled = app.session.is_enabled(MORE);
    let mut more = false;
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let head = ui.label(egui::RichText::new(tl!("Variations")).font(crate::theme::semibold(12.5)).color(t.text));
        head.on_hover_text(head_tip);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            more = ui.add_enabled_ui(enabled, |ui| crate::widgets::secondary_button(ui, tl!("Generate More"), 0.0).on_hover_text(tip)).inner.clicked();
        });
    });
    ui.add_space(4.0);
    let ctx = ui.ctx().clone();
    let height = 48.0;
    let mut chosen = None;
    egui::ScrollArea::horizontal().id_salt("genai-variations").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (i, v) in children.iter().enumerate() {
                let tex = variation_thumb(app, &ctx, v, group.mask.as_ref());
                let size = app.genai.thumbs.get(&v.id).map_or(egui::vec2(1.0, 1.0), |(_, h)| h.size_vec2());
                let width = (height * size.x / size.y.max(1.0)).clamp(36.0, 96.0);
                let (r, resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
                ui.painter().rect_filled(r, t.radius_sm, t.field);
                if let Some(tex) = tex {
                    let fit = ((width - 4.0) / size.x).min((height - 4.0) / size.y);
                    let img = egui::Rect::from_center_size(r.center(), size * fit);
                    ui.painter().image(tex, img, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                }
                let (w, c) = if v.visible {
                    (2.0, t.accent)
                } else if resp.hovered() {
                    (1.0, t.accent_border)
                } else {
                    (1.0, t.field_border)
                };
                ui.painter().rect_stroke(r, t.radius_sm, egui::Stroke::new(w, c), egui::StrokeKind::Inside);
                resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, v.visible, &v.name));
                if resp.on_hover_text(&v.name).clicked() && !v.visible {
                    chosen = Some(i);
                }
            }
        });
    });
    if let Some(i) = chosen
        && let Err(e) = app.run(SELECT_VARIATION, json!({"layer": group_id.0, "index": i}))
    {
        app.ui.status = e;
    }
    if more && let Err(e) = app.run(MORE, json!({"layer": group_id.0})) {
        app.ui.status = e;
    }
    ui.add_space(6.0);
    crate::widgets::hairline(ui);
    ui.add_space(4.0);
}

#[cfg(test)]
#[path = "genai_ui_tests.rs"]
mod tests;
