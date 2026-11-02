//! Translation for hover popovers, served by the built-in Bing channel or a
//! configured language model.
//!
//! Two entry points:
//! - [`translate_selection`]: keyboard action (`editor::TranslateSelection`).
//!   Translates the current selection, or—when the selection is empty—the
//!   word under the cursor, and shows the result in a small hover popover.
//! - Hover documentation translation: `hover_popover` asks
//!   [`TranslationService`] to translate the non-code parts of LSP hover
//!   contents when [`HoverTranslationSettings::enabled`] is set. The
//!   translation is rendered underlined below the original documentation.

use crate::code_explanations::ConfiguredModel;
use crate::{
    Editor,
    actions::TranslateSelection,
    hover_links::RangeInEditor,
    hover_popover::{InfoPopover, hide_hover},
    translation_cache::{TranslationDiskCache, translation_cache_path},
};
use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::{AsyncReadExt as _, StreamExt as _, channel::oneshot};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, ScrollHandle, SharedString, Task,
    Window,
};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy};
use itertools::Itertools as _;
use language::CharKind;
use language_model::{
    LanguageModelProviderId, LanguageModelRegistry, LanguageModelRequest,
    LanguageModelRequestMessage, MessageContent, Role,
};
use markdown::Markdown;
use multi_buffer::MultiBufferOffset;
use project::{HoverBlock, HoverBlockKind};
use settings::{RegisterSetting, Settings, SettingsContent};
use std::{
    cell::{Cell, RefCell},
    hash::{Hash, Hasher},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

/// Anonymous Microsoft Edge translation endpoint used by the built-in default
/// and explicit `bing` channels. No credentials are sent and redirects are
/// disabled; the endpoint's availability is not guaranteed.
const BING_TRANSLATE_ENDPOINT: &str = "https://edge.microsoft.com/translate/translatetext";
const ANONYMOUS_TRANSLATION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ANONYMOUS_RESPONSE_BYTES: usize = 1024 * 1024;
/// Provider value selecting the built-in anonymous translation channel.
const DEFAULT_TRANSLATION_PROVIDER: &str = "default";
/// Provider value selecting the built-in anonymous Bing channel explicitly.
const BING_TRANSLATION_PROVIDER: &str = "bing";

/// Settings for translation in hover popovers, configured under the
/// `hover_translation` key in settings.json.
#[derive(Clone, Debug, RegisterSetting)]
pub struct HoverTranslationSettings {
    /// Whether hover documentation is automatically translated.
    pub enabled: bool,
    /// The provider (channel) used for translation. `None`, an empty value or
    /// `default` selects the built-in anonymous channel; `bing` selects it
    /// explicitly. Any other value names a configured language model provider.
    pub provider: Option<SharedString>,
    /// The model used for translation.
    pub model: Option<SharedString>,
    /// The target language to translate into. `None` or an empty value follows
    /// the interface language.
    pub target_language: Option<SharedString>,
    /// Maximum number of characters sent for translation.
    pub max_chars: usize,
    /// Whether translations are persisted to an on-disk cache reused across
    /// sessions.
    pub cache_persist: bool,
    /// Maximum size of the on-disk translation cache, in bytes.
    pub cache_max_bytes: usize,
}

impl Settings for HoverTranslationSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.hover_translation.clone().unwrap();
        Self {
            enabled: content.enabled.unwrap(),
            provider: content.provider.map(|provider| provider.as_str().into()),
            model: content.model.map(|model| model.as_str().into()),
            target_language: content.target_language.map(|language| language.into()),
            max_chars: content.max_chars.unwrap() as usize,
            cache_persist: content.cache_persist.unwrap(),
            cache_max_bytes: content.cache_max_bytes.unwrap() as usize,
        }
    }
}

/// Returns whether `text` is worth translating: it contains alphabetic
/// content and does not already contain CJK characters.
pub fn needs_translation(text: &str) -> bool {
    let mut has_cjk = false;
    let mut has_alphabetic = false;
    for ch in text.chars() {
        if matches!(
            ch,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{3040}'..='\u{30FF}'
                | '\u{AC00}'..='\u{D7AF}'
        ) {
            has_cjk = true;
        } else if ch.is_alphabetic() {
            has_alphabetic = true;
        }
        if has_cjk {
            return false;
        }
    }
    has_alphabetic && !has_cjk
}

/// Truncates `text` to at most `max_chars` characters.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

/// Extracts the translatable documentation from hover blocks: plain text and
/// markdown blocks, excluding code blocks.
pub fn documentation_text(blocks: &[HoverBlock]) -> Option<String> {
    let text = blocks
        .iter()
        .filter_map(|block| match &block.kind {
            HoverBlockKind::PlainText | HoverBlockKind::Markdown => Some(block.text.trim()),
            HoverBlockKind::Code { .. } => None,
        })
        .join("\n\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The backend that serves a translation request.
enum TranslationRoute {
    /// The built-in anonymous Bing endpoint, used both by the default channel
    /// and by an explicit `bing` channel.
    Bing,
    /// A language model provider configured under `language_models`.
    LanguageModel(ConfiguredModel),
}

impl TranslationRoute {
    /// The provider part of the cache key. Built-in requests are pinned to the
    /// `bing` channel so they cannot collide with a configured provider named
    /// `default`.
    fn cache_key_prefix(&self) -> String {
        match self {
            Self::Bing => BING_TRANSLATION_PROVIDER.to_string(),
            Self::LanguageModel(model) => {
                format!("{}:{}", model.provider.id().0, model.model.id().0)
            }
        }
    }

    /// The target-language part of the cache key. Built-in requests use the
    /// normalized BCP-47 tag so `中文` and `zh-Hans` share results.
    fn cache_key_target<'a>(&self, target: &'a TranslationTarget) -> &'a str {
        match self {
            Self::Bing => &target.tag,
            Self::LanguageModel(_) => &target.name,
        }
    }
}

/// The resolved translation target: a human-readable name for the model prompt
/// and a BCP-47 tag for the anonymous endpoint.
struct TranslationTarget {
    name: String,
    tag: String,
}

/// Maps a configured language name or tag to the tag accepted by the Bing
/// endpoint. Known names are mapped to their language; anything else is passed
/// through unchanged, matching the reference translator.
fn bing_language_tag(language: &str) -> String {
    let language = language.trim();
    match language.to_lowercase().as_str() {
        "中文" | "简体中文" | "中文（简体）" | "chinese" | "simplified chinese" | "zh"
        | "zh-cn" | "zh-sg" | "zh-hans" => "zh-Hans".to_string(),
        "繁體中文"
        | "繁体中文"
        | "中文（繁體）"
        | "traditional chinese"
        | "zh-tw"
        | "zh-hk"
        | "zh-mo"
        | "zh-hant" => "zh-Hant".to_string(),
        "英文" | "英语" | "english" | "en" => "en".to_string(),
        "日文" | "日语" | "japanese" | "ja" => "ja".to_string(),
        "韩文" | "韩语" | "korean" | "ko" => "ko".to_string(),
        "法文" | "法语" | "french" | "fr" => "fr".to_string(),
        "德文" | "德语" | "german" | "de" => "de".to_string(),
        "西班牙文" | "西班牙语" | "spanish" | "es" => "es".to_string(),
        "俄文" | "俄语" | "russian" | "ru" => "ru".to_string(),
        "葡萄牙文" | "葡萄牙语" | "portuguese" | "pt" => "pt".to_string(),
        "意大利文" | "意大利语" | "italian" | "it" => "it".to_string(),
        _ => language.to_string(),
    }
}

/// The language name used in the model prompt when the target follows the
/// interface language.
fn interface_language_name(locale: i18n::Locale) -> &'static str {
    match locale.id() {
        "zh-Hans" => "Simplified Chinese",
        "en" => "English",
        other => other,
    }
}

fn resolve_target_language(configured: Option<&str>) -> TranslationTarget {
    translation_target(configured, i18n::current_locale())
}

fn translation_target(
    configured: Option<&str>,
    interface_locale: i18n::Locale,
) -> TranslationTarget {
    match configured
        .map(str::trim)
        .filter(|language| !language.is_empty() && !language.eq_ignore_ascii_case("auto"))
    {
        Some(language) => TranslationTarget {
            name: language.to_string(),
            tag: bing_language_tag(language),
        },
        None => TranslationTarget {
            name: interface_language_name(interface_locale).to_string(),
            tag: bing_language_tag(interface_locale.id()),
        },
    }
}

/// The translation channel chosen by the provider/model settings.
enum ProviderSelection {
    Builtin,
    LanguageModel { provider: String, model: String },
    Incomplete,
}

fn select_provider(provider: Option<&str>, model: Option<&str>) -> ProviderSelection {
    let provider = provider
        .map(str::trim)
        .filter(|provider| !provider.is_empty());
    let model = model.map(str::trim).filter(|model| !model.is_empty());

    if matches!(
        provider,
        Some(DEFAULT_TRANSLATION_PROVIDER) | Some(BING_TRANSLATION_PROVIDER)
    ) {
        return ProviderSelection::Builtin;
    }

    // An unconfigured provider uses the built-in anonymous channel, which needs
    // no language model at all. A model without a provider stays an error so a
    // partially configured LLM channel never silently switches services.
    match (provider, model) {
        (None, None) => ProviderSelection::Builtin,
        (Some(provider), Some(model)) => ProviderSelection::LanguageModel {
            provider: provider.to_string(),
            model: model.to_string(),
        },
        _ => ProviderSelection::Incomplete,
    }
}

fn bing_translation_url(target_tag: &str) -> Result<String> {
    let mut url = http_client::Url::parse(BING_TRANSLATE_ENDPOINT)?;
    url.query_pairs_mut()
        .append_pair("to", target_tag)
        .append_pair("isEnterpriseClient", "false");
    Ok(url.into())
}

fn parse_bing_translation(body: &[u8]) -> Result<SharedString> {
    let payload: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| anyhow::anyhow!(i18n::t!("d410a8f58de1ef69")))?;
    let translation = payload
        .as_array()
        .and_then(|segments| segments.first())
        .and_then(|segment| segment.get("translations"))
        .and_then(|translations| translations.as_array())
        .and_then(|translations| translations.first())
        .and_then(|translation| translation.get("text"))
        .and_then(|text| text.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow::anyhow!(i18n::t!("d410a8f58de1ef69")))?;
    Ok(SharedString::from(translation.to_string()))
}

/// A translation request that is currently in flight, along with the waiters
/// that receive the result once it completes.
struct InFlightTranslation {
    waiters: Vec<oneshot::Sender<Result<SharedString, String>>>,
}

/// Process-wide translation service with an in-memory cache and an optional
/// on-disk cache, so hovering the same content repeatedly only calls the model
/// once.
///
/// In-flight requests are detached, so they keep running—and cache their
/// result—even when the caller drops the returned `Task`, e.g. when the hover
/// popover is dismissed while the translation is still in flight. Dropping the
/// returned task then only cancels the *waiting*, not the request.
pub struct TranslationService {
    /// Completed translations for the current session, keyed by the cache key
    /// (route:target_language:text-hash).
    cache: HashMap<String, SharedString>,
    /// Requests that are still running, keyed by the cache key.
    in_flight: HashMap<String, InFlightTranslation>,
    /// Persistent cache reused across sessions, when `cache_persist` is set.
    pub(crate) disk_cache: TranslationDiskCache,
}

/// Returns the cache key for a translation: the route and target language,
/// plus a hash of the text. Changing any of them naturally misses the cache, so
/// a stale translation can never be served for a new configuration.
fn cache_key(route: &TranslationRoute, target: &TranslationTarget, text: &str) -> String {
    format!(
        "{}:{}:{:x}",
        route.cache_key_prefix(),
        route.cache_key_target(target),
        hash_text(text)
    )
}

struct GlobalTranslationService(Entity<TranslationService>);

impl Global for GlobalTranslationService {}

impl TranslationService {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(service) = cx.try_global::<GlobalTranslationService>() {
            return service.0.clone();
        }
        Self::create(translation_cache_path(), cx)
    }

    fn create(path: std::path::PathBuf, cx: &mut App) -> Entity<Self> {
        let settings = HoverTranslationSettings::get_global(cx);
        let persist = settings.cache_persist;
        let max_bytes = settings.cache_max_bytes;
        let service = cx.new(|cx| {
            cx.observe_global::<settings::SettingsStore>(|service: &mut Self, cx| {
                let settings = HoverTranslationSettings::get_global(cx);
                service
                    .disk_cache
                    .configure(settings.cache_persist, settings.cache_max_bytes);
                service.disk_cache.schedule_save(cx);
            })
            .detach();
            Self {
                cache: HashMap::default(),
                in_flight: HashMap::default(),
                disk_cache: TranslationDiskCache::new(path.clone(), max_bytes, persist),
            }
        });
        if persist {
            // Kick off the one-time disk load in the background.
            let entity = service.downgrade();
            let load_task = cx.spawn(async move |cx| {
                let bytes = cx
                    .background_spawn(async move { std::fs::read(path) })
                    .await;
                entity
                    .update(cx, |service, _| match bytes {
                        Ok(bytes) => service.disk_cache.load_from_bytes(&bytes),
                        Err(_) => service.disk_cache.mark_loaded(),
                    })
                    .ok();
            });
            service.update(cx, |service, _| service.disk_cache.set_load_task(load_task));
        }
        cx.set_global(GlobalTranslationService(service.clone()));
        service
    }

    /// Translates `text` into the configured target language. Results are
    /// cached in memory for the current session and—when `cache_persist` is
    /// enabled—on disk for future sessions.
    pub fn translate(text: String, cx: &mut App) -> Task<Result<SharedString>> {
        let settings = HoverTranslationSettings::get_global(cx);
        let target = resolve_target_language(settings.target_language.as_deref());
        let persist = settings.cache_persist;
        let max_bytes = settings.cache_max_bytes;
        let text = truncate_text(&text, settings.max_chars);

        let route = match resolve_route(&settings, cx) {
            Ok(route) => route,
            Err(error) => return Task::ready(Err(error)),
        };
        let service = Self::global(cx);
        let key = cache_key(&route, &target, &text);
        let http_client = cx.http_client();

        // Keep the persistent cache in sync with the current settings.
        service.update(cx, |service, _| {
            service.disk_cache.configure(persist, max_bytes)
        });

        // Fast in-memory hit.
        if let Some(cached) = service.read(cx).cache.get(&key).cloned() {
            if persist {
                service.update(cx, |service, cx| {
                    service.disk_cache.record_view(&key);
                    service.disk_cache.schedule_save(cx);
                });
            }
            return Task::ready(Ok(cached));
        }

        // Persistent cache hit, if the disk cache has already been loaded.
        if persist {
            let disk_hit = service.update(cx, |service, cx| {
                let translation = service.disk_cache.get(&key)?.translation.clone();
                service.cache.insert(key.clone(), translation.clone());
                service.disk_cache.record_view(&key);
                service.disk_cache.schedule_save(cx);
                Some(translation)
            });
            if let Some(translation) = disk_hit {
                return Task::ready(Ok(translation));
            }
        }

        // The one-time disk load must complete before the request consults or
        // writes the persistent cache; take it here and await it inside the
        // detached request task.
        let load_task = service.update(cx, |service, _| service.disk_cache.take_load_task());

        // Register the in-flight entry (or join an existing one) synchronously,
        // so the request is started even if the returned task is dropped
        // before it is ever polled.
        let (tx, rx) = oneshot::channel();
        let started = service.update(cx, |service, _| {
            if let Some(in_flight) = service.in_flight.get_mut(&key) {
                in_flight.waiters.push(tx);
                return false;
            }
            service
                .in_flight
                .insert(key.clone(), InFlightTranslation { waiters: vec![tx] });
            true
        });

        if started {
            // The actual request is detached so it keeps running—and caches
            // its result—even if every caller drops its `Task`. Callers only
            // await a one-shot channel that the request completes.
            let this = service.downgrade();
            cx.spawn(async move |cx| {
                if let Some(load_task) = load_task {
                    let _ = load_task.await;
                }

                // Persistent cache hit (the disk load completed since the
                // synchronous check above): resolve the waiters without
                // calling the model.
                let disk_hit = this
                    .update(cx, |service, cx| {
                        let translation = service.disk_cache.get(&key)?.translation.clone();
                        service.cache.insert(key.clone(), translation.clone());
                        service.disk_cache.record_view(&key);
                        service.disk_cache.schedule_save(cx);
                        Some(translation)
                    })
                    .ok()
                    .flatten();
                if let Some(translation) = disk_hit {
                    this.update(cx, |service, _| {
                        if let Some(in_flight) = service.in_flight.remove(&key) {
                            for waiter in in_flight.waiters {
                                let _ = waiter.send(Ok(translation.clone()));
                            }
                        }
                    })
                    .ok();
                    return;
                }

                let result = match route {
                    TranslationRoute::Bing => {
                        request_bing_translation(&http_client, text, &target).await
                    }
                    TranslationRoute::LanguageModel(model) => {
                        request_translation(&model, text, &target.name, cx).await
                    }
                }
                .map_err(|err| err.to_string());
                this.update(cx, |service, cx| {
                    if let Some(in_flight) = service.in_flight.remove(&key) {
                        if let Ok(translation) = &result {
                            service.cache.insert(key.clone(), translation.clone());
                            service.disk_cache.store(&key, translation.clone());
                            service.disk_cache.schedule_save(cx);
                        }
                        for waiter in in_flight.waiters {
                            let _ = waiter.send(result.clone());
                        }
                    }
                })
                .ok();
            })
            .detach();
        }

        cx.spawn(async move |_| {
            let result = rx
                .await
                .map_err(|_| anyhow::anyhow!(i18n::t!("091ddc24fa8961a6")))?;
            result.map_err(anyhow::Error::msg)
        })
    }
}

fn resolve_route(settings: &HoverTranslationSettings, cx: &App) -> Result<TranslationRoute> {
    match select_provider(settings.provider.as_deref(), settings.model.as_deref()) {
        ProviderSelection::Builtin => Ok(TranslationRoute::Bing),
        ProviderSelection::Incomplete => Err(anyhow::anyhow!(i18n::t!("c70371b6a0586b63"))),
        ProviderSelection::LanguageModel {
            provider: provider_id,
            model: model_name,
        } => {
            let registry = LanguageModelRegistry::read_global(cx);
            let provider = registry
                .provider(&LanguageModelProviderId(provider_id.as_str().into()))
                .with_context(|| i18n::t!("f5aca7fe71bf5d1f", provider_id = provider_id))?;
            let model = provider
                .provided_models(cx)
                .into_iter()
                .find(|candidate| candidate.id().0.as_ref() == model_name.as_str())
                .with_context(|| {
                    i18n::t!(
                        "6ae3539f811d5bad",
                        provider_id = provider_id,
                        model_name = model_name
                    )
                })?;
            Ok(TranslationRoute::LanguageModel(ConfiguredModel {
                provider,
                model,
            }))
        }
    }
}

async fn request_bing_translation(
    client: &Arc<dyn HttpClient>,
    text: String,
    target: &TranslationTarget,
) -> Result<SharedString> {
    let url = bing_translation_url(&target.tag)?;
    let body = serde_json::to_string(&[text.as_str()])?;
    let request = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(url)
        .header("Content-Type", "application/json")
        .follow_redirects(RedirectPolicy::NoFollow)
        .timeout(ANONYMOUS_TRANSLATION_TIMEOUT)
        .body(AsyncBody::from(body))?;
    let response = client.send(request).await?;
    let status = response.status();
    anyhow::ensure!(
        status.is_success(),
        i18n::t!("05d7d00737291230", status = status.as_u16())
    );

    let mut body = Vec::new();
    response
        .into_body()
        .take(MAX_ANONYMOUS_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .await?;
    anyhow::ensure!(
        body.len() <= MAX_ANONYMOUS_RESPONSE_BYTES,
        i18n::t!("808c91a847b2948f", max_bytes = MAX_ANONYMOUS_RESPONSE_BYTES)
    );
    parse_bing_translation(&body)
}

async fn request_translation(
    model: &ConfiguredModel,
    text: String,
    target_language: &str,
    cx: &mut AsyncApp,
) -> Result<SharedString> {
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(format!(
                    "You are a translation engine. Translate the user's text into \
                     {target_language}. Preserve all code, identifiers, file paths and \
                     markdown structure. Output only the translation, without any \
                     explanations or surrounding quotes."
                ))],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(text)],
                cache: false,
                reasoning_details: None,
            },
        ],
        temperature: Some(0.2),
        thinking_allowed: false,
        ..Default::default()
    };

    let mut stream = model
        .provider
        .stream_completion_text(&model.model, request, cx)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let mut translation = String::new();
    while let Some(chunk) = stream.stream.next().await {
        let chunk = chunk.map_err(|err| anyhow::anyhow!(err.to_string()))?;
        translation.push_str(&chunk);
    }
    let translation = translation.trim();
    anyhow::ensure!(!translation.is_empty(), i18n::t!("0d88845ca53c345e"));
    Ok(SharedString::from(translation.to_string()))
}

/// The `editor::TranslateSelection` action: translates the current selection
/// or, when empty, the word under the cursor, and shows the translation in a
/// small hover popover. Works regardless of the `hover_translation.enabled`
/// setting; an unconfigured channel uses the built-in Bing translation.
pub fn translate_selection(
    editor: &mut Editor,
    _: &TranslateSelection,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let snapshot = editor.snapshot(window, cx);
    let buffer_snapshot = snapshot.buffer_snapshot();
    let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);

    let (text, range) = if selection.is_empty() {
        let (word_range, word_kind) = buffer_snapshot.surrounding_word(selection.head(), None);
        if word_kind != Some(CharKind::Word) || word_range.is_empty() {
            return;
        }
        (
            buffer_snapshot
                .text_for_range(word_range.clone())
                .collect::<String>(),
            word_range,
        )
    } else {
        let range = selection.start..selection.end;
        (
            buffer_snapshot
                .text_for_range(range.clone())
                .collect::<String>(),
            range,
        )
    };

    if !needs_translation(&text) {
        return;
    }

    let markdown = cx.new(|cx| Markdown::new(i18n::t!("07f81963cdeb4dca").into(), None, None, cx));
    let subscription = cx.observe(&markdown, |_, _, cx| cx.notify());

    let translation = TranslationService::translate(text, cx);
    let task = cx.spawn_in(window, {
        let markdown = markdown.clone();
        async move |_, cx| {
            let content = match translation.await {
                Ok(translation) => translation,
                Err(err) => SharedString::from(i18n::t!("6312a196d155bd2d", err = err)),
            };
            markdown.update(cx, |markdown, cx| markdown.reset(content, cx));
        }
    });

    hide_hover(editor, cx);
    editor.hover_state.info_popovers = vec![InfoPopover {
        symbol_range: RangeInEditor::Text(
            buffer_snapshot.anchor_before(range.start)..buffer_snapshot.anchor_after(range.end),
        ),
        parsed_content: Some(markdown),
        translated_content: None,
        debugger_hover: None,
        scroll_handle: ScrollHandle::new(),
        keyboard_grace: Rc::new(RefCell::new(false)),
        anchor: Some(buffer_snapshot.anchor_before(range.start)),
        last_bounds: Rc::new(Cell::new(None)),
        fallback_state: None,
        _subscriptions: vec![subscription],
        _translated_subscription: None,
        _translation_task: Some(task),
    }];
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        actions::TranslateSelection, editor_tests::init_test,
        test::editor_test_context::EditorTestContext,
    };
    use gpui::BorrowAppContext;
    use language_model::fake_provider::FakeLanguageModelProvider;
    use settings::SettingsStore;
    use std::sync::{Arc, Mutex};

    /// Initializes the editor with the persistent translation cache disabled,
    /// so tests never read or write the real cache file under the user's data
    /// directory.
    fn init_test_without_persistent_cache(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});
        cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings: &mut SettingsContent| {
                    settings.hover_translation = Some(
                        serde_json::from_value(serde_json::json!({ "cache_persist": false }))
                            .unwrap(),
                    );
                });
            });
        });
    }

    #[test]
    fn test_needs_translation() {
        assert!(needs_translation("parse blocks"));
        assert!(needs_translation("hover_popover"));
        assert!(needs_translation("Returns the current selection."));
        assert!(!needs_translation("解析代码块"));
        assert!(!needs_translation("返回当前选区 selection"));
        assert!(!needs_translation("12345"));
        assert!(!needs_translation("..."));
        assert!(!needs_translation(""));
    }

    #[test]
    fn test_truncate_text() {
        assert_eq!(truncate_text("hello", 10), "hello");
        assert_eq!(truncate_text("hello world", 5), "hello…");
        assert_eq!(truncate_text("你好世界", 2), "你好…");
    }

    fn setup_fake_model(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Arc<FakeLanguageModelProvider>,
        language_model::LanguageModel,
    ) {
        cx.update(|cx| {
            LanguageModelRegistry::test(cx);
            let provider = Arc::new(FakeLanguageModelProvider::new(
                LanguageModelProviderId::from("fake".to_string()),
                language_model::LanguageModelProviderName::from("Fake".to_string()),
            ));
            let model = provider.model("fake");
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(provider.clone(), cx);
                registry.set_default_model(Some(model.clone()), cx);
            });
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings: &mut SettingsContent| {
                    let hover = settings.hover_translation.get_or_insert_default();
                    hover.provider = Some(settings::TranslationProviderSetting("fake".to_string()));
                    hover.model = Some(settings::TranslationModelSetting("fake".to_string()));
                });
            });
            (provider, model)
        })
    }

    fn popover_text(editor: &Editor, cx: &App) -> String {
        editor.hover_state.info_popovers[0]
            .parsed_content
            .as_ref()
            .unwrap()
            .read(cx)
            .source()
            .to_string()
    }

    #[gpui::test]
    async fn test_translate_word_under_cursor(cx: &mut gpui::TestAppContext) {
        init_test_without_persistent_cache(cx);
        let (provider, model) = setup_fake_model(cx);
        let mut cx = EditorTestContext::new(cx).await;

        // The word under the cursor (bounded by punctuation) is translated.
        cx.set_state("let value = foo.ˇbar();");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();

        cx.editor(|editor, _, cx| {
            assert_eq!(popover_text(editor, cx), "翻译中…");
        });

        let requests = provider.pending_completions();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages[1].string_contents(), "bar");
        assert!(
            requests[0].messages[0]
                .string_contents()
                .contains("Simplified Chinese")
        );

        provider.send_last_text(&model, "柱");
        provider.end_last(&model);
        cx.run_until_parked();

        cx.editor(|editor, _, cx| {
            assert_eq!(popover_text(editor, cx), "柱");
        });

        // Translating the same word again is served from the cache and does
        // not issue another request.
        cx.update_editor(|editor, _, cx| {
            hide_hover(editor, cx);
        });
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(popover_text(editor, cx), "柱");
        });
        assert!(provider.pending_completions().is_empty());
    }

    #[gpui::test]
    async fn test_in_flight_translation_survives_dropped_caller(cx: &mut gpui::TestAppContext) {
        init_test_without_persistent_cache(cx);
        let (provider, model) = setup_fake_model(cx);

        // Start a translation and drop the returned task before the model
        // responds — this is what happens when the hover popover is dismissed
        // while the translation is still in flight. The request must keep
        // running and cache its result.
        let task = cx.update(|cx| TranslationService::translate("hello world".into(), cx));
        drop(task);
        cx.run_until_parked();

        // The request is still running, not canceled.
        assert_eq!(provider.pending_completions().len(), 1);

        // The detached request completes and caches the result.
        provider.send_last_text(&model, "你好世界");
        provider.end_last(&model);
        cx.run_until_parked();

        // Translating the same text again is served from the cache and does
        // not issue another request.
        let task = cx.update(|cx| TranslationService::translate("hello world".into(), cx));
        assert_eq!(task.await.unwrap(), "你好世界");
        assert!(provider.pending_completions().is_empty());
    }

    #[gpui::test]
    async fn test_in_flight_translation_is_deduplicated(cx: &mut gpui::TestAppContext) {
        init_test_without_persistent_cache(cx);
        let (provider, model) = setup_fake_model(cx);
        // Two callers translate the same text while the request is in flight;
        // only one model request should be issued.
        let task1 = cx.update(|cx| TranslationService::translate("hello world".into(), cx));
        let task2 = cx.update(|cx| TranslationService::translate("hello world".into(), cx));
        cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);

        // Both callers receive the result.
        provider.send_last_text(&model, "你好世界");
        provider.end_last(&model);
        cx.run_until_parked();
        assert_eq!(task1.await.unwrap(), "你好世界");
        assert_eq!(task2.await.unwrap(), "你好世界");
        assert!(provider.pending_completions().is_empty());
    }

    #[gpui::test]
    async fn test_translation_stored_in_disk_cache(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});
        let directory = util::test::TempTree::new(serde_json::json!({}));
        cx.update(|cx| {
            TranslationService::create(directory.path().join("translation_cache.json"), cx);
        });
        let (provider, model) = setup_fake_model(cx);

        let task = cx.update(|cx| TranslationService::translate("hello world".into(), cx));
        cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);

        provider.send_last_text(&model, "你好世界");
        provider.end_last(&model);
        cx.run_until_parked();
        assert_eq!(task.await.unwrap(), "你好世界");

        // The translation must have been stored in the persistent cache.
        cx.update(|cx| {
            let service = TranslationService::global(cx);
            let entries = service.read(cx).disk_cache.entries();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries.values().next().unwrap().translation, "你好世界");
        });
    }

    #[gpui::test]
    async fn test_persistent_cache_load_uses_injected_directory(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});
        let directory = util::test::TempTree::new(serde_json::json!({
            "translation_cache.json": serde_json::json!({
                "version": 1,
                "entries": {"isolated-fixture": {"translation": "隔离译文"}}
            }).to_string()
        }));
        let path = directory.path().join("translation_cache.json");
        let service = cx.update(|cx| TranslationService::create(path.clone(), cx));
        cx.run_until_parked();
        service.read_with(cx, |service, _| {
            assert_eq!(service.disk_cache.entries().len(), 1);
            assert_eq!(
                service
                    .disk_cache
                    .get("isolated-fixture")
                    .unwrap()
                    .translation,
                "隔离译文"
            );
        });
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[gpui::test]
    async fn test_disabling_persistence_cancels_scheduled_save(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});
        let directory = util::test::TempTree::new(serde_json::json!({}));
        let path = directory.path().join("translation_cache.json");
        let service = cx.update(|cx| TranslationService::create(path.clone(), cx));
        cx.run_until_parked();
        service.update(cx, |service, cx| {
            service.disk_cache.store("key", "译文".into());
            service.disk_cache.schedule_save(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings: &mut SettingsContent| {
                    settings
                        .hover_translation
                        .get_or_insert_with(Default::default)
                        .cache_persist = Some(false);
                });
            })
        });
        cx.run_until_parked();
        cx.background_executor
            .timer(std::time::Duration::from_secs(10))
            .await;
        cx.run_until_parked();
        assert!(!path.exists());
        assert!(service.read_with(cx, |service, _| service.disk_cache.is_dirty()));

        cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings: &mut SettingsContent| {
                    settings
                        .hover_translation
                        .get_or_insert_with(Default::default)
                        .cache_persist = Some(true);
                });
            })
        });
        cx.run_until_parked();
        cx.background_executor
            .timer(std::time::Duration::from_secs(5))
            .await;
        cx.run_until_parked();
        assert!(path.is_file());
        assert!(!service.read_with(cx, |service, _| service.disk_cache.is_dirty()));
    }

    #[gpui::test]
    async fn test_persistence_write_failure_remains_dirty_and_can_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        init_test(cx, |_| {});
        let directory = util::test::TempTree::new(serde_json::json!({}));
        let path = directory.path().join("missing/translation_cache.json");
        let service = cx.update(|cx| TranslationService::create(path.clone(), cx));
        cx.run_until_parked();
        service.update(cx, |service, cx| {
            service.disk_cache.store("key", "译文".into());
            service.disk_cache.schedule_save(cx);
        });
        cx.run_until_parked();
        cx.background_executor
            .timer(std::time::Duration::from_secs(5))
            .await;
        cx.run_until_parked();
        assert!(!path.exists());
        assert!(service.read_with(cx, |service, _| service.disk_cache.is_dirty()));
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        service.update(cx, |service, cx| service.disk_cache.schedule_save(cx));
        cx.run_until_parked();
        cx.background_executor
            .timer(std::time::Duration::from_secs(5))
            .await;
        cx.run_until_parked();
        let bytes = std::fs::read(&path).unwrap();
        let mut reloaded = TranslationDiskCache::new(path, 5242880, true);
        reloaded.load_from_bytes(&bytes);
        assert_eq!(reloaded.get("key").unwrap().translation, "译文");
        assert!(!service.read_with(cx, |service, _| service.disk_cache.is_dirty()));
    }

    #[test]
    fn test_bing_language_tags() {
        assert_eq!(bing_language_tag("中文"), "zh-Hans");
        assert_eq!(bing_language_tag("简体中文"), "zh-Hans");
        assert_eq!(bing_language_tag("zh-CN"), "zh-Hans");
        assert_eq!(bing_language_tag("zh-Hans"), "zh-Hans");
        assert_eq!(bing_language_tag("繁體中文"), "zh-Hant");
        assert_eq!(bing_language_tag("English"), "en");
        assert_eq!(bing_language_tag("英语"), "en");
        assert_eq!(bing_language_tag("日语"), "ja");
        assert_eq!(bing_language_tag("fr-FR"), "fr-FR");
        assert_eq!(bing_language_tag("  "), "");
    }

    #[test]
    fn test_translation_target_follows_interface_language() {
        let followed = translation_target(None, i18n::Locale::En);
        assert_eq!(followed.name, "English");
        assert_eq!(followed.tag, "en");

        let followed = translation_target(Some("  "), i18n::Locale::ZhHans);
        assert_eq!(followed.name, "Simplified Chinese");
        assert_eq!(followed.tag, "zh-Hans");

        let automatic = translation_target(Some("auto"), i18n::Locale::ZhHans);
        assert_eq!(automatic.tag, "zh-Hans");

        let configured = translation_target(Some("日语"), i18n::Locale::En);
        assert_eq!(configured.name, "日语");
        assert_eq!(configured.tag, "ja");
    }

    #[test]
    fn test_provider_selection_uses_builtin_by_default() {
        assert!(matches!(
            select_provider(None, None),
            ProviderSelection::Builtin
        ));
        assert!(matches!(
            select_provider(Some("default"), None),
            ProviderSelection::Builtin
        ));
        assert!(matches!(
            select_provider(Some("bing"), Some("ignored")),
            ProviderSelection::Builtin
        ));
        assert!(matches!(
            select_provider(Some(""), Some("")),
            ProviderSelection::Builtin
        ));
        assert!(matches!(
            select_provider(Some("openai"), Some("gpt")),
            ProviderSelection::LanguageModel { .. }
        ));
        assert!(matches!(
            select_provider(Some("openai"), None),
            ProviderSelection::Incomplete
        ));
        assert!(matches!(
            select_provider(None, Some("gpt")),
            ProviderSelection::Incomplete
        ));
    }

    #[test]
    fn test_bing_translation_url_encodes_target() {
        let url = bing_translation_url("zh-Hans").expect("built-in endpoint url");
        assert!(url.starts_with(BING_TRANSLATE_ENDPOINT));
        assert!(url.contains("to=zh-Hans"), "url: {url}");
        assert!(url.contains("isEnterpriseClient=false"), "url: {url}");
    }

    #[test]
    fn test_parse_bing_translation_response() {
        assert_eq!(
            parse_bing_translation(r#"[{"translations":[{"text":"你好"}]}]"#.as_bytes())
                .expect("valid response")
                .as_ref(),
            "你好"
        );
        assert!(parse_bing_translation(br#"[]"#).is_err());
        assert!(parse_bing_translation(br#"[{"translations":[]}]"#).is_err());
        assert!(parse_bing_translation(br#"[{"translations":[{"text":"  "}]}]"#).is_err());
        assert!(parse_bing_translation(br#"[{"translations":[{"text":null}]}]"#).is_err());
        assert!(parse_bing_translation(b"not json").is_err());
    }

    #[test]
    fn test_builtin_cache_key_uses_normalized_tag() {
        let target = translation_target(Some("中文"), i18n::Locale::ZhHans);
        let key = cache_key(&TranslationRoute::Bing, &target, "hello");
        assert!(key.starts_with("bing:zh-Hans:"), "key: {key}");
    }

    #[gpui::test]
    async fn test_default_translation_uses_builtin_bing(cx: &mut gpui::TestAppContext) {
        init_test_without_persistent_cache(cx);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_handler = requests.clone();
        let http_client = http_client::FakeHttpClient::create(move |request| {
            let requests = requests_for_handler.clone();
            async move {
                let uri = request.uri().to_string();
                let mut body = request.into_body();
                let mut text = String::new();
                body.read_to_string(&mut text).await?;
                requests.lock().unwrap().push((uri, text));
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(AsyncBody::from(
                        r#"[{"translations":[{"text":"你好，世界","to":"zh-Hans"}]}]"#,
                    ))?)
            }
        });
        cx.update(|cx| cx.set_http_client(http_client));

        let mut cx = EditorTestContext::new(cx).await;
        cx.set_state("let value = foo.ˇbar();");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();

        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert!(
                captured[0].0.contains("to=zh-Hans"),
                "uri: {}",
                captured[0].0
            );
            assert!(
                captured[0].0.contains("isEnterpriseClient=false"),
                "uri: {}",
                captured[0].0
            );
            assert_eq!(captured[0].1, r#"["bar"]"#);
        }

        cx.editor(|editor, _, cx| {
            assert_eq!(popover_text(editor, cx), "你好，世界");
        });

        // The built-in translation is cached like a model translation.
        cx.update_editor(|editor, _, cx| {
            hide_hover(editor, cx);
        });
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();
        assert_eq!(requests.lock().unwrap().len(), 1);
        cx.editor(|editor, _, cx| {
            assert_eq!(popover_text(editor, cx), "你好，世界");
        });
    }

    #[gpui::test]
    async fn test_configured_translation_model_does_not_fall_back(cx: &mut gpui::TestAppContext) {
        init_test_without_persistent_cache(cx);
        let (default_provider, _default_model) = setup_fake_model(cx);
        cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings: &mut SettingsContent| {
                    settings.hover_translation = Some(
                        serde_json::from_value(serde_json::json!({
                            "cache_persist": false,
                            "provider": "missing-provider",
                            "model": "missing-model"
                        }))
                        .unwrap(),
                    );
                });
            });
        });
        let mut cx = EditorTestContext::new(cx).await;

        cx.set_state("let «sensitive_textˇ» = 1;");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();

        assert!(default_provider.pending_completions().is_empty());
        cx.editor(|editor, _, cx| {
            let text = popover_text(editor, cx);
            assert!(text.contains("配置的翻译模型提供商不可用"), "got: {text}");
        });
    }

    #[gpui::test]
    async fn test_translate_selection_range(cx: &mut gpui::TestAppContext) {
        init_test_without_persistent_cache(cx);
        let (provider, model) = setup_fake_model(cx);
        let mut cx = EditorTestContext::new(cx).await;

        // An explicit selection is translated as-is, even across word
        // boundaries.
        cx.set_state("let «foo.barˇ» = 1;");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();

        let requests = provider.pending_completions();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages[1].string_contents(), "foo.bar");
        provider.end_last(&model);
        cx.run_until_parked();

        // A Chinese selection is not translated.
        cx.set_state("let «变量ˇ» = 1;");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();
        assert!(provider.pending_completions().is_empty());
        cx.editor(|editor, _, _| {
            assert!(editor.hover_state.info_popovers.is_empty());
        });
    }
}
