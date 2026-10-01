use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context as _;
use editor::Editor;
use gpui::{AnyView, Entity, Focusable as _, ScrollHandle, prelude::*};
use language_model::{
    ApiKeyConfiguration, CreateProviderSettingsView, IconOrSvg, InlineDescription,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelRegistry, ProviderSettingsView,
};
use language_models::AllLanguageModelSettings;
use settings::Settings as _;

use settings::{
    AnthropicCompatibleAvailableModel, AnthropicCompatibleModelCapabilities,
    AnthropicCompatibleSettingsContent, OpenAiCompatibleAvailableModel,
    OpenAiCompatibleModelCapabilities, OpenAiCompatibleSettingsContent, OpenAiReasoningEffort,
};
use ui::{
    ButtonLink, Checkbox, ConfiguredApiCard, ContextMenu, Divider, DividerColor, DropdownMenu,
    DropdownStyle, IconPosition, PopoverMenu, ToggleState, prelude::*,
};
use util::ResultExt as _;

use crate::SettingsWindow;
use crate::components::SettingsInputField;

pub(crate) fn render_llm_providers_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let providers = LanguageModelRegistry::read_global(cx).visible_providers();

    v_flex()
        .id("llm-providers-page")
        .size_full()
        .px_8()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(
            Label::new(i18n::t!("6b011be6687aeb70"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .children(
            providers
                .iter()
                .enumerate()
                .map(|(index, provider)| {
                    render_provider_section(settings_window, provider, index == 0, window, cx)
                })
                .collect::<Vec<_>>(),
        )
        .into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompatibleProviderKind {
    OpenAi,
    Anthropic,
}

impl CompatibleProviderKind {
    fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
        }
    }

    fn default_api_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }
}

pub(crate) fn render_add_llm_provider_popover(
    settings_window: &SettingsWindow,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let focus_handle = settings_window
        .llm_provider_add_focus_handle
        .clone()
        .tab_index(0)
        .tab_stop(true);

    let settings_window = cx.entity().downgrade();

    PopoverMenu::new("add-llm-provider-popover")
        .trigger(
            Button::new("add-llm-provider", i18n::t!("a96064159efa96f8"))
                .style(ButtonStyle::Outlined)
                .track_focus(&focus_handle)
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(IconName::Plus)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
        )
        .anchor(gpui::Anchor::TopRight)
        .offset(gpui::Point {
            x: px(0.0),
            y: px(2.0),
        })
        .menu(move |window, cx| {
            let settings_window = settings_window.clone();
            Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                menu.header(i18n::t!("f19fde529ae13c47"))
                    .entry("OpenAI", None, {
                        let settings_window = settings_window.clone();
                        move |window, cx| {
                            settings_window
                                .update(cx, |this, cx| {
                                    open_llm_provider_form(
                                        this,
                                        CompatibleProviderKind::OpenAi,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        }
                    })
                    .entry("Anthropic", None, {
                        let settings_window = settings_window;
                        move |window, cx| {
                            settings_window
                                .update(cx, |this, cx| {
                                    open_llm_provider_form(
                                        this,
                                        CompatibleProviderKind::Anthropic,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        }
                    })
            }))
        })
}

fn render_provider_section(
    settings_window: &SettingsWindow,
    provider: &Arc<dyn LanguageModelProvider>,
    is_first: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let provider_id = provider.id();
    let provider_name = provider.name().0;

    let body = match provider.settings_view(cx) {
        Some(ProviderSettingsView::ApiKey(config)) => {
            render_api_key_providers_item(provider, provider_name.clone(), config, cx)
        }
        Some(ProviderSettingsView::Inline(settings)) => {
            let view = get_or_create_configuration_view(
                settings_window,
                &provider_id,
                settings.create_view,
                window,
                cx,
            );
            render_inline_body(
                provider_name.clone(),
                settings.title,
                settings.description,
                view,
            )
        }
        Some(ProviderSettingsView::SubPage(settings)) => {
            render_subpage_item(provider, settings.description, cx)
        }
        None => div().into_any_element(),
    };

    v_flex()
        .min_w_0()
        .map(|s| if is_first { s.pt_4() } else { s.pt_8() })
        .gap_1p5()
        .child(render_provider_header(provider_name, provider.icon(), cx))
        .child(body)
        .into_any_element()
}

/// An icon + name header with a faded divider, mirroring `SettingsSectionHeader`
/// but able to render providers' external SVG icons.
fn render_provider_header(
    provider_name: SharedString,
    icon: IconOrSvg,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let icon = match icon {
        IconOrSvg::Svg(path) => Icon::from_external_svg(path),
        IconOrSvg::Icon(name) => Icon::new(name),
    }
    .color(Color::Muted);

    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            h_flex().gap_1p5().child(icon).child(
                Label::new(provider_name)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .buffer_font(cx),
            ),
        )
        .child(Divider::horizontal().color(DividerColor::BorderFaded))
}

fn render_api_key_providers_item(
    provider: &Arc<dyn LanguageModelProvider>,
    provider_name: SharedString,
    config: ApiKeyConfiguration,
    _cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let provider_id = provider.id();
    let has_key = config.has_key;
    let is_from_env_var = config.is_from_env_var;
    let env_var_name = config.env_var_name;
    let api_key_url = config.api_key_url;

    if has_key {
        let configured_label = if is_from_env_var {
            i18n::t!("7628f7e8d8a3a919")
        } else {
            i18n::t!("d95b24a24825e6c7")
        };
        let button_id = format!("reset-api-key-{}", provider_id.0);

        let card = ConfiguredApiCard::new(button_id, configured_label)
            .button_label(i18n::t!("053a696c633ca1a6"))
            .button_tab_index(0)
            .disabled(is_from_env_var)
            .when(is_from_env_var, |this| {
                this.tooltip_label(i18n::t!("b2a6bb525f03fd35", env_var_name = env_var_name))
            })
            .on_click({
                let provider = provider.clone();
                move |_, _, cx| {
                    provider.set_api_key(None, cx).detach_and_log_err(cx);
                }
            })
            .into_any_element();

        return v_flex().gap_2().child(card).into_any_element();
    }

    let input_id = format!("{}-api-key-input", provider_id.0);
    let aria_label = format!("{provider_name} API Key");

    v_flex()
        .gap_2()
        .child(
            h_flex()
                .pt_2p5()
                .w_full()
                .min_w_0()
                .gap_4()
                .justify_between()
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .max_w_1_2()
                        .gap_0p5()
                        .child(Label::new(i18n::t!("5f600b307b4eb0fb")))
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .flex_wrap()
                                .gap_0p5()
                                .child(
                                    Label::new(i18n::t!("9cff688763e22a22"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    ButtonLink::new(
                                        format!("{provider_name} dashboard"),
                                        api_key_url,
                                    )
                                    .no_icon(true)
                                    .label_size(LabelSize::Small)
                                    .label_color(Color::Muted),
                                )
                                .child(
                                    Label::new(i18n::t!("4776c33f2a626335"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(
                            Label::new(i18n::t!("7c36154c6b113d75", env_var_name = env_var_name))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    SettingsInputField::new(input_id)
                        .tab_index(0)
                        .with_placeholder("xxxxxxxxxxxxxxxxxxxx")
                        .aria_label(aria_label)
                        .on_confirm({
                            let provider = provider.clone();
                            move |api_key, _window, cx| {
                                if let Some(key) = api_key.filter(|key| !key.is_empty()) {
                                    provider.set_api_key(Some(key), cx).detach_and_log_err(cx);
                                }
                            }
                        }),
                ),
        )
        .into_any_element()
}

fn render_inline_body(
    provider_name: SharedString,
    title: Option<SharedString>,
    description: Option<InlineDescription>,
    view: impl IntoElement,
) -> AnyElement {
    let view = view.into_any_element();

    if title.is_none() && description.is_none() {
        return v_flex()
            .pt_1()
            .w_full()
            .min_w_0()
            .child(view)
            .into_any_element();
    }

    h_flex()
        .pt_2p5()
        .w_full()
        .min_w_0()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .max_w_1_2()
                .debug_selector(|| "inline-provider-description".into())
                .when_some(title, |this, title| this.child(Label::new(title)))
                .when_some(description, |this, description| {
                    this.child(render_inline_description(provider_name, description))
                }),
        )
        .child(
            h_flex()
                .min_w_0()
                .max_w_1_2()
                .flex_1()
                .justify_end()
                .child(view),
        )
        .into_any_element()
}

fn render_subpage_item(
    provider: &Arc<dyn LanguageModelProvider>,
    description: Option<InlineDescription>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let provider_id = provider.id();
    let provider_name = provider.name().0;

    h_flex()
        .pt_2p5()
        .w_full()
        .min_w_0()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .max_w_1_2()
                .gap_0p5()
                .child(Label::new(i18n::t!("ba97c7905b135af6")))
                .when_some(description, |this, description| {
                    this.child(render_inline_description(provider_name, description))
                }),
        )
        .child(
            Button::new(
                format!("configure-{}", provider_id.0),
                i18n::t!("148d195e21b05db5"),
            )
            .style(ButtonStyle::OutlinedGhost)
            .size(ButtonSize::Medium)
            .end_icon(
                Icon::new(IconName::ChevronRight)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .tab_index(0isize)
            .on_click(cx.listener(move |this, _, window, cx| {
                open_provider_configuration(this, provider_id.clone(), window, cx);
            })),
        )
        .into_any_element()
}

fn render_inline_description(
    provider_name: SharedString,
    description: InlineDescription,
) -> AnyElement {
    match description {
        InlineDescription::ApiKeyUrl(url) => h_flex()
            .gap_0p5()
            .child(
                Label::new(i18n::t!("8feee8c1382b6238"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                ButtonLink::new(format!("{provider_name} dashboard."), url)
                    .label_size(LabelSize::Small),
            )
            .into_any_element(),
        InlineDescription::Text(text) => Label::new(text)
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    }
}

fn open_provider_configuration(
    settings_window: &mut SettingsWindow,
    provider_id: LanguageModelProviderId,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let title = LanguageModelRegistry::read_global(cx)
        .provider(&provider_id)
        .map(|provider| provider.name().0)
        .unwrap_or_else(|| provider_id.0.clone());

    settings_window.configuring_provider = Some(provider_id);

    settings_window.push_dynamic_sub_page(
        title,
        i18n::t!("e3ce49ba838f9dbd"),
        Some("llm_providers"),
        true,
        render_provider_config_sub_page,
        window,
        cx,
    );
}

fn render_provider_config_sub_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(provider_id) = settings_window.configuring_provider.clone() else {
        return div().into_any_element();
    };
    let Some(provider) = LanguageModelRegistry::read_global(cx).provider(&provider_id) else {
        return div().into_any_element();
    };

    let Some(create_view) =
        provider
            .settings_view(cx)
            .and_then(|settings_view| match settings_view {
                ProviderSettingsView::Inline(settings) => Some(settings.create_view),
                ProviderSettingsView::SubPage(settings) => Some(settings.create_view),
                ProviderSettingsView::ApiKey(_) => None,
            })
    else {
        return div().into_any_element();
    };
    let view =
        get_or_create_configuration_view(settings_window, &provider_id, create_view, window, cx);
    let compatible = compatible_provider(&provider_id.0, cx);

    v_flex()
        .id("provider-config-sub-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .when_some(compatible, |this, (kind, api_url, count)| {
            this.child(
                v_flex()
                    .gap_2()
                    .pb_4()
                    .child(
                        Label::new(i18n::t_args!(
                            "19d62472a1e08556",
                            kind.label(),
                            count,
                            api_url
                        ))
                        .color(Color::Muted),
                    )
                    .child(
                        Button::new("edit-compatible-provider", i18n::t!("a26d9020c0ed5f94"))
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                open_existing_llm_provider_form(
                                    this,
                                    provider_id.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
        })
        .child(view)
        .into_any_element()
}

fn compatible_provider(id: &str, cx: &App) -> Option<(CompatibleProviderKind, String, usize)> {
    let settings = AllLanguageModelSettings::get_global(cx);
    if let Some(provider) = settings.openai_compatible.get(id) {
        return Some((
            CompatibleProviderKind::OpenAi,
            provider.api_url.clone(),
            provider.available_models.len(),
        ));
    }
    settings.anthropic_compatible.get(id).map(|provider| {
        (
            CompatibleProviderKind::Anthropic,
            provider.api_url.clone(),
            provider.available_models.len(),
        )
    })
}

fn open_existing_llm_provider_form(
    settings_window: &mut SettingsWindow,
    provider_id: LanguageModelProviderId,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let id = provider_id.0.as_ref();
    let existing = {
        let settings = AllLanguageModelSettings::get_global(cx);
        if let Some(provider) = settings.openai_compatible.get(id) {
            Some((
                CompatibleProviderKind::OpenAi,
                provider.api_url.clone(),
                ParsedModels::OpenAi(provider.available_models.clone()),
            ))
        } else {
            settings.anthropic_compatible.get(id).map(|provider| {
                (
                    CompatibleProviderKind::Anthropic,
                    provider.api_url.clone(),
                    ParsedModels::Anthropic(provider.available_models.clone()),
                )
            })
        }
    };
    let Some((kind, api_url, existing_models)) = existing else {
        return;
    };
    let models = match existing_models {
        ParsedModels::OpenAi(models) => models
            .iter()
            .map(|model| ModelInput::from_open_ai(model, window, cx))
            .collect(),
        ParsedModels::Anthropic(models) => models
            .iter()
            .map(|model| ModelInput::from_anthropic(model, window, cx))
            .collect(),
    };
    settings_window.llm_provider_form = Some(LlmProviderForm::for_existing(
        kind, id, &api_url, models, window, cx,
    ));
    settings_window.push_dynamic_sub_page(
        i18n::t!("0449faac00f2038c", id = id),
        i18n::t!("e3ce49ba838f9dbd"),
        Some("llm_providers"),
        true,
        render_llm_provider_form_page,
        window,
        cx,
    );
}

fn get_or_create_configuration_view(
    settings_window: &SettingsWindow,
    provider_id: &LanguageModelProviderId,
    create_view: CreateProviderSettingsView,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyView {
    if let Some(view) = settings_window
        .provider_configuration_views
        .get(provider_id)
    {
        return view.clone();
    }

    let view = create_view(window, cx);

    // Store the view for future renders by deferring a mutation
    let provider_id = provider_id.clone();
    let view_clone = view.clone();
    cx.defer_in(window, move |this, _window, _cx| {
        this.provider_configuration_views
            .insert(provider_id, view_clone);
    });

    view
}

static NEXT_LLM_PROVIDER_FORM_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct LlmProviderForm {
    id: u64,
    kind: CompatibleProviderKind,
    editing_name: Option<String>,
    original_api_url: Option<String>,
    provider_name: Entity<Editor>,
    api_url: Entity<Editor>,
    api_key: Entity<Editor>,
    models: Vec<ModelInput>,
    is_fetching_models: bool,
    is_saving: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
}

impl LlmProviderForm {
    fn new(
        kind: CompatibleProviderKind,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        Self {
            id: NEXT_LLM_PROVIDER_FORM_ID.fetch_add(1, Ordering::Relaxed),
            kind,
            editing_name: None,
            original_api_url: None,
            provider_name: new_input(kind.label(), None, false, window, cx),
            api_url: new_input(kind.default_api_url(), None, false, window, cx),
            api_key: new_input(
                "000000000000000000000000000000000000000000000000",
                None,
                true,
                window,
                cx,
            ),
            models: Vec::new(),
            is_fetching_models: false,
            is_saving: false,
            error: None,
            notice: None,
        }
    }

    fn for_existing(
        kind: CompatibleProviderKind,
        name: &str,
        api_url: &str,
        models: Vec<ModelInput>,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        let mut form = Self::new(kind, window, cx);
        form.editing_name = Some(name.to_string());
        form.original_api_url = Some(api_url.to_string());
        form.provider_name
            .update(cx, |editor, cx| editor.set_text(name, window, cx));
        form.api_url
            .update(cx, |editor, cx| editor.set_text(api_url, window, cx));
        form.models = models;
        form
    }
}

struct ModelInput {
    name: Entity<Editor>,
    max_completion_tokens: Entity<Editor>,
    max_output_tokens: Entity<Editor>,
    max_tokens: Entity<Editor>,
    reasoning_effort: OpenAiReasoningEffort,
    supports_tools: ToggleState,
    supports_images: ToggleState,
    supports_parallel_tool_calls: ToggleState,
    supports_prompt_cache_key: ToggleState,
    supports_chat_completions: ToggleState,
    supports_thinking: ToggleState,
    interleaved_reasoning: ToggleState,
    max_tokens_parameter: ToggleState,
    expanded: bool,
    original: Option<OriginalModel>,
}

#[derive(Clone)]
enum OriginalModel {
    OpenAi(OpenAiCompatibleAvailableModel),
    Anthropic(AnthropicCompatibleAvailableModel),
}

impl ModelInput {
    fn new(_index: usize, window: &mut Window, cx: &mut Context<SettingsWindow>) -> Self {
        let OpenAiCompatibleModelCapabilities {
            tools,
            images,
            parallel_tool_calls,
            prompt_cache_key,
            chat_completions,
            interleaved_reasoning,
            max_tokens_parameter,
        } = OpenAiCompatibleModelCapabilities::default();

        Self {
            name: new_input(
                "e.g. gpt-5, claude-opus-4, gemini-2.5-pro",
                None,
                false,
                window,
                cx,
            ),
            max_completion_tokens: new_input("200000", Some("200000"), false, window, cx),
            max_output_tokens: new_input(
                i18n::t!("94409bc79114731f"),
                Some("32000"),
                false,
                window,
                cx,
            ),
            max_tokens: new_input(
                i18n::t!("7c1d0708fac48697"),
                Some("200000"),
                false,
                window,
                cx,
            ),
            reasoning_effort: OpenAiReasoningEffort::Medium,
            supports_tools: tools.into(),
            supports_images: images.into(),
            supports_parallel_tool_calls: parallel_tool_calls.into(),
            supports_prompt_cache_key: prompt_cache_key.into(),
            supports_chat_completions: chat_completions.into(),
            supports_thinking: ToggleState::Unselected,
            interleaved_reasoning: interleaved_reasoning.into(),
            max_tokens_parameter: max_tokens_parameter.into(),
            expanded: true,
            original: None,
        }
    }

    fn from_discovered(
        index: usize,
        model: DiscoveredModel,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        let mut input = Self::new(index, window, cx);
        input.name.update(cx, |editor, cx| {
            editor.set_text(model.name, window, cx);
        });
        input.max_completion_tokens.update(cx, |editor, cx| {
            editor.set_text(model.max_tokens.to_string(), window, cx);
        });
        input.max_output_tokens.update(cx, |editor, cx| {
            editor.set_text(model.max_output_tokens.to_string(), window, cx);
        });
        input.max_tokens.update(cx, |editor, cx| {
            editor.set_text(model.max_tokens.to_string(), window, cx);
        });
        input.supports_tools = model.supports_tools.into();
        input.supports_images = model.supports_images.into();
        input.supports_thinking = model.supports_thinking.into();
        input.expanded = false;
        input
    }

    fn from_open_ai(
        model: &OpenAiCompatibleAvailableModel,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        let mut input = Self::new(0, window, cx);
        input.name.update(cx, |editor, cx| {
            editor.set_text(model.name.as_str(), window, cx)
        });
        input.max_tokens.update(cx, |editor, cx| {
            editor.set_text(model.max_tokens.to_string(), window, cx)
        });
        input.max_output_tokens.update(cx, |editor, cx| {
            editor.set_text(
                model.max_output_tokens.unwrap_or(32_000).to_string(),
                window,
                cx,
            )
        });
        input.max_completion_tokens.update(cx, |editor, cx| {
            editor.set_text(
                model.max_completion_tokens.unwrap_or(200_000).to_string(),
                window,
                cx,
            )
        });
        input.reasoning_effort = model
            .reasoning_effort
            .unwrap_or(OpenAiReasoningEffort::Medium);
        input.supports_thinking = model.reasoning_effort.is_some().into();
        input.supports_tools = model.capabilities.tools.into();
        input.supports_images = model.capabilities.images.into();
        input.supports_parallel_tool_calls = model.capabilities.parallel_tool_calls.into();
        input.supports_prompt_cache_key = model.capabilities.prompt_cache_key.into();
        input.supports_chat_completions = model.capabilities.chat_completions.into();
        input.interleaved_reasoning = model.capabilities.interleaved_reasoning.into();
        input.max_tokens_parameter = model.capabilities.max_tokens_parameter.into();
        input.expanded = false;
        input.original = Some(OriginalModel::OpenAi(model.clone()));
        input
    }

    fn from_anthropic(
        model: &AnthropicCompatibleAvailableModel,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        let mut input = Self::new(0, window, cx);
        input.name.update(cx, |editor, cx| {
            editor.set_text(model.name.as_str(), window, cx)
        });
        input.max_tokens.update(cx, |editor, cx| {
            editor.set_text(model.max_tokens.to_string(), window, cx)
        });
        input.max_output_tokens.update(cx, |editor, cx| {
            editor.set_text(
                model.max_output_tokens.unwrap_or(32_000).to_string(),
                window,
                cx,
            )
        });
        input.supports_tools = model.capabilities.tools.into();
        input.supports_images = model.capabilities.images.into();
        input.supports_thinking = model
            .mode
            .as_ref()
            .is_some_and(|mode| !matches!(mode, settings::ModelMode::Default))
            .into();
        input.expanded = false;
        input.original = Some(OriginalModel::Anthropic(model.clone()));
        input
    }
}

fn new_input(
    placeholder: &str,
    initial: Option<&str>,
    masked: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Entity<Editor> {
    let placeholder = placeholder.to_string();
    let initial = initial.map(str::to_string);
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder.as_str(), window, cx);
        editor.set_masked(masked, cx);
        if let Some(text) = initial {
            editor.set_text(text, window, cx);
        }
        editor
    })
}

fn open_llm_provider_form(
    settings_window: &mut SettingsWindow,
    kind: CompatibleProviderKind,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.llm_provider_form = Some(LlmProviderForm::new(kind, window, cx));
    settings_window.push_dynamic_sub_page(
        i18n::t_args!("0619cad6caf6fda1", kind.label()),
        i18n::t!("e3ce49ba838f9dbd"),
        Some("llm_providers"),
        true,
        render_llm_provider_form_page,
        window,
        cx,
    );
}

fn render_llm_provider_form_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(form) = settings_window.llm_provider_form.as_ref() else {
        return div().into_any_element();
    };

    v_flex()
        .size_full()
        .child(
            v_flex()
                .id("llm-provider-form-page")
                .track_scroll(scroll_handle)
                .pt_2p5()
                .px_8()
                .pb_16()
                .gap_4()
                .overflow_y_scroll()
                .child(Label::new(match form.kind {
                    CompatibleProviderKind::OpenAi => i18n::t!("5ae8295bad0a7dde"),
                    CompatibleProviderKind::Anthropic => {
                        i18n::t!("589ab69acb7ee8f7")
                    }
                }))
                .child(Divider::horizontal().flex_shrink_0())
                .when(form.editing_name.is_some(), |this| {
                    this.child(Label::new(i18n::t!("720bc01dc885d311")).color(Color::Muted))
                })
                .when(form.editing_name.is_none(), |this| {
                    this.child(render_form_field(
                        i18n::t!("f2c043b16943ef53"),
                        i18n::t!("7a7a82f011553f03"),
                        &form.provider_name,
                        cx,
                    ))
                })
                .child(render_form_field(
                    "API URL",
                    i18n::t!("1e82a64d79f4f0bf"),
                    &form.api_url,
                    cx,
                ))
                .child(render_form_field(
                    "API Key",
                    if form.editing_name.is_some() {
                        i18n::t!("c3d2c3e6b2c9ff11")
                    } else {
                        i18n::t!("f23c4b081a17b3d0")
                    },
                    &form.api_key,
                    cx,
                ))
                .child(render_models_section(form, window, cx)),
        )
        .child(
            v_flex()
                .px_8()
                .py_2p5()
                .gap_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .when_some(form.error.clone(), |this, error| {
                    this.child(render_form_error(error))
                })
                .when_some(form.notice.clone(), |this, notice| {
                    this.child(
                        Label::new(notice)
                            .size(LabelSize::Small)
                            .color(Color::Success),
                    )
                })
                .child(render_form_actions(form.is_saving, cx)),
        )
        .into_any_element()
}

fn render_form_field(
    title: &'static str,
    description: &'static str,
    editor: &Entity<Editor>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let colors = cx.theme().colors();
    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    h_flex().gap_0p5().child(Label::new(title)).child(
                        Label::new("*")
                            .size(LabelSize::Small)
                            .color(Color::Error)
                            .mb_2(),
                    ),
                )
                .child(
                    Label::new(description)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .child(
            h_flex()
                .w_full()
                .min_w_64()
                .h_8()
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border)
                .bg(colors.editor_background)
                .track_focus(&focus_handle)
                .focus(|style| style.border_color(colors.border_focused))
                .child(editor.clone()),
        )
        .into_any_element()
}

fn render_models_section(
    form: &LlmProviderForm,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let fetch_label = if form.is_fetching_models {
        i18n::t!("85ab30269d8fc924")
    } else {
        i18n::t!("556f985a69d620d8")
    };

    v_flex()
        .mt_1()
        .gap_2()
        .child(
            Label::new(i18n::t!("389f7369b58b6072"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            h_flex()
                .justify_between()
                .child(Label::new(i18n::t!("c98e118e0a43f078")))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("fetch-models", fetch_label)
                                .start_icon(
                                    Icon::new(IconName::ArrowCircle)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .label_size(LabelSize::Small)
                                .disabled(form.is_fetching_models || form.is_saving)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    fetch_llm_provider_models(this, window, cx);
                                })),
                        )
                        .child(
                            Button::new("add-model", i18n::t!("168e845a86bc3703"))
                                .start_icon(
                                    Icon::new(IconName::Plus)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .label_size(LabelSize::Small)
                                .disabled(form.is_saving)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(form) = this.llm_provider_form.as_mut() {
                                        let index = form.models.len();
                                        form.models.push(ModelInput::new(index, window, cx));
                                    }
                                    cx.notify();
                                })),
                        ),
                ),
        )
        .children(
            form.models
                .iter()
                .enumerate()
                .map(|(index, model)| render_model(form.kind, model, index, window, cx)),
        )
}

fn fetch_llm_provider_models(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(form) = settings_window.llm_provider_form.as_mut() else {
        return;
    };
    if form.is_fetching_models || form.is_saving {
        return;
    }

    let form_id = form.id;
    let kind = form.kind;
    let api_url = form.api_url.read(cx).text(cx);
    let api_key = form.api_key.read(cx).text(cx);
    if api_url.trim().is_empty() || api_key.trim().is_empty() {
        form.error = Some(i18n::t!("676790accedbfb32").into());
        cx.notify();
        return;
    }

    form.is_fetching_models = true;
    form.error = None;
    form.notice = None;
    cx.notify();

    let api_url = api_url.trim().trim_end_matches('/').to_string();
    let api_key = api_key.trim().to_string();
    let http_client = cx.http_client();
    cx.spawn_in(window, async move |this, cx| {
        let result = match kind {
            CompatibleProviderKind::OpenAi => lmstudio::get_models(
                http_client.as_ref(),
                &api_url,
                Some(&api_key),
                None,
                &Default::default(),
            )
            .await
            .context(i18n::t!("ecefdf3d55ecebcf"))
            .map(|models| {
                models
                    .into_iter()
                    .filter(|model| model.r#type != lmstudio::ModelType::Embeddings)
                    .map(|model| DiscoveredModel {
                        name: model.id,
                        max_tokens: model
                            .loaded_context_length
                            .or(model.max_context_length)
                            .unwrap_or(200_000),
                        max_output_tokens: 32_000,
                        supports_tools: model.capabilities.is_empty()
                            || model.capabilities.supports_tool_calls(),
                        supports_images: model.capabilities.supports_images()
                            || model.r#type == lmstudio::ModelType::Vlm,
                        supports_thinking: false,
                    })
                    .collect::<Vec<_>>()
            }),
            CompatibleProviderKind::Anthropic => anthropic::list_models(
                http_client.as_ref(),
                &api_url,
                &api_key,
                &Default::default(),
            )
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))
            .context(i18n::t!("d183ee00dc4aef1f"))
            .map(|models| {
                models
                    .into_iter()
                    .map(|model| DiscoveredModel {
                        name: model.id,
                        max_tokens: model.max_input_tokens,
                        max_output_tokens: model.max_output_tokens,
                        supports_tools: true,
                        supports_images: model.supports_images,
                        supports_thinking: model.supports_thinking,
                    })
                    .collect::<Vec<_>>()
            }),
        };

        this.update_in(cx, |this, window, cx| {
            let Some(form) = this.llm_provider_form.as_mut() else {
                return;
            };
            if form.id != form_id {
                return;
            }
            form.is_fetching_models = false;
            match result {
                Ok(models) if models.is_empty() => {
                    form.error = Some(i18n::t!("c7b3d64add4ec35e").into());
                }
                Ok(models) => {
                    let names = form.models.iter().map(|model| model.name.read(cx).text(cx));
                    let models = new_discovered_models(names, models);
                    let added = models.len();
                    for model in models {
                        form.models.push(ModelInput::from_discovered(
                            form.models.len(),
                            model,
                            window,
                            cx,
                        ));
                    }
                    form.notice = Some(i18n::t!("4e2791001ca690a5", added = added).into());
                }
                Err(error) => {
                    log::warn!("Compatible model discovery failed: {error:#}");
                    form.error = Some(i18n::t!("ca2f5726f96dc2ac").into());
                }
            }
            cx.notify();
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn new_discovered_models(
    existing: impl IntoIterator<Item = String>,
    discovered: Vec<DiscoveredModel>,
) -> Vec<DiscoveredModel> {
    let mut names: HashSet<String> = existing.into_iter().collect();
    discovered
        .into_iter()
        .filter(|model| names.insert(model.name.clone()))
        .collect()
}

struct DiscoveredModel {
    name: String,
    max_tokens: u64,
    max_output_tokens: u64,
    supports_tools: bool,
    supports_images: bool,
    supports_thinking: bool,
}

fn render_model(
    kind: CompatibleProviderKind,
    model: &ModelInput,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    v_flex()
        .p_2()
        .gap_2()
        .rounded_sm()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .bg(cx.theme().colors().element_active.opacity(0.15))
        .child(
            h_flex()
                .justify_between()
                .child(Label::new(model.name.read(cx).text(cx)).color(Color::Muted))
                .child(
                    Button::new(
                        ("expand-model", index),
                        if model.expanded {
                            i18n::t!("bf76bc5ea98fe0cd")
                        } else {
                            i18n::t!("5078505856cb904f")
                        },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(model) = this
                            .llm_provider_form
                            .as_mut()
                            .and_then(|form| form.models.get_mut(index))
                        {
                            model.expanded = !model.expanded;
                        }
                        cx.notify();
                    })),
                ),
        )
        .when(model.expanded, |this| {
            this.child(render_form_field(
                i18n::t!("cb2f1709f983d2f4"),
                i18n::t!("6514a7130bf4071c"),
                &model.name,
                cx,
            ))
        })
        .when(
            model.expanded && matches!(kind, CompatibleProviderKind::OpenAi),
            |this| {
                this.child(render_form_field(
                    i18n::t!("dc41e7648bb28044"),
                    i18n::t!("1815716af4dac34f"),
                    &model.max_completion_tokens,
                    cx,
                ))
            },
        )
        .when(model.expanded, |this| {
            this.child(render_form_field(
                i18n::t!("94409bc79114731f"),
                i18n::t!("da1d53395f6aa3b2"),
                &model.max_output_tokens,
                cx,
            ))
        })
        .when(model.expanded, |this| {
            this.child(render_form_field(
                i18n::t!("7c1d0708fac48697"),
                i18n::t!("2656f7307edc0a88"),
                &model.max_tokens,
                cx,
            ))
        })
        .when(model.expanded, |this| {
            this.child(render_model_capabilities(kind, model, index, window, cx))
        })
        .child(
            Button::new(("remove-model", index), i18n::t!("11c67ef20336beed"))
                .start_icon(
                    Icon::new(IconName::Trash)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Outlined)
                .full_width()
                .on_click(cx.listener(move |this, _, _window, cx| {
                    if let Some(form) = this.llm_provider_form.as_mut()
                        && index < form.models.len()
                    {
                        form.models.remove(index);
                    }
                    cx.notify();
                })),
        )
        .into_any_element()
}

fn render_model_capabilities(
    kind: CompatibleProviderKind,
    model: &ModelInput,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(render_capability_checkbox(
            "supports-tools",
            index,
            i18n::t!("c9954fa45c7dab88"),
            model.supports_tools,
            |model, state| model.supports_tools = state,
            cx,
        ))
        .child(render_capability_checkbox(
            "supports-images",
            index,
            i18n::t!("f1fff850de6df26a"),
            model.supports_images,
            |model, state| model.supports_images = state,
            cx,
        ))
        .when(matches!(kind, CompatibleProviderKind::OpenAi), |this| {
            this.child(render_capability_checkbox(
                "supports-parallel-tool-calls",
                index,
                i18n::t!("afdfe6960a6fc0ab"),
                model.supports_parallel_tool_calls,
                |model, state| model.supports_parallel_tool_calls = state,
                cx,
            ))
            .child(render_capability_checkbox(
                "supports-prompt-cache-key",
                index,
                i18n::t!("ea9383eb6d15eab5"),
                model.supports_prompt_cache_key,
                |model, state| model.supports_prompt_cache_key = state,
                cx,
            ))
            .child(render_capability_checkbox(
                "supports-chat-completions",
                index,
                i18n::t!("ca73231a059e1c48"),
                model.supports_chat_completions,
                |model, state| model.supports_chat_completions = state,
                cx,
            ))
            .when(model.supports_chat_completions.selected(), |this| {
                this.child(render_capability_checkbox(
                    "max-tokens-parameter",
                    index,
                    i18n::t!("c0c797759ff5ddf9"),
                    model.max_tokens_parameter,
                    |model, state| model.max_tokens_parameter = state,
                    cx,
                ))
            })
            .child(render_capability_checkbox(
                "supports-thinking",
                index,
                i18n::t!("ef810f2360d1100c"),
                model.supports_thinking,
                |model, state| model.supports_thinking = state,
                cx,
            ))
            .when(model.supports_thinking.selected(), |this| {
                this.child(render_reasoning_effort_selector(
                    model.reasoning_effort,
                    index,
                    window,
                    cx,
                ))
                .when(model.supports_chat_completions.selected(), |this| {
                    this.child(render_capability_checkbox(
                        "interleaved-reasoning",
                        index,
                        i18n::t!("509c1018e3ecefb8"),
                        model.interleaved_reasoning,
                        |model, state| model.interleaved_reasoning = state,
                        cx,
                    ))
                })
            })
        })
}

fn render_capability_checkbox(
    id: &'static str,
    index: usize,
    label: &'static str,
    state: ToggleState,
    update: fn(&mut ModelInput, ToggleState),
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    Checkbox::new((id, index), state)
        .label(label)
        .on_click(cx.listener(move |this, checked, _window, cx| {
            if let Some(form) = this.llm_provider_form.as_mut()
                && let Some(model) = form.models.get_mut(index)
            {
                update(model, *checked);
            }
            cx.notify();
        }))
}

fn render_reasoning_effort_selector(
    selected: OpenAiReasoningEffort,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let settings_window = cx.weak_entity();
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        for effort in OpenAiReasoningEffort::OPENAI_COMPATIBLE_SELECTABLE {
            let is_selected = effort == selected;
            let settings_window = settings_window.clone();
            menu.push_item(
                ui::ContextMenuEntry::new(effort.label())
                    .toggleable(IconPosition::End, is_selected)
                    .handler(move |_window, cx| {
                        settings_window
                            .update(cx, |this, cx| {
                                if let Some(form) = this.llm_provider_form.as_mut()
                                    && let Some(model) = form.models.get_mut(index)
                                {
                                    model.reasoning_effort = effort;
                                }
                                cx.notify();
                            })
                            .ok();
                    }),
            );
        }
        menu
    });

    v_flex()
        .gap_1()
        .child(Label::new(i18n::t!("b0d949fca8e26ef5")).size(LabelSize::Small))
        .child(
            DropdownMenu::new(
                ElementId::Name(format!("reasoning-effort-selector-{index}").into()),
                selected.label(),
                menu,
            )
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .full_width(true)
            .aria_label(i18n::t!("b0d949fca8e26ef5")),
        )
}

fn render_form_error(error: SharedString) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .child(
            Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error),
        )
        .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
}

fn render_form_actions(is_saving: bool, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_1()
        .justify_end()
        .child(
            Button::new("llm-provider-form-cancel", i18n::t!("2cd0f3be8738a86c"))
                .disabled(is_saving)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.pop_sub_page(window, cx);
                })),
        )
        .child(
            Button::new("llm-provider-form-save", i18n::t!("ab65008428009126"))
                .style(ButtonStyle::Filled)
                .disabled(is_saving)
                .on_click(cx.listener(|this, _, window, cx| {
                    save_llm_provider_form(this, window, cx);
                })),
        )
}

struct LlmProviderFormValues {
    kind: CompatibleProviderKind,
    provider_name: String,
    api_url: String,
    api_key: String,
    models: Vec<ModelValues>,
    editing_name: Option<String>,
    original_api_url: Option<String>,
}

struct ModelValues {
    name: String,
    max_completion_tokens: String,
    max_output_tokens: String,
    max_tokens: String,
    reasoning_effort: OpenAiReasoningEffort,
    supports_tools: bool,
    supports_images: bool,
    supports_parallel_tool_calls: bool,
    supports_prompt_cache_key: bool,
    supports_chat_completions: bool,
    supports_thinking: bool,
    interleaved_reasoning: bool,
    max_tokens_parameter: bool,
    original: Option<OriginalModel>,
}

enum ParsedModels {
    OpenAi(Vec<OpenAiCompatibleAvailableModel>),
    Anthropic(Vec<AnthropicCompatibleAvailableModel>),
}

fn save_llm_provider_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let values = {
        let Some(form) = settings_window.llm_provider_form.as_ref() else {
            return;
        };
        if form.is_saving {
            return;
        }
        LlmProviderFormValues {
            kind: form.kind,
            editing_name: form.editing_name.clone(),
            original_api_url: form.original_api_url.clone(),
            provider_name: form.provider_name.read(cx).text(cx),
            api_url: form.api_url.read(cx).text(cx),
            api_key: form.api_key.read(cx).text(cx),
            models: form
                .models
                .iter()
                .map(|model| ModelValues {
                    name: model.name.read(cx).text(cx),
                    max_completion_tokens: model.max_completion_tokens.read(cx).text(cx),
                    max_output_tokens: model.max_output_tokens.read(cx).text(cx),
                    max_tokens: model.max_tokens.read(cx).text(cx),
                    reasoning_effort: model.reasoning_effort,
                    supports_tools: model.supports_tools.selected(),
                    supports_images: model.supports_images.selected(),
                    supports_parallel_tool_calls: model.supports_parallel_tool_calls.selected(),
                    supports_prompt_cache_key: model.supports_prompt_cache_key.selected(),
                    supports_chat_completions: model.supports_chat_completions.selected(),
                    supports_thinking: model.supports_thinking.selected(),
                    interleaved_reasoning: model.interleaved_reasoning.selected(),
                    max_tokens_parameter: model.max_tokens_parameter.selected(),
                    original: model.original.clone(),
                })
                .collect(),
        }
    };

    let (provider_name, api_url, api_key, models) = match validate_llm_provider_form(&values, cx) {
        Ok(value) => value,
        Err(error) => {
            if let Some(form) = settings_window.llm_provider_form.as_mut() {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
    };

    if let Some(form) = settings_window.llm_provider_form.as_mut() {
        form.is_saving = true;
        form.error = None;
        form.notice = None;
    }
    cx.notify();
    let form_id = settings_window
        .llm_provider_form
        .as_ref()
        .map(|form| form.id);
    let fs = <dyn fs::Fs>::global(cx);
    cx.spawn_in(window, async move |this, cx| {
        let result = async {
            let provider_id = LanguageModelProviderId(provider_name.clone().into());
            let settings_update = cx.update(|_window, cx| {
                settings::update_settings_file_with_completion(fs, cx, move |settings, _cx| {
                    let language_models = settings.language_models.get_or_insert_default();
                    match models {
                        ParsedModels::OpenAi(available_models) => {
                            let providers =
                                language_models.openai_compatible.get_or_insert_default();
                            providers.insert(
                                Arc::from(provider_name.as_str()),
                                OpenAiCompatibleSettingsContent {
                                    api_url: api_url.clone(),
                                    available_models,
                                    custom_headers: providers
                                        .get(provider_name.as_str())
                                        .and_then(|previous| previous.custom_headers.clone()),
                                },
                            );
                        }
                        ParsedModels::Anthropic(available_models) => {
                            let providers =
                                language_models.anthropic_compatible.get_or_insert_default();
                            providers.insert(
                                Arc::from(provider_name.as_str()),
                                AnthropicCompatibleSettingsContent {
                                    api_url: api_url.clone(),
                                    available_models,
                                    custom_headers: providers
                                        .get(provider_name.as_str())
                                        .and_then(|previous| previous.custom_headers.clone()),
                                },
                            );
                        }
                    }
                })
            })?;

            settings_update
                .await
                .map_err(|_| anyhow::anyhow!(i18n::t!("f36e07af8e9558c1")))??;

            if !api_key.is_empty() {
                let set_api_key = cx.update(|_window, cx| {
                    let provider = LanguageModelRegistry::read_global(cx)
                        .provider(&provider_id)
                        .ok_or_else(|| anyhow::anyhow!(i18n::t!("274e50973762b2a2")))?;
                    anyhow::Ok(provider.set_api_key(Some(api_key), cx))
                })??;
                set_api_key.await?;
            }

            cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    this.provider_configuration_views.remove(&provider_id);
                    if this
                        .llm_provider_form
                        .as_ref()
                        .is_some_and(|form| Some(form.id) == form_id)
                    {
                        this.llm_provider_form = None;
                        this.pop_sub_page(window, cx);
                    }
                })
            })??;

            anyhow::Ok(())
        }
        .await;

        if let Err(error) = result {
            this.update(cx, |this, cx| {
                if let Some(form) = this
                    .llm_provider_form
                    .as_mut()
                    .filter(|form| Some(form.id) == form_id)
                {
                    form.is_saving = false;
                    form.error = Some(error.to_string().into());
                    cx.notify();
                }
            })?;
        }

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn validate_llm_provider_form(
    values: &LlmProviderFormValues,
    cx: &App,
) -> Result<(String, String, String, ParsedModels), SharedString> {
    let provider_name = values.provider_name.trim().to_string();
    if provider_name.is_empty() {
        return Err(i18n::t!("95cfcff3c0d1ef8b").into());
    }

    if values
        .editing_name
        .as_deref()
        .is_some_and(|name| name != provider_name)
    {
        return Err(i18n::t!("17435c31ea11eb00").into());
    }
    if values.editing_name.is_none()
        && LanguageModelRegistry::read_global(cx)
            .providers()
            .iter()
            .any(|provider| {
                provider.id().0.as_ref() == provider_name.as_str()
                    || provider.name().0.as_ref() == provider_name.as_str()
            })
    {
        return Err(i18n::t!("05d915294bee06eb").into());
    }

    let api_url = values.api_url.trim().to_string();
    if api_url.is_empty() {
        return Err(i18n::t!("b15ce5e6298d2cf2").into());
    }

    let api_key = values.api_key.trim().to_string();
    validate_credential_change(
        values.editing_name.as_deref(),
        values.original_api_url.as_deref(),
        &api_url,
        &api_key,
    )?;

    if values.models.is_empty() {
        return Err(i18n::t!("639a0aaf68ca7fb0").into());
    }

    let models = match values.kind {
        CompatibleProviderKind::OpenAi => ParsedModels::OpenAi(
            values
                .models
                .iter()
                .map(parse_open_ai_model)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        CompatibleProviderKind::Anthropic => ParsedModels::Anthropic(
            values
                .models
                .iter()
                .map(parse_anthropic_model)
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };

    let mut model_names = HashSet::new();
    let model_names_are_unique = match &models {
        ParsedModels::OpenAi(models) => models
            .iter()
            .all(|model| model_names.insert(model.name.clone())),
        ParsedModels::Anthropic(models) => models
            .iter()
            .all(|model| model_names.insert(model.name.clone())),
    };
    if !model_names_are_unique {
        return Err(i18n::t!("dbd3af7b0881a5f4").into());
    }

    Ok((provider_name, api_url, api_key, models))
}

fn validate_credential_change(
    editing_name: Option<&str>,
    original_api_url: Option<&str>,
    api_url: &str,
    api_key: &str,
) -> Result<(), SharedString> {
    if api_key.is_empty() && editing_name.is_none() {
        return Err(i18n::t!("d8f8de45f8a719df").into());
    }
    if api_key.is_empty() && original_api_url != Some(api_url) {
        return Err(i18n::t!("c7b5eb9d180ed235").into());
    }
    Ok(())
}

fn parse_model_name(model: &ModelValues) -> Result<String, SharedString> {
    let name = model.name.trim();
    if name.is_empty() {
        return Err(i18n::t!("1a8582fa6b62ac0e").into());
    }
    Ok(name.to_string())
}

fn parse_open_ai_model(
    model: &ModelValues,
) -> Result<OpenAiCompatibleAvailableModel, SharedString> {
    let original = match &model.original {
        Some(OriginalModel::OpenAi(original)) if original.name == model.name.trim() => {
            Some(original)
        }
        _ => None,
    };
    let max_output_tokens =
        parse_u64_field(&model.max_output_tokens, i18n::t!("94409bc79114731f"))?;
    let max_completion_tokens =
        parse_u64_field(&model.max_completion_tokens, i18n::t!("dc41e7648bb28044"))?;
    Ok(OpenAiCompatibleAvailableModel {
        name: parse_model_name(model)?,
        display_name: original.and_then(|original| original.display_name.clone()),
        max_completion_tokens: if original
            .is_some_and(|original| original.max_completion_tokens.is_none())
            && max_completion_tokens == 200_000
        {
            None
        } else {
            Some(max_completion_tokens)
        },
        max_output_tokens: if original.is_some_and(|original| original.max_output_tokens.is_none())
            && max_output_tokens == 32_000
        {
            None
        } else {
            Some(max_output_tokens)
        },
        max_tokens: parse_u64_field(&model.max_tokens, i18n::t!("7c1d0708fac48697"))?,
        reasoning_effort: model.supports_thinking.then_some(model.reasoning_effort),
        capabilities: OpenAiCompatibleModelCapabilities {
            tools: model.supports_tools,
            images: model.supports_images,
            parallel_tool_calls: model.supports_parallel_tool_calls,
            prompt_cache_key: model.supports_prompt_cache_key,
            chat_completions: model.supports_chat_completions,
            interleaved_reasoning: model.supports_thinking
                && model.supports_chat_completions
                && model.interleaved_reasoning,
            max_tokens_parameter: model.supports_chat_completions && model.max_tokens_parameter,
        },
    })
}

fn parse_anthropic_model(
    model: &ModelValues,
) -> Result<AnthropicCompatibleAvailableModel, SharedString> {
    let original = match &model.original {
        Some(OriginalModel::Anthropic(original)) if original.name == model.name.trim() => {
            Some(original)
        }
        _ => None,
    };
    let max_output_tokens =
        parse_u64_field(&model.max_output_tokens, i18n::t!("94409bc79114731f"))?;
    let mode = original.and_then(|original| original.mode);
    Ok(AnthropicCompatibleAvailableModel {
        name: parse_model_name(model)?,
        display_name: original.and_then(|original| original.display_name.clone()),
        max_tokens: parse_u64_field(&model.max_tokens, i18n::t!("7c1d0708fac48697"))?,
        tool_override: original.and_then(|original| original.tool_override.clone()),
        max_output_tokens: if original.is_some_and(|original| original.max_output_tokens.is_none())
            && max_output_tokens == 32_000
        {
            None
        } else {
            Some(max_output_tokens)
        },
        default_temperature: original.and_then(|original| original.default_temperature),
        extra_beta_headers: original
            .map_or_else(Vec::new, |original| original.extra_beta_headers.clone()),
        mode: if model.supports_thinking {
            mode.filter(|mode| !matches!(mode, settings::ModelMode::Default))
                .or(Some(settings::ModelMode::Adaptive))
        } else {
            mode.filter(|mode| matches!(mode, settings::ModelMode::Default))
        },
        capabilities: AnthropicCompatibleModelCapabilities {
            tools: model.supports_tools,
            images: model.supports_images,
            prompt_caching: original.is_some_and(|original| original.capabilities.prompt_caching),
        },
    })
}

fn parse_u64_field(value: &str, name: &str) -> Result<u64, SharedString> {
    value
        .parse::<u64>()
        .map_err(|_| i18n::t!("d9fc7a248efd76b1", name = name).into())
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext, size};
    use language_models::provider::cloud;
    use settings::SettingsStore;

    use super::*;

    #[test]
    fn editing_credential_policy_requires_new_key_only_for_new_url() {
        assert!(validate_credential_change(None, None, "https://example.com/v1", "").is_err());
        assert!(
            validate_credential_change(
                Some("my-provider"),
                Some("https://example.com/v1"),
                "https://example.com/v1",
                ""
            )
            .is_ok()
        );
        assert!(
            validate_credential_change(
                Some("my-provider"),
                Some("https://example.com/v1"),
                "https://other.example.com/v1",
                ""
            )
            .is_err()
        );
        assert!(
            validate_credential_change(
                Some("my-provider"),
                Some("https://example.com/v1"),
                "https://other.example.com/v1",
                "new-key"
            )
            .is_ok()
        );
    }

    #[test]
    fn discovery_preserves_existing_models_and_deduplicates_results() {
        let models = ["existing", "new", "new"]
            .into_iter()
            .map(|name| DiscoveredModel {
                name: name.to_string(),
                max_tokens: 200_000,
                max_output_tokens: 32_000,
                supports_tools: true,
                supports_images: false,
                supports_thinking: false,
            })
            .collect();
        let new = new_discovered_models(["existing".to_string()], models);
        assert_eq!(
            new.iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            ["new"]
        );
    }

    struct YoungAccountProviderRow;

    impl Render for YoungAccountProviderRow {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().p_4().child(
                div()
                    .w_full()
                    .debug_selector(|| "provider-row".into())
                    .child(render_inline_body(
                        "Zed".into(),
                        Some("已订阅商业版".into()),
                        Some(InlineDescription::Text(
                            "你可以通过所属组织使用 Zed 托管的模型。".into(),
                        )),
                        cloud::test_support::young_account_configuration(),
                    )),
            )
        }
    }

    #[gpui::test]
    fn young_account_configuration_stays_within_provider_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        for width in [500., 600., 800.] {
            let window = cx.open_window(size(px(width), px(400.)), |_window, _cx| {
                YoungAccountProviderRow
            });
            cx.run_until_parked();

            let mut visual_context = VisualTestContext::from_window(window.into(), cx);
            let provider_row_bounds = visual_context
                .debug_bounds("provider-row")
                .expect("provider row should be rendered");
            let description_bounds = visual_context
                .debug_bounds("inline-provider-description")
                .expect("provider description should be rendered");
            let configuration_bounds = visual_context
                .debug_bounds("zed-ai-configuration")
                .expect("Zed AI configuration should be rendered");

            assert!(
                configuration_bounds.right() <= provider_row_bounds.right(),
                "young account configuration extends past the provider row at {width}px"
            );
            assert!(
                configuration_bounds.size.height >= px(80.),
                "young account warning does not wrap at {width}px"
            );
            assert!(
                description_bounds.size.width >= px(width / 3.),
                "provider description collapsed at {width}px"
            );
        }
    }
}
