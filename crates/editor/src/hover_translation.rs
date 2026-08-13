//! AI-powered translation for hover popovers.
//!
//! Two entry points:
//! - [`translate_selection`]: keyboard action (`editor::TranslateSelection`).
//!   Translates the current selection, or—when the selection is empty—the
//!   word under the cursor, and shows the result in a small hover popover.
//! - Hover documentation translation: `hover_popover` asks
//!   [`TranslationService`] to translate the non-code parts of LSP hover
//!   contents when [`HoverTranslationSettings::enabled`] is set. The
//!   translation is rendered underlined below the original documentation.

use crate::{
    Editor,
    actions::TranslateSelection,
    hover_links::RangeInEditor,
    hover_popover::{InfoPopover, hide_hover},
};
use anyhow::Result;
use collections::HashMap;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, ScrollHandle, SharedString, Task,
    Window,
};
use itertools::Itertools as _;
use language::CharKind;
use language_model::{
    ConfiguredModel, LanguageModelProviderId, LanguageModelRegistry, LanguageModelRequest,
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
};

/// Settings for AI-powered translation in hover popovers, configured under
/// the `hover_translation` key in settings.json.
#[derive(Clone, Debug, RegisterSetting)]
pub struct HoverTranslationSettings {
    /// Whether hover documentation is automatically translated.
    pub enabled: bool,
    /// The provider (channel) used for translation. When `None`, the default
    /// fast model is used.
    pub provider: Option<SharedString>,
    /// The model used for translation.
    pub model: Option<SharedString>,
    /// The target language to translate into.
    pub target_language: SharedString,
    /// Maximum number of characters sent for translation.
    pub max_chars: usize,
}

impl Settings for HoverTranslationSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.hover_translation.clone().unwrap();
        Self {
            enabled: content.enabled.unwrap(),
            provider: content.provider.map(|provider| provider.as_str().into()),
            model: content.model.map(|model| model.as_str().into()),
            target_language: content.target_language.unwrap().into(),
            max_chars: content.max_chars.unwrap() as usize,
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

/// Process-wide translation service with an in-memory cache, so hovering the
/// same content repeatedly only calls the model once.
pub struct TranslationService {
    cache: HashMap<u64, SharedString>,
}

struct GlobalTranslationService(Entity<TranslationService>);

impl Global for GlobalTranslationService {}

impl TranslationService {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(service) = cx.try_global::<GlobalTranslationService>() {
            return service.0.clone();
        }
        let service = cx.new(|_| Self {
            cache: HashMap::default(),
        });
        cx.set_global(GlobalTranslationService(service.clone()));
        service
    }

    /// Translates `text` into the configured target language. Results are
    /// cached in memory for the duration of the session.
    pub fn translate(text: String, cx: &mut App) -> Task<Result<SharedString>> {
        let settings = HoverTranslationSettings::get_global(cx);
        let target_language = settings.target_language.clone();
        let text = truncate_text(&text, settings.max_chars);

        let service = Self::global(cx);
        let hash = hash_text(&text);
        if let Some(cached) = service.read(cx).cache.get(&hash) {
            return Task::ready(Ok(cached.clone()));
        }

        let Some(model) = resolve_model(cx) else {
            return Task::ready(Err(anyhow::anyhow!(
                "未配置翻译模型：请在 settings.json 的 \"hover_translation\" 中填写 \
                 \"provider\" 和 \"model\"，或先配置默认语言模型"
            )));
        };

        service.update(cx, |_, cx| {
            cx.spawn(async move |this, cx| {
                let translation = request_translation(&model, text, &target_language, cx).await?;
                this.update(cx, |service, _| {
                    service.cache.insert(hash, translation.clone());
                })?;
                Ok(translation)
            })
        })
    }
}

fn resolve_model(cx: &App) -> Option<ConfiguredModel> {
    let settings = HoverTranslationSettings::get_global(cx);
    let registry = LanguageModelRegistry::read_global(cx);
    if let (Some(provider_id), Some(model_name)) = (&settings.provider, &settings.model) {
        if let Some(provider) = registry.provider(&LanguageModelProviderId(provider_id.clone())) {
            if let Some(model) = provider
                .provided_models(cx)
                .into_iter()
                .find(|model| model.id().0.as_ref() == model_name.as_str())
            {
                return Some(ConfiguredModel { provider, model });
            }
        }
    }
    // Fall back to the default (fast) model when no provider/model is
    // configured, or when the configured one is no longer available.
    registry
        .default_fast_model(cx)
        .or_else(|| registry.default_model())
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
        .model
        .stream_completion_text(request, cx)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let mut translation = String::new();
    while let Some(chunk) = stream.stream.next().await {
        let chunk = chunk.map_err(|err| anyhow::anyhow!(err.to_string()))?;
        translation.push_str(&chunk);
    }
    let translation = translation.trim();
    anyhow::ensure!(!translation.is_empty(), "翻译结果为空");
    Ok(SharedString::from(translation.to_string()))
}

/// The `editor::TranslateSelection` action: translates the current selection
/// or, when empty, the word under the cursor, and shows the translation in a
/// small hover popover. Works regardless of the `hover_translation.enabled`
/// setting, as long as a translation model is configured.
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

    let markdown = cx.new(|cx| Markdown::new("翻译中…".into(), None, None, cx));
    let subscription = cx.observe(&markdown, |_, _, cx| cx.notify());

    let translation = TranslationService::translate(text, cx);
    let task = cx.spawn_in(window, {
        let markdown = markdown.clone();
        async move |_, cx| {
            let content = match translation.await {
                Ok(translation) => translation,
                Err(err) => SharedString::from(format!("翻译失败：{err}")),
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
        scroll_handle: ScrollHandle::new(),
        keyboard_grace: Rc::new(RefCell::new(false)),
        anchor: Some(buffer_snapshot.anchor_before(range.start)),
        last_bounds: Rc::new(Cell::new(None)),
        _subscription: Some(subscription),
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
    use language_model::fake_provider::{FakeLanguageModel, FakeLanguageModelProvider};
    use std::sync::Arc;

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

    fn setup_fake_model(cx: &mut gpui::TestAppContext) -> Arc<FakeLanguageModel> {
        cx.update(|cx| {
            LanguageModelRegistry::test(cx);
            let model = Arc::new(FakeLanguageModel::default());
            let provider =
                Arc::new(FakeLanguageModelProvider::default().with_models(vec![model.clone()]));
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(provider.clone(), cx);
                registry.set_default_model(
                    Some(ConfiguredModel {
                        provider,
                        model: model.clone(),
                    }),
                    cx,
                );
            });
            model
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
        init_test(cx, |_| {});
        let model = setup_fake_model(cx);
        let mut cx = EditorTestContext::new(cx).await;

        // The word under the cursor (bounded by punctuation) is translated.
        cx.set_state("let value = foo.ˇbar();");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();

        cx.editor(|editor, _, cx| {
            assert_eq!(popover_text(editor, cx), "翻译中…");
        });

        let requests = model.pending_completions();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages[1].string_contents(), "bar");
        assert!(requests[0].messages[0].string_contents().contains("中文"));

        model.send_last_completion_stream_text_chunk("柱");
        model.end_last_completion_stream();
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
        assert!(model.pending_completions().is_empty());
    }

    #[gpui::test]
    async fn test_translate_selection_without_configured_model(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});
        cx.update(|cx| {
            LanguageModelRegistry::test(cx);
            // Simulate an unconfigured translation setup: no default model
            // and no explicit provider/model in hover_translation settings.
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.set_default_model(None, cx);
            });
        });
        let mut cx = EditorTestContext::new(cx).await;

        cx.set_state("let \u{02C7}value = 1;");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();

        // The popover should tell the user the model is not configured,
        // instead of doing nothing.
        cx.editor(|editor, _, cx| {
            let text = popover_text(editor, cx);
            assert!(text.contains("未配置翻译模型"), "got: {text}");
        });
    }

    #[gpui::test]
    async fn test_translate_selection_range(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});
        let model = setup_fake_model(cx);
        let mut cx = EditorTestContext::new(cx).await;

        // An explicit selection is translated as-is, even across word
        // boundaries.
        cx.set_state("let «foo.barˇ» = 1;");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();

        let requests = model.pending_completions();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages[1].string_contents(), "foo.bar");
        model.end_last_completion_stream();
        cx.run_until_parked();

        // A Chinese selection is not translated.
        cx.set_state("let «变量ˇ» = 1;");
        cx.dispatch_action(TranslateSelection);
        cx.run_until_parked();
        assert!(model.pending_completions().is_empty());
        cx.editor(|editor, _, _| {
            assert!(editor.hover_state.info_popovers.is_empty());
        });
    }
}
