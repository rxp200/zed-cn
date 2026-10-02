use gpui::{Action as _, App};
use itertools::Itertools as _;
use settings::{
    AudioInputDeviceName, AudioOutputDeviceName, EditPredictionDataCollectionChoice,
    LanguageSettingsContent, SemanticTokens, SettingsContent,
};
use std::sync::{Arc, OnceLock};
use strum::{EnumMessage, IntoDiscriminant as _, VariantArray};
use theme::SystemAppearance;
use ui::IntoElement;

use crate::{
    ActionLink, DynamicItem, PROJECT, SettingField, SettingItem, SettingsFieldMetadata,
    SettingsPage, SettingsPageItem, SubPageLink, USER, active_language, all_language_names,
    pages::{
        open_audio_test_window, render_edit_prediction_setup_page, render_external_agents_page,
        render_llm_providers_page, render_mcp_servers_page, render_sandbox_settings_page,
        render_skills_setup_page, render_tool_permissions_setup_page,
    },
};

const DEFAULT_STRING: String = String::new();
/// A default empty string reference. Useful in `pick` functions for cases either in dynamic item fields, or when dealing with `settings::Maybe`
/// to avoid the "NO DEFAULT" case.
const DEFAULT_EMPTY_STRING: Option<&String> = Some(&DEFAULT_STRING);

const DEFAULT_AUDIO_OUTPUT: AudioOutputDeviceName = AudioOutputDeviceName(None);
const DEFAULT_EMPTY_AUDIO_OUTPUT: Option<&AudioOutputDeviceName> = Some(&DEFAULT_AUDIO_OUTPUT);
const DEFAULT_AUDIO_INPUT: AudioInputDeviceName = AudioInputDeviceName(None);
const DEFAULT_EMPTY_AUDIO_INPUT: Option<&AudioInputDeviceName> = Some(&DEFAULT_AUDIO_INPUT);

macro_rules! concat_sections {
    (@vec, $($arr:expr),+ $(,)?) => {{
        let total_len = 0_usize $(+ $arr.len())+;
        let mut out = Vec::with_capacity(total_len);

        $(
            out.extend($arr);
        )+

        out
    }};

    ($($arr:expr),+ $(,)?) => {{
        let total_len = 0_usize $(+ $arr.len())+;

        let mut out: Box<[std::mem::MaybeUninit<_>]> = Box::new_uninit_slice(total_len);

        let mut index = 0usize;
        $(
            let array = $arr;
            for item in array {
                out[index].write(item);
                index += 1;
            }
        )+

        debug_assert_eq!(index, total_len);

        // SAFETY: we wrote exactly `total_len` elements.
        unsafe { out.assume_init() }
    }};
}

pub(crate) fn settings_data(cx: &App) -> Vec<SettingsPage> {
    vec![
        general_page(cx),
        appearance_page(),
        keymap_page(),
        editor_page(),
        languages_and_tools_page(cx),
        search_and_files_page(),
        window_and_layout_page(),
        panels_page(),
        debugger_page(),
        terminal_page(),
        version_control_page(),
        collaboration_page(),
        ai_page(cx),
        network_page(),
        developer_page(cx),
    ]
}

fn developer_page(cx: &App) -> SettingsPage {
    use feature_flags::FeatureFlagAppExt as _;

    let mut items: Vec<SettingsPageItem> = Vec::new();

    // Feature flag overrides are a staff-only affordance, so only surface the section when the overrides are enabled.
    if cx.feature_flag_overrides_enabled() {
        items.push(SettingsPageItem::SectionHeader("Feature Flags"));
        items.push(SettingsPageItem::SubPageLink(SubPageLink {
            title: i18n::t!("50f87e4af37fb572").into(),
            r#type: Default::default(),
            description: None,
            search_aliases: &[],
            json_path: Some("feature_flags"),
            in_json: true,
            files: USER,
            render: crate::pages::render_feature_flags_page,
        }));
    }

    items.push(SettingsPageItem::SectionHeader(i18n::t!(
        "83f2e6c57ab63dbf"
    )));
    items.push(SettingsPageItem::SettingItem(SettingItem {
        title: i18n::t!("55f8aa09fcb0e8e8"),
        description: i18n::t!("126b7c2c0585ea34"),
        field: Box::new(SettingField {
            organization_override: None,
            json_path: Some("instrumentation.performance_profiler.enabled"),
            pick: |settings_content| {
                settings_content
                    .instrumentation
                    .as_ref()
                    .and_then(|i| i.performance_profiler.as_ref())
                    .and_then(|p| p.enabled.as_ref())
            },
            write: |settings_content, value, _| {
                settings_content
                    .instrumentation
                    .get_or_insert_default()
                    .performance_profiler
                    .get_or_insert_default()
                    .enabled = value;
            },
        }),
        metadata: None,
        files: USER,
    }));

    SettingsPage {
        title: i18n::t!("38084d301e3f1a31"),
        items: items.into_boxed_slice(),
    }
}

fn general_page(cx: &App) -> SettingsPage {
    fn general_settings_section(_cx: &App) -> Vec<SettingsPageItem> {
        vec![
            SettingsPageItem::SectionHeader(i18n::t!("18a9d61deeb3373d")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0011112554026935"),
                description: i18n::t!("2ff8e99695359021"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("accessible_mode"),
                    pick: |settings_content| settings_content.workspace.accessible_mode.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.accessible_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("94a5143840d13839"),
                description: i18n::t!("68648cc8b6f4fcd4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("when_closing_with_no_tabs"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .when_closing_with_no_tabs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.when_closing_with_no_tabs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("430a1ffc7492b510"),
                description: i18n::t!("bf39996e71a45bf3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("on_new_window"),
                    pick: |settings_content| settings_content.workspace.on_new_window.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.on_new_window = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("497cae10a9114234"),
                description: i18n::t!("7404121120333f5e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("on_last_window_closed"),
                    pick: |settings_content| {
                        settings_content.workspace.on_last_window_closed.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.on_last_window_closed = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("484a7f00ad7ccc09"),
                description: i18n::t!("fb94b0ea927e2d81"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_system_path_prompts"),
                    pick: |settings_content| {
                        settings_content.workspace.use_system_path_prompts.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.use_system_path_prompts = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1955cc1e1e020c69"),
                description: i18n::t!("a92718b5c1f700f0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_system_prompts"),
                    pick: |settings_content| settings_content.workspace.use_system_prompts.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.use_system_prompts = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a97ed73eef0ae006"),
                description: i18n::t!("3f5597c781adf20c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("redact_private_values"),
                    pick: |settings_content| settings_content.editor.redact_private_values.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.redact_private_values = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e57f571d6140a6e5"),
                description: i18n::t!("c85beffc5fc8cb97"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("private_files"),
                        pick: |settings_content| {
                            settings_content.project.worktree.private_files.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.private_files = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8850d86331664bfa"),
                description: i18n::t!("46b02620639dcbfb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("cli_default_open_behavior"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .cli_default_open_behavior
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.cli_default_open_behavior = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("28ea4ef555bfc0f1"),
                description: i18n::t!("adb2700141113411"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("reveal_if_open"),
                    pick: |settings_content| settings_content.workspace.reveal_if_open.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.reveal_if_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3a28b3c4bae3afe1"),
                description: i18n::t!("082fc6a07f61639a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("default_open_behavior"),
                    pick: |settings_content| {
                        settings_content.workspace.default_open_behavior.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.default_open_behavior = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]
    }
    fn security_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("afb63a620bdcff15")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b5d58f5a4aeb21b9"),
                description: i18n::t!("0c7dac9871771a0d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("session.trust_all_projects"),
                    pick: |settings_content| {
                        settings_content
                            .session
                            .as_ref()
                            .and_then(|session| session.trust_all_worktrees.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .session
                            .get_or_insert_default()
                            .trust_all_worktrees = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn workspace_restoration_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("75d0f8540f72fa2d")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9d915ae9f17bbb31"),
                description: i18n::t!("3ec7ceeac4510475"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("session.restore_unsaved_buffers"),
                    pick: |settings_content| {
                        settings_content
                            .session
                            .as_ref()
                            .and_then(|session| session.restore_unsaved_buffers.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .session
                            .get_or_insert_default()
                            .restore_unsaved_buffers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0c7db6a74c3b2cdf"),
                description: i18n::t!("2ce6e5e9c43d29c4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("restore_on_startup"),
                    pick: |settings_content| settings_content.workspace.restore_on_startup.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.restore_on_startup = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scoped_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("10e4c125a796807d")),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("7c7714a1fd7f5315"),
                description: i18n::t!("5a1d783af6b7002a"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("preview_channel_settings"),
                        pick: |settings_content| Some(settings_content),
                        write: |_settings_content, _value, _| {},
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("0e2d914fdf49c377"),
                description: i18n::t!("c9ccbb2742139c80"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("settings_profiles"),
                        pick: |settings_content| Some(settings_content),
                        write: |_settings_content, _value, _| {},
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
        ]
    }

    fn privacy_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("86651d17a401c55b")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("58b224818bcc926a"),
                description: i18n::t!("9bbab95bdaf2704d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("telemetry.diagnostics"),
                    pick: |settings_content| {
                        settings_content
                            .telemetry
                            .as_ref()
                            .and_then(|telemetry| telemetry.diagnostics.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .telemetry
                            .get_or_insert_default()
                            .diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8373d3ec31d728ea"),
                description: i18n::t!("38525d13fb1fe980"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("telemetry.metrics"),
                    pick: |settings_content| {
                        settings_content
                            .telemetry
                            .as_ref()
                            .and_then(|telemetry| telemetry.metrics.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.telemetry.get_or_insert_default().metrics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0469ffab0e7b13aa"),
                description: i18n::t!("12af6b29d0857418"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("telemetry.anthropic_retention"),
                    pick: |settings_content| {
                        settings_content
                            .telemetry
                            .as_ref()
                            .and_then(|telemetry| telemetry.anthropic_retention.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .telemetry
                            .get_or_insert_default()
                            .anthropic_retention = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn auto_update_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("736cff237d7d9255")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("736cff237d7d9255"),
                description: i18n::t!("0a2b66530a9a451e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("auto_update"),
                    pick: |settings_content| settings_content.auto_update.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.auto_update = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn language_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("9f6fee1aba17a565")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3d13868593ae4eeb"),
                description: i18n::t!("502c405590993170"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("language"),
                    pick: |settings_content| settings_content.language.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.language = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("835b700e028c9b20"),
        items: concat_sections!(
            @vec,
            language_section(),
            general_settings_section(cx),
            security_section(),
            workspace_restoration_section(),
            scoped_settings_section(),
            privacy_section(),
            auto_update_section(),
        )
        .into(),
    }
}

fn appearance_page() -> SettingsPage {
    fn theme_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("788db1cfec2a3db5")),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("44fb814b166ed6ae"),
                    description: i18n::t!("93e7de90e33550c8"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("theme$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::ThemeSelection>()[settings_content
                                    .theme
                                    .theme
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, app: &App| {
                            let Some(value) = value else {
                                settings_content.theme.theme = None;
                                return;
                            };
                            let settings_value =
                                settings_content.theme.theme.get_or_insert_default();
                            *settings_value = match value {
                                settings::ThemeSelectionDiscriminants::Static => {
                                    let name = match settings_value {
                                        settings::ThemeSelection::Static(_) => return,
                                        settings::ThemeSelection::Dynamic { mode, light, dark } => {
                                            match mode {
                                                theme_settings::ThemeAppearanceMode::Light => {
                                                    light.clone()
                                                }
                                                theme_settings::ThemeAppearanceMode::Dark => {
                                                    dark.clone()
                                                }
                                                theme_settings::ThemeAppearanceMode::System => {
                                                    if SystemAppearance::global(app).is_light() {
                                                        light.clone()
                                                    } else {
                                                        dark.clone()
                                                    }
                                                }
                                            }
                                        }
                                    };
                                    settings::ThemeSelection::Static(name)
                                }
                                settings::ThemeSelectionDiscriminants::Dynamic => {
                                    let static_name = match settings_value {
                                        settings::ThemeSelection::Static(theme_name) => {
                                            theme_name.clone()
                                        }
                                        settings::ThemeSelection::Dynamic { .. } => return,
                                    };

                                    settings::ThemeSelection::Dynamic {
                                        mode: settings::ThemeAppearanceMode::System,
                                        light: static_name.clone(),
                                        dark: static_name,
                                    }
                                }
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.theme.theme.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::ThemeSelection>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::ThemeSelectionDiscriminants::Static => vec![SettingItem {
                            files: USER,
                            title: i18n::t!("e479c05c94a10744"),
                            description: i18n::t!("6a7d5983eb67f3e0"),
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("theme"),
                                pick: |settings_content| match settings_content.theme.theme.as_ref()
                                {
                                    Some(settings::ThemeSelection::Static(name)) => Some(name),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        return;
                                    };
                                    match settings_content.theme.theme.get_or_insert_default() {
                                        settings::ThemeSelection::Static(theme_name) => {
                                            *theme_name = value
                                        }
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                        settings::ThemeSelectionDiscriminants::Dynamic => vec![
                            SettingItem {
                                files: USER,
                                title: i18n::t!("47a270081ab2892f"),
                                description: i18n::t!("2b5b7cc4ccfa598e"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("theme.mode"),
                                    pick: |settings_content| match settings_content
                                        .theme
                                        .theme
                                        .as_ref()
                                    {
                                        Some(settings::ThemeSelection::Dynamic {
                                            mode, ..
                                        }) => Some(mode),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content.theme.theme.get_or_insert_default() {
                                            settings::ThemeSelection::Dynamic { mode, .. } => {
                                                *mode = value
                                            }
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: i18n::t!("4bab51186df53fc0"),
                                description: i18n::t!("d5cd89e02d9af4a3"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("theme.light"),
                                    pick: |settings_content| match settings_content
                                        .theme
                                        .theme
                                        .as_ref()
                                    {
                                        Some(settings::ThemeSelection::Dynamic {
                                            light, ..
                                        }) => Some(light),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content.theme.theme.get_or_insert_default() {
                                            settings::ThemeSelection::Dynamic { light, .. } => {
                                                *light = value
                                            }
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: i18n::t!("352a598cd470b1e1"),
                                description: i18n::t!("d8ba08df42e60a61"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("theme.dark"),
                                    pick: |settings_content| match settings_content
                                        .theme
                                        .theme
                                        .as_ref()
                                    {
                                        Some(settings::ThemeSelection::Dynamic {
                                            dark, ..
                                        }) => Some(dark),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content.theme.theme.get_or_insert_default() {
                                            settings::ThemeSelection::Dynamic { dark, .. } => {
                                                *dark = value
                                            }
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            },
                        ],
                    })
                    .collect(),
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("62aec9c5a86a0bff"),
                    description: i18n::t!("82a7fd7370b147c9"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("icon_theme$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::IconThemeSelection>()[settings_content
                                    .theme
                                    .icon_theme
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, app| {
                            let Some(value) = value else {
                                settings_content.theme.icon_theme = None;
                                return;
                            };
                            let settings_value =
                                settings_content.theme.icon_theme.get_or_insert_with(|| {
                                    settings::IconThemeSelection::Static(settings::IconThemeName(
                                        theme::default_icon_theme().name.clone().into(),
                                    ))
                                });
                            *settings_value = match value {
                                settings::IconThemeSelectionDiscriminants::Static => {
                                    let name = match settings_value {
                                        settings::IconThemeSelection::Static(_) => return,
                                        settings::IconThemeSelection::Dynamic {
                                            mode,
                                            light,
                                            dark,
                                        } => match mode {
                                            theme_settings::ThemeAppearanceMode::Light => {
                                                light.clone()
                                            }
                                            theme_settings::ThemeAppearanceMode::Dark => {
                                                dark.clone()
                                            }
                                            theme_settings::ThemeAppearanceMode::System => {
                                                if SystemAppearance::global(app).is_light() {
                                                    light.clone()
                                                } else {
                                                    dark.clone()
                                                }
                                            }
                                        },
                                    };
                                    settings::IconThemeSelection::Static(name)
                                }
                                settings::IconThemeSelectionDiscriminants::Dynamic => {
                                    let static_name = match settings_value {
                                        settings::IconThemeSelection::Static(theme_name) => {
                                            theme_name.clone()
                                        }
                                        settings::IconThemeSelection::Dynamic { .. } => return,
                                    };

                                    settings::IconThemeSelection::Dynamic {
                                        mode: settings::ThemeAppearanceMode::System,
                                        light: static_name.clone(),
                                        dark: static_name,
                                    }
                                }
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.theme.icon_theme.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::IconThemeSelection>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::IconThemeSelectionDiscriminants::Static => vec![SettingItem {
                            files: USER,
                            title: i18n::t!("f4cf8b83d5ab239d"),
                            description: i18n::t!("24dea9cf0df255a7"),
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("icon_theme$string"),
                                pick: |settings_content| match settings_content
                                    .theme
                                    .icon_theme
                                    .as_ref()
                                {
                                    Some(settings::IconThemeSelection::Static(name)) => Some(name),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        return;
                                    };
                                    match settings_content.theme.icon_theme.as_mut() {
                                        Some(settings::IconThemeSelection::Static(theme_name)) => {
                                            *theme_name = value
                                        }
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                        settings::IconThemeSelectionDiscriminants::Dynamic => vec![
                            SettingItem {
                                files: USER,
                                title: i18n::t!("47a270081ab2892f"),
                                description: i18n::t!("9007a2953d994e93"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("icon_theme"),
                                    pick: |settings_content| match settings_content
                                        .theme
                                        .icon_theme
                                        .as_ref()
                                    {
                                        Some(settings::IconThemeSelection::Dynamic {
                                            mode,
                                            ..
                                        }) => Some(mode),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content.theme.icon_theme.as_mut() {
                                            Some(settings::IconThemeSelection::Dynamic {
                                                mode,
                                                ..
                                            }) => *mode = value,
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: i18n::t!("c71bc97ae617a5ea"),
                                description: i18n::t!("e2fb7f0e9ae17c5c"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("icon_theme.light"),
                                    pick: |settings_content| match settings_content
                                        .theme
                                        .icon_theme
                                        .as_ref()
                                    {
                                        Some(settings::IconThemeSelection::Dynamic {
                                            light,
                                            ..
                                        }) => Some(light),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content.theme.icon_theme.as_mut() {
                                            Some(settings::IconThemeSelection::Dynamic {
                                                light,
                                                ..
                                            }) => *light = value,
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER,
                                title: i18n::t!("9b43fcaa46b8d4fb"),
                                description: i18n::t!("176aeffe0d1066c9"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("icon_theme.dark"),
                                    pick: |settings_content| match settings_content
                                        .theme
                                        .icon_theme
                                        .as_ref()
                                    {
                                        Some(settings::IconThemeSelection::Dynamic {
                                            dark,
                                            ..
                                        }) => Some(dark),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content.theme.icon_theme.as_mut() {
                                            Some(settings::IconThemeSelection::Dynamic {
                                                dark,
                                                ..
                                            }) => *dark = value,
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            },
                        ],
                    })
                    .collect(),
            }),
        ]
    }

    fn buffer_font_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("186e55deaf9d600b")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c75431892ad1880c"),
                description: i18n::t!("38f8f77dd0e34436"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("buffer_font_family"),
                    pick: |settings_content| settings_content.theme.buffer_font_family.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0c30c37c6ead953b"),
                description: i18n::t!("e22ddb0ca2a11cba"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("buffer_font_size"),
                    pick: |settings_content| settings_content.theme.buffer_font_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("db0c79d9d7d6c577"),
                description: i18n::t!("4b70b60e6fbbb670"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("buffer_font_weight"),
                    pick: |settings_content| settings_content.theme.buffer_font_weight.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.buffer_font_weight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("6b44b7ba432abf47"),
                    description: i18n::t!("e7632e55ead5db3f"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("buffer_line_height$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::BufferLineHeight>()[settings_content
                                    .theme
                                    .buffer_line_height
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                settings_content.theme.buffer_line_height = None;
                                return;
                            };
                            let settings_value = settings_content
                                .theme
                                .buffer_line_height
                                .get_or_insert_with(|| settings::BufferLineHeight::default());
                            *settings_value = match value {
                                settings::BufferLineHeightDiscriminants::Comfortable => {
                                    settings::BufferLineHeight::Comfortable
                                }
                                settings::BufferLineHeightDiscriminants::Standard => {
                                    settings::BufferLineHeight::Standard
                                }
                                settings::BufferLineHeightDiscriminants::Custom => {
                                    let custom_value =
                                        theme_settings::buffer_line_height_from_settings(
                                            *settings_value,
                                        )
                                        .value();
                                    settings::BufferLineHeight::Custom(custom_value)
                                }
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .theme
                            .buffer_line_height
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::BufferLineHeight>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::BufferLineHeightDiscriminants::Comfortable => vec![],
                        settings::BufferLineHeightDiscriminants::Standard => vec![],
                        settings::BufferLineHeightDiscriminants::Custom => vec![SettingItem {
                            files: USER,
                            title: i18n::t!("bbc3144ff5dbba47"),
                            description: i18n::t!("7d1ffbddcebfbf73"),
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("buffer_line_height"),
                                pick: |settings_content| match settings_content
                                    .theme
                                    .buffer_line_height
                                    .as_ref()
                                {
                                    Some(settings::BufferLineHeight::Custom(value)) => Some(value),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        return;
                                    };
                                    match settings_content.theme.buffer_line_height.as_mut() {
                                        Some(settings::BufferLineHeight::Custom(line_height)) => {
                                            *line_height = f32::max(value, 1.0)
                                        }
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                    })
                    .collect(),
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("cf2674acf2bbad54"),
                description: i18n::t!("4b5a43a25fa5f0ff"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("buffer_font_features"),
                        pick: |settings_content| {
                            settings_content.theme.buffer_font_features.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.theme.buffer_font_features = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("e037c3476fcbc213"),
                description: i18n::t!("094ed8739a6c6686"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("buffer_font_fallbacks"),
                        pick: |settings_content| {
                            settings_content.theme.buffer_font_fallbacks.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.theme.buffer_font_fallbacks = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
        ]
    }

    fn ui_font_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("a77a611a7c8298ed")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c75431892ad1880c"),
                description: i18n::t!("6b1c1e42e29de331"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ui_font_family"),
                    pick: |settings_content| settings_content.theme.ui_font_family.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0c30c37c6ead953b"),
                description: i18n::t!("5b16dc095701bc9f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ui_font_size"),
                    pick: |settings_content| settings_content.theme.ui_font_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("db0c79d9d7d6c577"),
                description: i18n::t!("5585c9b8f1361032"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("ui_font_weight"),
                    pick: |settings_content| settings_content.theme.ui_font_weight.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.ui_font_weight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("cf2674acf2bbad54"),
                description: i18n::t!("f008b89d5c0ee08f"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("ui_font_features"),
                        pick: |settings_content| settings_content.theme.ui_font_features.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.theme.ui_font_features = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("e037c3476fcbc213"),
                description: i18n::t!("4a6b40d42638980c"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("ui_font_fallbacks"),
                        pick: |settings_content| settings_content.theme.ui_font_fallbacks.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.theme.ui_font_fallbacks = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
        ]
    }

    fn agent_panel_font_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("5373158eadbed7a4")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("caa57f17b9d8386b"),
                description: i18n::t!("8f112d03376f5482"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_ui_font_family"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_ui_font_family
                            .as_ref()
                            .or(settings_content.theme.ui_font_family.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_ui_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b107622308080eb1"),
                description: i18n::t!("b303f07f5c38bd76"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_ui_font_size"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_ui_font_size
                            .as_ref()
                            .or(settings_content.theme.ui_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_ui_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("57a6ff77dd42b65d"),
                description: i18n::t!("33e6bde3aabba581"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_buffer_font_family"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_buffer_font_family
                            .as_ref()
                            .or(settings_content.theme.buffer_font_family.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_buffer_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("59265ec3bf4a871d"),
                description: i18n::t!("6db08a1c683ee3e3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent_buffer_font_size"),
                    pick: |settings_content| {
                        settings_content
                            .theme
                            .agent_buffer_font_size
                            .as_ref()
                            .or(settings_content.theme.buffer_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.theme.agent_buffer_font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn markdown_preview_font_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("a6ddab2ec94b01c1")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c75431892ad1880c"),
                description: i18n::t!("2ad15be3c6cafe94"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("markdown_preview.font_family"),
                    pick: |settings_content| {
                        settings_content
                            .markdown_preview
                            .as_ref()?
                            .font_family
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .markdown_preview
                            .get_or_insert_default()
                            .font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f9eef84daaf8157e"),
                description: i18n::t!("d0c6dfe6fafc98d8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("markdown_preview.code_font_family"),
                    pick: |settings_content| {
                        settings_content
                            .markdown_preview
                            .as_ref()?
                            .code_font_family
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .markdown_preview
                            .get_or_insert_default()
                            .code_font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0c30c37c6ead953b"),
                description: i18n::t!("71b093c6ec5912a2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("markdown_preview.font_size"),
                    pick: |settings_content| {
                        settings_content
                            .markdown_preview
                            .as_ref()
                            .and_then(|preview| preview.font_size.as_ref())
                            .or(settings_content.theme.buffer_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .markdown_preview
                            .get_or_insert_default()
                            .font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn text_rendering_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("72bb8c9e6023e181")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7e428f9821e9119c"),
                description: i18n::t!("0b3b638e41374680"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("text_rendering_mode"),
                    pick: |settings_content| {
                        settings_content.workspace.text_rendering_mode.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.text_rendering_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn cursor_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("47a75ca0b8fef37c")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("19456bb1882a9d62"),
                description: i18n::t!("a5704c1f95ac3cbf"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("multi_cursor_modifier"),
                    pick: |settings_content| settings_content.editor.multi_cursor_modifier.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.multi_cursor_modifier = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9342b40e1c885cee"),
                description: i18n::t!("5b7b4dca3b8136df"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("cursor_blink"),
                    pick: |settings_content| settings_content.editor.cursor_blink.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.cursor_blink = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("55000845a57808af"),
                description: i18n::t!("9b63faf4f0acdbd6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("cursor_animation.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .cursor_animation
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .cursor_animation
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("98ec5a07a6ee6050"),
                description: i18n::t!("5ce23a27e0fe20e6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("cursor_shape"),
                    pick: |settings_content| settings_content.editor.cursor_shape.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.cursor_shape = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9e2bf7c699e18d4e"),
                description: i18n::t!("a9668db274aa56d8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hide_mouse"),
                    pick: |settings_content| settings_content.hide_mouse.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.hide_mouse = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b4ec700fc0ee22a7"),
                description: i18n::t!("5618d04c8832f162"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("reduce_motion"),
                    pick: |settings_content| settings_content.reduce_motion.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.reduce_motion = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn highlighting_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("05f954565f29b0b6")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("bae28b05c12e963a"),
                description: i18n::t!("f2ae47254b986d30"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("unnecessary_code_fade"),
                    pick: |settings_content| settings_content.theme.unnecessary_code_fade.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.theme.unnecessary_code_fade = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("78958d6e888cebed"),
                description: i18n::t!("32332104b78785a9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("current_line_highlight"),
                    pick: |settings_content| {
                        settings_content.editor.current_line_highlight.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.current_line_highlight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4b0914fc8fb476f3"),
                description: i18n::t!("cdb99a3ed55523f2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("selection_highlight"),
                    pick: |settings_content| settings_content.editor.selection_highlight.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.selection_highlight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("07389c9e4adf42e0"),
                description: i18n::t!("a6027ce7357d023b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("rounded_selection"),
                    pick: |settings_content| settings_content.editor.rounded_selection.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.rounded_selection = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1d4cf8a5ceec0781"),
                description: i18n::t!("f2d37deac60d2b92"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimum_contrast_for_highlights"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimum_contrast_for_highlights
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.minimum_contrast_for_highlights = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn guides_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("b34ce4d70417d1dc")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fac02752ade29996"),
                description: i18n::t!("0628263125af432c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("show_wrap_guides"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .show_wrap_guides
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .show_wrap_guides = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            // todo(settings_ui): This needs a custom component
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("66a6577105c54d61"),
                description: i18n::t!("35efd255d9312f85"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("wrap_guides"),
                        pick: |settings_content| {
                            settings_content
                                .project
                                .all_languages
                                .defaults
                                .wrap_guides
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.all_languages.defaults.wrap_guides = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn indent_guides_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("6ad938c8c789f951")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("dfb802238b38fbd4"),
                description: i18n::t!("88f25dc7f2c7dcb8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("indent_guides.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .as_ref()
                            .and_then(|indent_guides| indent_guides.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5eb9f4e84a63fb27"),
                description: i18n::t!("efab44b0a6db76aa"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("indent_guides.line_width"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .as_ref()
                            .and_then(|indent_guides| indent_guides.line_width.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .get_or_insert_default()
                            .line_width = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("62e66562fc72c865"),
                description: i18n::t!("d987bf8d5c56aa27"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("indent_guides.active_line_width"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .as_ref()
                            .and_then(|indent_guides| indent_guides.active_line_width.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .get_or_insert_default()
                            .active_line_width = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7b6fd1f9a75ed3df"),
                description: i18n::t!("179418cb9bffada7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("indent_guides.coloring"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .as_ref()
                            .and_then(|indent_guides| indent_guides.coloring.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .get_or_insert_default()
                            .coloring = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("98d41705630f0df5"),
                description: i18n::t!("7a2d3d2de3ddfd35"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("indent_guides.background_coloring"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .as_ref()
                            .and_then(|indent_guides| indent_guides.background_coloring.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .indent_guides
                            .get_or_insert_default()
                            .background_coloring = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    let items: Box<[SettingsPageItem]> = concat_sections!(
        theme_section(),
        buffer_font_section(),
        ui_font_section(),
        agent_panel_font_section(),
        markdown_preview_font_section(),
        text_rendering_section(),
        cursor_section(),
        highlighting_section(),
        guides_section(),
        indent_guides_section(),
    );

    SettingsPage {
        title: i18n::t!("86a63f23a076b11e"),
        items,
    }
}

fn keymap_page() -> SettingsPage {
    fn keybindings_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("66239d367b0fb5ce")),
            SettingsPageItem::ActionLink(ActionLink {
                title: i18n::t!("8cd0663cb33f9b9b").into(),
                description: Some(i18n::t!("05a72bbe589a24b4").into()),
                button_text: i18n::t!("f6d844d023d08eac").into(),
                on_click: Arc::new(|settings_window, window, cx| {
                    let Some(original_window) = settings_window.original_window else {
                        return;
                    };
                    original_window
                        .update(cx, |_workspace, original_window, cx| {
                            original_window
                                .dispatch_action(zed_actions::OpenKeymap.boxed_clone(), cx);
                            original_window.activate_window();
                        })
                        .ok();
                    window.remove_window();
                }),
                files: USER,
            }),
        ]
    }

    fn base_keymap_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("36f446e49bc9f495")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("36f446e49bc9f495"),
                description: i18n::t!("c7ca2309c305d388"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("base_keymap"),
                    pick: |settings_content| settings_content.base_keymap.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.base_keymap = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]
    }

    fn modal_editing_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("1033d4bbb19de4d9")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("10043421065bbf14"),
                description: i18n::t!("68f9fe68bfcc38c2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim_mode"),
                    pick: |settings_content| settings_content.vim_mode.as_ref(),
                    write: write_vim_mode,
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d0ee3dbf6281161b"),
                description: i18n::t!("11786261064ea1cf"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("helix_mode"),
                    pick: |settings_content| settings_content.helix_mode.as_ref(),
                    write: write_helix_mode,
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    let items: Box<[SettingsPageItem]> = concat_sections!(
        keybindings_section(),
        base_keymap_section(),
        modal_editing_section(),
    );

    SettingsPage {
        title: i18n::t!("166f65a9ea0b7fa3"),
        items,
    }
}

fn editor_page() -> SettingsPage {
    fn auto_save_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("f2db3712a685913a")),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("d16f0d0db09b2cdf"),
                    description: i18n::t!("31ed5fe16138ac3f"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("autosave$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::AutosaveSetting>()[settings_content
                                    .workspace
                                    .autosave
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                settings_content.workspace.autosave = None;
                                return;
                            };
                            let settings_value = settings_content
                                .workspace
                                .autosave
                                .get_or_insert_with(|| settings::AutosaveSetting::Off);
                            *settings_value = match value {
                                settings::AutosaveSettingDiscriminants::Off => {
                                    settings::AutosaveSetting::Off
                                }
                                settings::AutosaveSettingDiscriminants::AfterDelay => {
                                    let milliseconds = match settings_value {
                                        settings::AutosaveSetting::AfterDelay { milliseconds } => {
                                            *milliseconds
                                        }
                                        _ => settings::DelayMs(1000),
                                    };
                                    settings::AutosaveSetting::AfterDelay { milliseconds }
                                }
                                settings::AutosaveSettingDiscriminants::OnFocusChange => {
                                    settings::AutosaveSetting::OnFocusChange
                                }
                                settings::AutosaveSettingDiscriminants::OnWindowChange => {
                                    settings::AutosaveSetting::OnWindowChange
                                }
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(settings_content.workspace.autosave.as_ref()?.discriminant() as usize)
                },
                fields: dynamic_variants::<settings::AutosaveSetting>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::AutosaveSettingDiscriminants::Off => vec![],
                        settings::AutosaveSettingDiscriminants::AfterDelay => vec![SettingItem {
                            files: USER,
                            title: i18n::t!("3ce4972a8598de6f"),
                            description: i18n::t!("73f9944cb7c1b403"),
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("autosave.after_delay.milliseconds"),
                                pick: |settings_content| match settings_content
                                    .workspace
                                    .autosave
                                    .as_ref()
                                {
                                    Some(settings::AutosaveSetting::AfterDelay {
                                        milliseconds,
                                    }) => Some(milliseconds),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        settings_content.workspace.autosave = None;
                                        return;
                                    };
                                    match settings_content.workspace.autosave.as_mut() {
                                        Some(settings::AutosaveSetting::AfterDelay {
                                            milliseconds,
                                        }) => *milliseconds = value,
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                        settings::AutosaveSettingDiscriminants::OnFocusChange => vec![],
                        settings::AutosaveSettingDiscriminants::OnWindowChange => vec![],
                    })
                    .collect(),
            }),
        ]
    }

    fn which_key_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("47084cf76ab6ec1a")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f26b86f913cca377"),
                description: i18n::t!("22f48abc9e707f6a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("which_key.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .which_key
                            .as_ref()
                            .and_then(|settings| settings.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.which_key.get_or_insert_default().enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4b810df5eb96a054"),
                description: i18n::t!("0328791a156d9a3b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("which_key.delay_ms"),
                    pick: |settings_content| {
                        settings_content
                            .which_key
                            .as_ref()
                            .and_then(|settings| settings.delay_ms.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.which_key.get_or_insert_default().delay_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn multibuffer_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("5903780aba8c0f48")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("44b5e97f3abb3346"),
                description: i18n::t!("7eab52595711b12b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("double_click_in_multibuffer"),
                    pick: |settings_content| {
                        settings_content.editor.double_click_in_multibuffer.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.double_click_in_multibuffer = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3ac51d203b3e1cea"),
                description: i18n::t!("82049cfb813a124a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("expand_excerpt_lines"),
                    pick: |settings_content| settings_content.editor.expand_excerpt_lines.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.expand_excerpt_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("05b36f579a7a1c3d"),
                description: i18n::t!("7c07926653a9d1a7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("excerpt_context_lines"),
                    pick: |settings_content| settings_content.editor.excerpt_context_lines.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.excerpt_context_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8a09d7344f251ab3"),
                description: i18n::t!("3e195af5b1d0e48a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.expand_outlines_with_depth"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()
                            .and_then(|outline_panel| {
                                outline_panel.expand_outlines_with_depth.as_ref()
                            })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .expand_outlines_with_depth = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3f63841125a8f9de"),
                description: i18n::t!("b612a64d35345623"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diff_view_style"),
                    pick: |settings_content| settings_content.editor.diff_view_style.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.diff_view_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("38fec55a06e010fe"),
                description: i18n::t!("9505d8ee20770e29"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimum_split_diff_width"),
                    pick: |settings_content| {
                        settings_content.editor.minimum_split_diff_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.minimum_split_diff_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scrolling_section() -> [SettingsPageItem; 9] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("34dedaffd3cc55f0")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("bdf56e7a45ecac2c"),
                description: i18n::t!("f63d943298d08df7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scroll_beyond_last_line"),
                    pick: |settings_content| {
                        settings_content.editor.scroll_beyond_last_line.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.scroll_beyond_last_line = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("42f357c86ce9aa31"),
                description: i18n::t!("0f218add3762cfe1"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vertical_scroll_margin"),
                    pick: |settings_content| {
                        settings_content.editor.vertical_scroll_margin.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.vertical_scroll_margin = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e71b5db136ac64ac"),
                description: i18n::t!("3b6a2434259da4ac"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("horizontal_scroll_margin"),
                    pick: |settings_content| {
                        settings_content.editor.horizontal_scroll_margin.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.horizontal_scroll_margin = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("765866c4ff34c39d"),
                description: i18n::t!("e9ff030661721b50"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scroll_sensitivity"),
                    pick: |settings_content| settings_content.editor.scroll_sensitivity.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.scroll_sensitivity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1dd8407fe384f0db"),
                description: i18n::t!("64f4432a7228aa33"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("mouse_wheel_zoom"),
                    pick: |settings_content| settings_content.editor.mouse_wheel_zoom.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.mouse_wheel_zoom = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4338420e8def24c6"),
                description: i18n::t!("9c73477e1bd6a58e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("fast_scroll_sensitivity"),
                    pick: |settings_content| {
                        settings_content.editor.fast_scroll_sensitivity.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.fast_scroll_sensitivity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("90116a03aad7b32c"),
                description: i18n::t!("8c6985bf4fffc591"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("autoscroll_on_clicks"),
                    pick: |settings_content| settings_content.editor.autoscroll_on_clicks.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.autoscroll_on_clicks = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4455a54e440b6bb7"),
                description: i18n::t!("4a9c80cf83146405"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("sticky_scroll.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .sticky_scroll
                            .as_ref()
                            .and_then(|sticky_scroll| sticky_scroll.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .sticky_scroll
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn signature_help_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("3e213486d69d019f")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6a9c86479a5fae46"),
                description: i18n::t!("9d18f0d93269edea"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("auto_signature_help"),
                    pick: |settings_content| settings_content.editor.auto_signature_help.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.auto_signature_help = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d96ddd3fd8d80363"),
                description: i18n::t!("568e869d3952235c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("show_signature_help_after_edits"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .show_signature_help_after_edits
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.show_signature_help_after_edits = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3768a9532b2d4500"),
                description: i18n::t!("bffd933009e33378"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("snippet_sort_order"),
                    pick: |settings_content| settings_content.editor.snippet_sort_order.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.snippet_sort_order = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn hover_translation_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("84f41f0b0c41982b")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9ba27619e4a34c53"),
                description: i18n::t!("1f17ceb9ed561cad"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_translation.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .hover_translation
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .hover_translation
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("148475fdd9ea8520"),
                description: i18n::t!("32f62f10d655753a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_translation.provider"),
                    pick: |settings_content| {
                        settings_content
                            .hover_translation
                            .as_ref()?
                            .provider
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .hover_translation
                            .get_or_insert_default()
                            .provider = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1b3ae8feb8b283ea"),
                description: i18n::t!("33ec843199d56ac3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_translation.model"),
                    pick: |settings_content| {
                        settings_content.hover_translation.as_ref()?.model.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .hover_translation
                            .get_or_insert_default()
                            .model = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("966a23e84aedeea5"),
                description: i18n::t!("77e285dcda49b9ae"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_translation.target_language"),
                    pick: |settings_content| {
                        settings_content
                            .hover_translation
                            .as_ref()?
                            .target_language
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .hover_translation
                            .get_or_insert_default()
                            .target_language = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e42cebb3a2521580"),
                description: i18n::t!("46f1de338f4324ea"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_translation.max_chars"),
                    pick: |settings_content| {
                        settings_content
                            .hover_translation
                            .as_ref()?
                            .max_chars
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .hover_translation
                            .get_or_insert_default()
                            .max_chars = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8818e0dd39df695f"),
                description: i18n::t!("6027936434165b0d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_translation.cache_persist"),
                    pick: |settings_content| {
                        settings_content
                            .hover_translation
                            .as_ref()?
                            .cache_persist
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .hover_translation
                            .get_or_insert_default()
                            .cache_persist = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("62b175a0af31aad1"),
                description: i18n::t!("aced275fccbd996d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_translation.cache_max_bytes"),
                    pick: |settings_content| {
                        settings_content
                            .hover_translation
                            .as_ref()?
                            .cache_max_bytes
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .hover_translation
                            .get_or_insert_default()
                            .cache_max_bytes = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn hover_popover_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("27473887e269cc16")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f4f0ead1116b5b62"),
                description: i18n::t!("626ac00970222e6d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_enabled"),
                    pick: |settings_content| settings_content.editor.hover_popover_enabled.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings ui): add units to this number input
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("18045b8c40f135cd"),
                description: i18n::t!("8dd87b135222720d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_delay"),
                    pick: |settings_content| settings_content.editor.hover_popover_delay.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_delay = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ee3ed785cc97494f"),
                description: i18n::t!("43c5f10cf17e430d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_sticky"),
                    pick: |settings_content| settings_content.editor.hover_popover_sticky.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_sticky = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings ui): add units to this number input
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("18716faa275b8a69"),
                description: i18n::t!("0031362a252d3e4a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("hover_popover_hiding_delay"),
                    pick: |settings_content| {
                        settings_content.editor.hover_popover_hiding_delay.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.hover_popover_hiding_delay = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn drag_and_drop_selection_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("6fe152bffbe86018")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f4f0ead1116b5b62"),
                description: i18n::t!("015f590007de190b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("drag_and_drop_selection.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .as_ref()
                            .and_then(|drag_and_drop| drag_and_drop.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("18045b8c40f135cd"),
                description: i18n::t!("7f252b23a98cd6b4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("drag_and_drop_selection.delay"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .as_ref()
                            .and_then(|drag_and_drop| drag_and_drop.delay.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .drag_and_drop_selection
                            .get_or_insert_default()
                            .delay = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn gutter_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("17959d2bc972a09f")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f7cfa333b5d14095"),
                description: i18n::t!("99b6587c88233ad3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.line_numbers"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.line_numbers.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .line_numbers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9e6953efd4f8f098"),
                description: i18n::t!("1580cb2658d30aa6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("relative_line_numbers"),
                    pick: |settings_content| settings_content.editor.relative_line_numbers.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.relative_line_numbers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("988742643affbed6"),
                description: i18n::t!("9ba773654c04123c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.runnables"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.runnables.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .runnables = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8ec39c792d765bed"),
                description: i18n::t!("f3928cb2b4f4e2e7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.breakpoints"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.breakpoints.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .breakpoints = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("140b3a290cc0ed06"),
                description: i18n::t!("ca197bee25b77ca6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.bookmarks"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.bookmarks.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .bookmarks = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("bbb8812ed5cc8237"),
                description: i18n::t!("d1535a9fca6f25a2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.folds"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.folds.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.gutter.get_or_insert_default().folds = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("2102dc89672900df"),
                description: i18n::t!("5415d65f73dedc4a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("gutter.min_line_number_digits"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .gutter
                            .as_ref()
                            .and_then(|gutter| gutter.min_line_number_digits.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .gutter
                            .get_or_insert_default()
                            .min_line_number_digits = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    title: i18n::t!("d6e681d957c42897"),
                    description: i18n::t!("26c416c201064d06"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("gutter.git_gutter_width$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::GitGutterWidth>()[settings_content
                                    .editor
                                    .gutter
                                    .as_ref()?
                                    .git_gutter_width
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let gutter = settings_content.editor.gutter.get_or_insert_default();
                            gutter.git_gutter_width = value.map(|value| match value {
                                settings::GitGutterWidthDiscriminants::Default => {
                                    settings::GitGutterWidth::Default
                                }
                                settings::GitGutterWidthDiscriminants::Custom => {
                                    let width = match gutter.git_gutter_width {
                                        Some(settings::GitGutterWidth::Custom(width)) => {
                                            settings::PixelSetting(*width)
                                        }
                                        _ => settings::PixelSetting(3.0),
                                    };
                                    settings::GitGutterWidth::Custom(width)
                                }
                            });
                        },
                    }),
                    metadata: None,
                    files: USER,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .editor
                            .gutter
                            .as_ref()?
                            .git_gutter_width
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::GitGutterWidth>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::GitGutterWidthDiscriminants::Default => vec![],
                        settings::GitGutterWidthDiscriminants::Custom => vec![SettingItem {
                            files: USER,
                            title: i18n::t!("07616794f0ad7344"),
                            description: i18n::t!("2eef9c6cf29c2437"),
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("gutter.git_gutter_width"),
                                pick: |settings_content| match settings_content
                                    .editor
                                    .gutter
                                    .as_ref()
                                    .and_then(|gutter| gutter.git_gutter_width.as_ref())
                                {
                                    Some(settings::GitGutterWidth::Custom(value)) => Some(value),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        return;
                                    };
                                    if let Some(settings::GitGutterWidth::Custom(width)) =
                                        settings_content
                                            .editor
                                            .gutter
                                            .as_mut()
                                            .and_then(|gutter| gutter.git_gutter_width.as_mut())
                                    {
                                        *width = value;
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                    })
                    .collect(),
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4201c13fbaaf1f85"),
                description: i18n::t!("2305f749a9c9b088"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("inline_code_actions"),
                    pick: |settings_content| settings_content.editor.inline_code_actions.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.inline_code_actions = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scrollbar_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("53bcc015611bb7fa")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4e1449e7d5e50593"),
                description: i18n::t!("7e8f2977ba35d157"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar"),
                    pick: |settings_content| {
                        settings_content.editor.scrollbar.as_ref()?.show.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("47a75ca0b8fef37c"),
                description: i18n::t!("0b6abe53d3cf5e60"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.cursors"),
                    pick: |settings_content| {
                        settings_content.editor.scrollbar.as_ref()?.cursors.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .cursors = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6a556c4bf3b83cf8"),
                description: i18n::t!("90ab70ce8d6cf58f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.git_diff"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .git_diff
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .git_diff = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("88d72ece7cf76737"),
                description: i18n::t!("c2cdcdb63659ffb4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.search_results"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .search_results
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .search_results = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a12b558baecbdaf1"),
                description: i18n::t!("abd8a64a96537351"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.selected_text"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .selected_text
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .selected_text = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("261d3ebb49628b68"),
                description: i18n::t!("057291e45c93ed9b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.selected_symbol"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .selected_symbol
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .selected_symbol = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("40ff6300f9817deb"),
                description: i18n::t!("efe494fdfc63d189"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.diagnostics"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .diagnostics
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d1d1b9b2a5211a53"),
                description: i18n::t!("cc3ab673d56b9c6b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.axes.horizontal"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .axes
                            .as_ref()?
                            .horizontal
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .axes
                            .get_or_insert_default()
                            .horizontal = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("20f6657acf74d947"),
                description: i18n::t!("75b10fc45555439a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("scrollbar.axes.vertical"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .scrollbar
                            .as_ref()?
                            .axes
                            .as_ref()?
                            .vertical
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .scrollbar
                            .get_or_insert_default()
                            .axes
                            .get_or_insert_default()
                            .vertical = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn minimap_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("a623478771b1a95b")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4e1449e7d5e50593"),
                description: i18n::t!("108588f49a7845af"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.show"),
                    pick: |settings_content| {
                        settings_content.editor.minimap.as_ref()?.show.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.minimap.get_or_insert_default().show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5ac9489c03e4e9f9"),
                description: i18n::t!("a5ec68711d179605"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.display_in"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()?
                            .display_in
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .display_in = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9348e939e4965a5d"),
                description: i18n::t!("f60900ba226b6c14"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.thumb"),
                    pick: |settings_content| {
                        settings_content.editor.minimap.as_ref()?.thumb.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .thumb = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("eb95b663e64122bc"),
                description: i18n::t!("7be4b2914954b0ea"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.thumb_border"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()?
                            .thumb_border
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .thumb_border = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("78958d6e888cebed"),
                description: i18n::t!("40be6f0b2d899a33"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.current_line_highlight"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()
                            .and_then(|minimap| minimap.current_line_highlight.as_ref())
                            .or(settings_content.editor.current_line_highlight.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .current_line_highlight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("77c74c892a209547"),
                description: i18n::t!("5f95d9a717cd1fc3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("minimap.max_width_columns"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .minimap
                            .as_ref()?
                            .max_width_columns
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .minimap
                            .get_or_insert_default()
                            .max_width_columns = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn toolbar_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("3166d8af51f15eb6")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6c3f7b6a12a97468"),
                description: i18n::t!("1a0a8e89fb85c2dd"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.breadcrumbs"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .breadcrumbs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .breadcrumbs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("2cf085b4eb79248a"),
                description: i18n::t!("4a9160e69aba2741"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.quick_actions"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .quick_actions
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .quick_actions = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8d74a69aba011325"),
                description: i18n::t!("9593120c17917448"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.selections_menu"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .selections_menu
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .selections_menu = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6bdf9057c4e8df71"),
                description: i18n::t!("cd0c48254fd226c9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.agent_review"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .agent_review
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .agent_review = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("567c0d21fcd613b5"),
                description: i18n::t!("8aad09c242a336bf"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("toolbar.code_actions"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .toolbar
                            .as_ref()?
                            .code_actions
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .toolbar
                            .get_or_insert_default()
                            .code_actions = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn vim_settings_section() -> [SettingsPageItem; 14] {
        [
            SettingsPageItem::SectionHeader("Vim"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ca0e02c309a977df"),
                description: i18n::t!("c7ae0c3ba33aaf51"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.default_mode"),
                    pick: |settings_content| settings_content.vim.as_ref()?.default_mode.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.vim.get_or_insert_default().default_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("469e6d30e96519ea"),
                description: i18n::t!("ad1e613da68ae482"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.toggle_relative_line_numbers"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .toggle_relative_line_numbers
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .toggle_relative_line_numbers = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("54241968a990dbaf"),
                description: i18n::t!("000c1a4b303623f6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.use_system_clipboard"),
                    pick: |settings_content| {
                        settings_content.vim.as_ref()?.use_system_clipboard.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .use_system_clipboard = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b048ed457a88bdbc"),
                description: i18n::t!("ad4edb44a04a703e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.use_smartcase_find"),
                    pick: |settings_content| {
                        settings_content.vim.as_ref()?.use_smartcase_find.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .use_smartcase_find = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6dc3e367c159af87"),
                description: i18n::t!("f25b0592e4d11dd8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.gdefault"),
                    pick: |settings_content| settings_content.vim.as_ref()?.gdefault.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.vim.get_or_insert_default().gdefault = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3f953f0f3414ce68"),
                description: i18n::t!("f9f5a9209bbed027"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.highlight_on_yank_duration"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .highlight_on_yank_duration
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .highlight_on_yank_duration = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1b7b160b02769846"),
                description: i18n::t!("43a5517b903bbb1f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.use_regex_search"),
                    pick: |settings_content| {
                        settings_content.vim.as_ref()?.use_regex_search.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .use_regex_search = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5abf25647be21e73"),
                description: i18n::t!("9533a149c969acd0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.show_edit_predictions_in_normal_mode"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .show_edit_predictions_in_normal_mode
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .show_edit_predictions_in_normal_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b794013a0ce6dfc2"),
                description: i18n::t!("71242a773af93165"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.normal"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .normal
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .normal = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7e9a7f0f048ea120"),
                description: i18n::t!("3c2aa19adf2e5255"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.insert"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .insert
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .insert = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a57c953ab965c06c"),
                description: i18n::t!("d34b107ed872e031"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.replace"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .replace
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .replace = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f0ed1ecb4616439e"),
                description: i18n::t!("aa8ef27f70cdfa65"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("vim.cursor_shape.visual"),
                    pick: |settings_content| {
                        settings_content
                            .vim
                            .as_ref()?
                            .cursor_shape
                            .as_ref()?
                            .visual
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .vim
                            .get_or_insert_default()
                            .cursor_shape
                            .get_or_insert_default()
                            .visual = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a110228e6a567c6c"),
                description: i18n::t!("57e78bb05b1cb2ff"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("vim.custom_digraphs"),
                        pick: |settings_content| {
                            settings_content.vim.as_ref()?.custom_digraphs.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.vim.get_or_insert_default().custom_digraphs = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    let items = concat_sections!(
        auto_save_section(),
        which_key_section(),
        multibuffer_section(),
        scrolling_section(),
        signature_help_section(),
        hover_popover_section(),
        hover_translation_section(),
        drag_and_drop_selection_section(),
        gutter_section(),
        scrollbar_section(),
        minimap_section(),
        toolbar_section(),
        vim_settings_section(),
        language_settings_data(),
    );

    SettingsPage {
        title: i18n::t!("3b7f5965bdbfee34"),
        items: items,
    }
}

fn languages_and_tools_page(cx: &App) -> SettingsPage {
    fn file_types_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("9a8457b3dc844478")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e94c16837e3ef00c"),
                description: i18n::t!("3ada78fa15c834d4"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("file_type_associations"),
                        pick: |settings_content| {
                            settings_content.project.all_languages.file_types.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.all_languages.file_types = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn diagnostics_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("40ff6300f9817deb")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("52cf66734153babd"),
                description: i18n::t!("740f4a182f929bdb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics_max_severity"),
                    pick: |settings_content| {
                        settings_content.editor.diagnostics_max_severity.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.diagnostics_max_severity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9e4cbe5f6cb51559"),
                description: i18n::t!("83d1c2facd844f92"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.include_warnings"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .include_warnings
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .include_warnings = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn inline_diagnostics_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("874ee510372ec389")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f4f0ead1116b5b62"),
                description: i18n::t!("ad796846f8752204"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a60a7904f66371d2"),
                description: i18n::t!("50d4db21cbcc2aec"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.update_debounce_ms"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .update_debounce_ms
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .update_debounce_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c2dc4da52ed35127"),
                description: i18n::t!("ab162ef09644cb2a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.padding"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .padding = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("10f88762da79a5db"),
                description: i18n::t!("0755192799d225a3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.inline.min_column"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .inline
                            .as_ref()?
                            .min_column
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .inline
                            .get_or_insert_default()
                            .min_column = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn lsp_pull_diagnostics_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("LSP Pull Diagnostics"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f4f0ead1116b5b62"),
                description: i18n::t!("a0b27328788dc9b0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.lsp_pull_diagnostics.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .lsp_pull_diagnostics
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .lsp_pull_diagnostics
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings_ui): Needs unit
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1e9f8da0f725c8e8"),
                description: i18n::t!("476baef0753e7de0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.lsp_pull_diagnostics.debounce_ms"),
                    pick: |settings_content| {
                        settings_content
                            .diagnostics
                            .as_ref()?
                            .lsp_pull_diagnostics
                            .as_ref()?
                            .debounce_ms
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .diagnostics
                            .get_or_insert_default()
                            .lsp_pull_diagnostics
                            .get_or_insert_default()
                            .debounce_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn lsp_highlights_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("LSP Highlights"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1e9f8da0f725c8e8"),
                description: i18n::t!("b6840af071a8b24f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("lsp_highlight_debounce"),
                    pick: |settings_content| {
                        settings_content.editor.lsp_highlight_debounce.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.lsp_highlight_debounce = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn languages_list_section(cx: &App) -> Box<[SettingsPageItem]> {
        // todo(settings_ui): Refresh on extension (un)/installed
        // Note that `crates/json_schema_store` solves the same problem, there is probably a way to unify the two
        std::iter::once(SettingsPageItem::SectionHeader(i18n::t!(
            "9f6fee1aba17a565"
        )))
        .chain(all_language_names(cx).into_iter().map(|language_name| {
            let link = format!("languages.{language_name}");
            SettingsPageItem::SubPageLink(SubPageLink {
                title: language_name,
                r#type: crate::SubPageType::Language,
                description: None,
                search_aliases: &[],
                json_path: Some(link.leak()),
                in_json: true,
                files: USER | PROJECT,
                render: |this, scroll_handle, window, cx| {
                    let items: Box<[SettingsPageItem]> = concat_sections!(
                        language_settings_data(),
                        non_editor_language_settings_data(),
                        edit_prediction_language_settings_section()
                    );
                    this.render_sub_page_items(items.iter().enumerate(), scroll_handle, window, cx)
                        .into_any_element()
                },
            })
        }))
        .collect()
    }

    SettingsPage {
        title: i18n::t!("c22e51a826c237d4"),
        items: {
            concat_sections!(
                non_editor_language_settings_data(),
                file_types_section(),
                diagnostics_section(),
                inline_diagnostics_section(),
                lsp_pull_diagnostics_section(),
                lsp_highlights_section(),
                languages_list_section(cx),
            )
        },
    }
}

fn search_and_files_page() -> SettingsPage {
    fn search_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader("Search"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7fff7e76a48a8a43"),
                description: i18n::t!("3a92d3d476d73751"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.whole_word"),
                    pick: |settings_content| {
                        settings_content.editor.search.as_ref()?.whole_word.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .whole_word = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8c7e3447ec67023e"),
                description: i18n::t!("81002dba1918879a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.case_sensitive"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .search
                            .as_ref()?
                            .case_sensitive
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .case_sensitive = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("23c9b65c53286e16"),
                description: i18n::t!("e09c6c672c6dce97"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_smartcase_search"),
                    pick: |settings_content| settings_content.editor.use_smartcase_search.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.use_smartcase_search = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6beb9d61f866fa56"),
                description: i18n::t!("9bb3857eae5d2b1d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.include_ignored"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .search
                            .as_ref()?
                            .include_ignored
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .include_ignored = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("af33e62808ca6837"),
                description: i18n::t!("7727b66f3996e17a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.regex"),
                    pick: |settings_content| {
                        settings_content.editor.search.as_ref()?.regex.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.search.get_or_insert_default().regex = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0980a4f0243faa47"),
                description: i18n::t!("edab5dfd459e859c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search_wrap"),
                    pick: |settings_content| settings_content.editor.search_wrap.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.search_wrap = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d59ebc3ac3464687"),
                description: i18n::t!("d0e39a7ad9a63e78"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.search.center_on_match"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .search
                            .as_ref()
                            .and_then(|search| search.center_on_match.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .center_on_match = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c102d09dfbb304e3"),
                description: i18n::t!("9547b158901d0dfa"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.search.search_on_type"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .search
                            .as_ref()
                            .and_then(|search| search.search_on_type.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .search_on_type = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("33a6ca9eeb0c380e"),
                description: i18n::t!("c743b387f155efb1"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("seed_search_query_from_cursor"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .seed_search_query_from_cursor
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.seed_search_query_from_cursor = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn command_palette_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader("Command Palette"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d166cc48b6030749"),
                description: i18n::t!("71f82b1ef2124fb0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("command_palette.use_command_history"),
                    pick: |settings_content| {
                        settings_content
                            .command_palette
                            .as_ref()?
                            .use_command_history
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .command_palette
                            .get_or_insert_default()
                            .use_command_history = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn file_finder_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("6d2716c20338d09a")),
            // todo: null by default
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3c1ad49d05df20fa"),
                description: i18n::t!("7dfc27b214eba22e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_finder.include_ignored"),
                    pick: |settings_content| {
                        settings_content
                            .file_finder
                            .as_ref()?
                            .include_ignored
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .file_finder
                            .get_or_insert_default()
                            .include_ignored = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f78d0dfd6e2b782c"),
                description: i18n::t!("cc9b7122f281a503"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_finder.file_icons"),
                    pick: |settings_content| {
                        settings_content.file_finder.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .file_finder
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f60a32c25631d135"),
                description: i18n::t!("be754a8289c11207"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_finder.skip_focus_for_active_in_search"),
                    pick: |settings_content| {
                        settings_content
                            .file_finder
                            .as_ref()?
                            .skip_focus_for_active_in_search
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .file_finder
                            .get_or_insert_default()
                            .skip_focus_for_active_in_search = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn file_scan_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("bed24303d8574311")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ff570eb57b21d2ba"),
                description: i18n::t!("26134494eacbf430"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("file_scan_exclusions"),
                        pick: |settings_content| {
                            settings_content
                                .project
                                .worktree
                                .file_scan_exclusions
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.file_scan_exclusions = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3221426d2e3862db"),
                description: i18n::t!("2d19f1dcf2304b64"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("file_scan_inclusions"),
                        pick: |settings_content| {
                            settings_content
                                .project
                                .worktree
                                .file_scan_inclusions
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.file_scan_inclusions = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("24b496e200dfb02d"),
                description: i18n::t!("c71504a08d04de3e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("file_scan_depth"),
                    pick: |settings_content| {
                        settings_content.project.worktree.file_scan_depth.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.project.worktree.file_scan_depth = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("63a4d42187dddfcc"),
                description: i18n::t!("b3846057b956fbc0"),
                field: Box::new(SettingField {
                    json_path: Some("scan_symlinks"),
                    organization_override: None,
                    pick: |settings_content| {
                        settings_content.project.worktree.scan_symlinks.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.project.worktree.scan_symlinks = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ee6da3d1dce0f7ef"),
                description: i18n::t!("1c8c51ab48500baf"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("restore_on_file_reopen"),
                    pick: |settings_content| {
                        settings_content.workspace.restore_on_file_reopen.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.restore_on_file_reopen = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("75940ec05e8617bc"),
                description: i18n::t!("bc8c98dba09b0865"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("close_on_file_delete"),
                    pick: |settings_content| {
                        settings_content.workspace.close_on_file_delete.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.close_on_file_delete = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("03d27c8d16837132"),
        items: concat_sections![
            search_section(),
            command_palette_section(),
            file_finder_section(),
            file_scan_section(),
        ],
    }
}

fn window_and_layout_page() -> SettingsPage {
    fn status_bar_section() -> [SettingsPageItem; 12] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("c8592da567d69005")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1235e45b8400283c"),
                description: i18n::t!("85d8c0c3fdf52ad6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.button"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e48a7a6cc0c0647b"),
                description: i18n::t!("25e152ca00f17737"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.active_language_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .active_language_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .active_language_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1b80f4823fa1b6c3"),
                description: i18n::t!("6af14e1ebb3391ff"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.active_encoding_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .active_encoding_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .active_encoding_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8e1f63841c5d87a6"),
                description: i18n::t!("8c4571d8652ab733"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.cursor_position_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .cursor_position_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .cursor_position_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6960e6fbd4298e2d"),
                description: i18n::t!("7913d4a41d8a41e7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.line_endings_button"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .line_endings_button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .line_endings_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("2bf1bcb1c21d3169"),
                description: i18n::t!("d0ca8c54581839c7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.pending_keystrokes_indicator"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .pending_keystrokes_indicator
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .pending_keystrokes_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8c7f4de7be084cee"),
                description: i18n::t!("d4f097c7d51ed616"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.button"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("04670c44935898c4"),
                description: i18n::t!("4cdac2886a4bbbfb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("diagnostics.button"),
                    pick: |settings_content| settings_content.diagnostics.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.diagnostics.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e53231864c745408"),
                description: i18n::t!("5d2afbf84c51dde2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("search.button"),
                    pick: |settings_content| {
                        settings_content.editor.search.as_ref()?.button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .editor
                            .search
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("62ab43dfc6943206"),
                description: i18n::t!("4df77b026d91f9ec"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.button"),
                    pick: |settings_content| settings_content.debugger.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.debugger.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d9cdce4a7dd52954"),
                description: i18n::t!("5f7e42392f7b7849"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("status_bar.show_active_file"),
                    pick: |settings_content| {
                        settings_content
                            .status_bar
                            .as_ref()?
                            .show_active_file
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .status_bar
                            .get_or_insert_default()
                            .show_active_file = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn title_bar_section() -> [SettingsPageItem; 12] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("c3ebe56c4633ef87")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("29409cc4faf25751"),
                description: i18n::t!("315c790e7fbe87ef"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_branch_status_icon"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_branch_status_icon
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_branch_status_icon = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b8c9f5e3118eaf80"),
                description: i18n::t!("49e339d984aa6b6d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_branch_name"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_branch_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_branch_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("24cceeaa0ee73a9a"),
                description: i18n::t!("7eacc5d40cd6a1ad"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_worktree_name"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_worktree_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_worktree_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5710a6cfde9ee7c4"),
                description: i18n::t!("60d2834a093a7f7a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_project_items"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_project_items
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_project_items = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("010c44f6c6c10320"),
                description: i18n::t!("bb76cb38c621852f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_onboarding_banner"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_onboarding_banner
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_onboarding_banner = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0713b4380997a283"),
                description: i18n::t!("4344c4e4f18ad775"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_sign_in"),
                    pick: |settings_content| {
                        settings_content.title_bar.as_ref()?.show_sign_in.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_sign_in = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("426c5c81529899e6"),
                description: i18n::t!("6cf1cf0052c85266"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_user_menu"),
                    pick: |settings_content| {
                        settings_content.title_bar.as_ref()?.show_user_menu.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_user_menu = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ca91bf43d61c966f"),
                description: i18n::t!("7d1b70cf70b22fc1"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_user_picture"),
                    pick: |settings_content| {
                        settings_content
                            .title_bar
                            .as_ref()?
                            .show_user_picture
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_user_picture = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b2734abb1a173cdf"),
                description: i18n::t!("535fb088140ecdd1"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.show_menus"),
                    pick: |settings_content| {
                        settings_content.title_bar.as_ref()?.show_menus.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .show_menus = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("6d0bed22c93a94a0"),
                    description: i18n::t!("975490bbef5929de"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("title_bar.button_layout$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::WindowButtonLayoutContent>()
                                    [settings_content
                                        .title_bar
                                        .as_ref()?
                                        .button_layout
                                        .as_ref()?
                                        .discriminant()
                                        as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                settings_content
                                    .title_bar
                                    .get_or_insert_default()
                                    .button_layout = None;
                                return;
                            };

                            let current_custom_layout = settings_content
                                .title_bar
                                .as_ref()
                                .and_then(|title_bar| title_bar.button_layout.as_ref())
                                .and_then(|button_layout| match button_layout {
                                    settings::WindowButtonLayoutContent::Custom(layout) => {
                                        Some(layout.clone())
                                    }
                                    _ => None,
                                });

                            let button_layout = match value {
                                settings::WindowButtonLayoutContentDiscriminants::PlatformDefault => {
                                    settings::WindowButtonLayoutContent::PlatformDefault
                                }
                                settings::WindowButtonLayoutContentDiscriminants::Standard => {
                                    settings::WindowButtonLayoutContent::Standard
                                }
                                settings::WindowButtonLayoutContentDiscriminants::Custom => {
                                    settings::WindowButtonLayoutContent::Custom(
                                        current_custom_layout.unwrap_or_else(|| {
                                            "close:minimize,maximize".to_string()
                                        }),
                                    )
                                }
                            };

                            settings_content
                                .title_bar
                                .get_or_insert_default()
                                .button_layout = Some(button_layout);
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .title_bar
                            .as_ref()?
                            .button_layout
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::WindowButtonLayoutContent>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::WindowButtonLayoutContentDiscriminants::PlatformDefault => {
                            vec![]
                        }
                        settings::WindowButtonLayoutContentDiscriminants::Standard => vec![],
                        settings::WindowButtonLayoutContentDiscriminants::Custom => {
                            vec![SettingItem {
                                files: USER,
                                title: i18n::t!("39f14bd3657ede8b"),
                                description: i18n::t!("c1cf3b34837b4047"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("title_bar.button_layout"),
                                    pick: |settings_content| match settings_content
                                        .title_bar
                                        .as_ref()?
                                        .button_layout
                                        .as_ref()?
                                    {
                                        settings::WindowButtonLayoutContent::Custom(layout) => {
                                            Some(layout)
                                        }
                                        _ => DEFAULT_EMPTY_STRING,
                                    },
                                    write: |settings_content, value, _| {
                                        settings_content
                                            .title_bar
                                            .get_or_insert_default()
                                            .button_layout =
                                            value.map(settings::WindowButtonLayoutContent::Custom);
                                    },
                                }),
                                metadata: Some(Box::new(SettingsFieldMetadata {
                                    placeholder: Some("close:minimize,maximize"),
                                    ..Default::default()
                                })),
                            }]
                        }
                    })
                    .collect(),
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("749397d74a23065e"),
                description: i18n::t!("664832180567d9bb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("title_bar.open_menus_on_hover"),
                    pick: |settings_content| {
                        settings_content.title_bar.as_ref()?.open_menus_on_hover.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .title_bar
                            .get_or_insert_default()
                            .open_menus_on_hover = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn tab_bar_section() -> [SettingsPageItem; 9] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("c62deb22f9b2f98c")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ba4571eb66942a71"),
                description: i18n::t!("7065b0f0fd9097d8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show"),
                    pick: |settings_content| settings_content.tab_bar.as_ref()?.show.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.tab_bar.get_or_insert_default().show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("005a50765b8c067b"),
                description: i18n::t!("daf5b97050c6a911"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.git_status"),
                    pick: |settings_content| settings_content.tabs.as_ref()?.git_status.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.tabs.get_or_insert_default().git_status = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ebdc862811f0ec0b"),
                description: i18n::t!("b42b257184b631e8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.file_icons"),
                    pick: |settings_content| settings_content.tabs.as_ref()?.file_icons.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.tabs.get_or_insert_default().file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7892cad3b7219d60"),
                description: i18n::t!("a24e4ee3e6a50c00"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.close_position"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.close_position.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.tabs.get_or_insert_default().close_position = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("b15430bf745852eb"),
                description: i18n::t!("c051a4747c0cfebf"),
                // todo(settings_ui): The default for this value is null and it's use in code
                // is complex, so I'm going to come back to this later
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("max_tabs"),
                        pick: |settings_content| settings_content.workspace.max_tabs.as_ref(),
                        write: |settings_content, value, _| {
                            settings_content.workspace.max_tabs = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c7353e6fa4d7c820"),
                description: i18n::t!("1777ba7728aa71c3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show_nav_history_buttons"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()?
                            .show_nav_history_buttons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .show_nav_history_buttons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("72744d02fee052d2"),
                description: i18n::t!("9ab2c3ae4576d500"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show_tab_bar_buttons"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()?
                            .show_tab_bar_buttons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .show_tab_bar_buttons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("03263dd9fed32aef"),
                description: i18n::t!("66db8914fb43a80c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tab_bar.show_pinned_tabs_in_separate_row"),
                    pick: |settings_content| {
                        settings_content
                            .tab_bar
                            .as_ref()?
                            .show_pinned_tabs_in_separate_row
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tab_bar
                            .get_or_insert_default()
                            .show_pinned_tabs_in_separate_row = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn tab_settings_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("9f7697216e24a014")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("945517953290058d"),
                description: i18n::t!("bfbb827d22bc9231"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.activate_on_close"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.activate_on_close.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabs
                            .get_or_insert_default()
                            .activate_on_close = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("25676f42dbc07103"),
                description: i18n::t!("e71f4bf766e84f1d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.show_diagnostics"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.show_diagnostics.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabs
                            .get_or_insert_default()
                            .show_diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("72404ccd86bfa2a5"),
                description: i18n::t!("a6370b2b9ba53d6e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("tabs.show_close_button"),
                    pick: |settings_content| {
                        settings_content.tabs.as_ref()?.show_close_button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .tabs
                            .get_or_insert_default()
                            .show_close_button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn preview_tabs_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("b1781a3be7f2e6ca")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1c2a5d250d4f8950"),
                description: i18n::t!("468e9f2601e00ca3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enabled"),
                    pick: |settings_content| {
                        settings_content.preview_tabs.as_ref()?.enabled.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("2fe2c1eb195ec986"),
                description: i18n::t!("fc845d1226d611b2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_from_project_panel"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_from_project_panel
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_from_project_panel = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a0691f6762a91637"),
                description: i18n::t!("d4ff5987a3315c1f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_from_file_finder"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_from_file_finder
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_from_file_finder = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4e4cf190b0f6ab93"),
                description: i18n::t!("2344a2c163e0adb8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_from_multibuffer"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_from_multibuffer
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_from_multibuffer = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e9f8381850388a7b"),
                description: i18n::t!("0c937836c3d02b86"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_multibuffer_from_code_navigation"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_multibuffer_from_code_navigation
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_multibuffer_from_code_navigation = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("99517f653adb629d"),
                description: i18n::t!("8c3165d231880afe"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_preview_file_from_code_navigation"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_preview_file_from_code_navigation
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_preview_file_from_code_navigation = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ce515128b40891c4"),
                description: i18n::t!("0926f891e8455a29"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("preview_tabs.enable_keep_preview_on_code_navigation"),
                    pick: |settings_content| {
                        settings_content
                            .preview_tabs
                            .as_ref()?
                            .enable_keep_preview_on_code_navigation
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .preview_tabs
                            .get_or_insert_default()
                            .enable_keep_preview_on_code_navigation = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn layout_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("Layout"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3be54f50e25210b0"),
                description: i18n::t!("f513363043560320"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("bottom_dock_layout"),
                    pick: |settings_content| settings_content.workspace.bottom_dock_layout.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.bottom_dock_layout = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("e2c2a98aaa0e33e0"),
                description: i18n::t!("4a28da9778662f06"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("centered_layout.left_padding"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .centered_layout
                            .as_ref()?
                            .left_padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .centered_layout
                            .get_or_insert_default()
                            .left_padding = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("e3f301540fd43f6e"),
                description: i18n::t!("4fcc831ef24cfbd4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("centered_layout.right_padding"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .centered_layout
                            .as_ref()?
                            .right_padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .centered_layout
                            .get_or_insert_default()
                            .right_padding = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("45c0f0d76caf72ff"),
                description: i18n::t!("3525c6d3801896dc"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("focus_follows_mouse.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .as_ref()
                            .and_then(|s| s.enabled.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f10060f3d1fbb056"),
                description: i18n::t!("71f44b176ce7f28f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("focus_follows_mouse.debounce_ms"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .as_ref()
                            .and_then(|s| s.debounce_ms.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .focus_follows_mouse
                            .get_or_insert_default()
                            .debounce_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn window_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("Window"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("aa0122a68568f559"),
                description: i18n::t!("46e36dd4c6833888"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("window_title_format"),
                    pick: |settings_content| {
                        settings_content.workspace.window_title_format.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.window_title_format =
                            value.filter(|format| !format.is_empty());
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("${projectName}${separator}${fileName}"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("548c3830e51260e8"),
                description: i18n::t!("597f4d7c440fc8f7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("window_title_separator"),
                    pick: |settings_content| {
                        settings_content.workspace.window_title_separator.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.window_title_separator = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some(" — "),
                    ..Default::default()
                })),
                files: USER,
            }),
            // todo(settings_ui): Should we filter by platform.as_ref()?
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9d8dfec636f8af98"),
                description: i18n::t!("50e7d3cd0669dd9f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("use_system_window_tabs"),
                    pick: |settings_content| {
                        settings_content.workspace.use_system_window_tabs.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.use_system_window_tabs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7d2f05b34a712f7b"),
                description: i18n::t!("202b6ccad3157053"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("fullscreen_mode"),
                    pick: |settings_content| settings_content.workspace.fullscreen_mode.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.fullscreen_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("005de36a9f46a273"),
                description: i18n::t!("a54e5179dc17399d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("window_decorations"),
                    pick: |settings_content| settings_content.workspace.window_decorations.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.window_decorations = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn pane_modifiers_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Pane Modifiers"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("49f3986174d46768"),
                description: i18n::t!("a47efb51e8ce41f0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("active_pane_modifiers.inactive_opacity"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .as_ref()?
                            .inactive_opacity
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .get_or_insert_default()
                            .inactive_opacity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("17d2634a018f7f1b"),
                description: i18n::t!("1dfd6b607d303f9f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("active_pane_modifiers.border_size"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .as_ref()?
                            .border_size
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .workspace
                            .active_pane_modifiers
                            .get_or_insert_default()
                            .border_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("028babcff3c57600"),
                description: i18n::t!("b23235145f221155"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("zoomed_padding"),
                    pick: |settings_content| settings_content.workspace.zoomed_padding.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.zoomed_padding = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e6edfe9a1e112b0d"),
                description: i18n::t!("ed25880c7cfa22ef"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("close_panel_on_toggle"),
                    pick: |settings_content| {
                        settings_content.workspace.close_panel_on_toggle.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.close_panel_on_toggle = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn pane_split_direction_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader("Pane Split Direction"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("07fc08c582d57129"),
                description: i18n::t!("142484dabffe051c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("pane_split_direction_vertical"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .pane_split_direction_vertical
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.pane_split_direction_vertical = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("05ee953c510bc4b6"),
                description: i18n::t!("0ed9330e4b3f8c6d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("pane_split_direction_horizontal"),
                    pick: |settings_content| {
                        settings_content
                            .workspace
                            .pane_split_direction_horizontal
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.workspace.pane_split_direction_horizontal = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("5940f6c7ed446c6c"),
        items: concat_sections![
            status_bar_section(),
            title_bar_section(),
            tab_bar_section(),
            tab_settings_section(),
            preview_tabs_section(),
            layout_section(),
            window_section(),
            pane_modifiers_section(),
            pane_split_direction_section(),
        ],
    }
}

fn panels_page() -> SettingsPage {
    fn project_panel_section() -> [SettingsPageItem; 30] {
        [
            SettingsPageItem::SectionHeader("Project Panel"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("63ce3040afaddfe1"),
                description: i18n::t!("be7cba8d7690124d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.dock"),
                    pick: |settings_content| settings_content.project_panel.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.project_panel.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9244fd0a960ceeef"),
                description: i18n::t!("ea5af0512489598b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.default_width"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .default_width
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    title: i18n::t!("6d4885d84bc954d3"),
                    description: i18n::t!("fca63e9b0b25f2be"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("project_panel.title_tooltip_delay$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::ProjectPanelTitleTooltipDelay>()
                                    [settings_content
                                        .project_panel
                                        .as_ref()?
                                        .title_tooltip_delay
                                        .as_ref()?
                                        .discriminant()
                                        as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let project_panel =
                                settings_content.project_panel.get_or_insert_default();
                            project_panel.title_tooltip_delay = value.map(|value| match value {
                                settings::ProjectPanelTitleTooltipDelayDiscriminants::Default => {
                                    settings::ProjectPanelTitleTooltipDelay::Default
                                }
                                settings::ProjectPanelTitleTooltipDelayDiscriminants::Disabled => {
                                    settings::ProjectPanelTitleTooltipDelay::Disabled
                                }
                                settings::ProjectPanelTitleTooltipDelayDiscriminants::Custom => {
                                    let delay = match project_panel.title_tooltip_delay {
                                        Some(settings::ProjectPanelTitleTooltipDelay::Custom(
                                            delay,
                                        )) => settings::DelayMs(delay.0),
                                        _ => settings::DelayMs(1500),
                                    };
                                    settings::ProjectPanelTitleTooltipDelay::Custom(delay)
                                }
                            });
                        },
                    }),
                    metadata: None,
                    files: USER,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .project_panel
                            .as_ref()?
                            .title_tooltip_delay
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::ProjectPanelTitleTooltipDelay>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::ProjectPanelTitleTooltipDelayDiscriminants::Default => vec![],
                        settings::ProjectPanelTitleTooltipDelayDiscriminants::Disabled => vec![],
                        settings::ProjectPanelTitleTooltipDelayDiscriminants::Custom => {
                            vec![SettingItem {
                                files: USER,
                                title: i18n::t!("33c2c3d68ba7e4a5"),
                                description: i18n::t!("31f958c56ce25eb8"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("project_panel.title_tooltip_delay"),
                                    pick: |settings_content| match settings_content
                                        .project_panel
                                        .as_ref()
                                        .and_then(|project_panel| {
                                            project_panel.title_tooltip_delay.as_ref()
                                        }) {
                                        Some(settings::ProjectPanelTitleTooltipDelay::Custom(
                                            value,
                                        )) => Some(value),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        if let Some(
                                            settings::ProjectPanelTitleTooltipDelay::Custom(width),
                                        ) = settings_content.project_panel.as_mut().and_then(
                                            |project_panel| {
                                                project_panel.title_tooltip_delay.as_mut()
                                            },
                                        ) {
                                            *width = value;
                                        }
                                    },
                                }),
                                metadata: None,
                            }]
                        }
                    })
                    .collect(),
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("99ff55e1d0d81a90"),
                description: i18n::t!("366d0c6199434366"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.hide_gitignore"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .hide_gitignore
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .hide_gitignore = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3e320caedd44f106"),
                description: i18n::t!("fb393293c06f755f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.entry_spacing"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .entry_spacing
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .entry_spacing = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f78d0dfd6e2b782c"),
                description: i18n::t!("c002c1269900f846"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.file_icons"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fb360e3288f59da9"),
                description: i18n::t!("d232aba7d6279218"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.folder_indicator"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .folder_indicator
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .folder_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("54030c68e840e87d"),
                description: i18n::t!("d5ca2daf51425d0c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.git_status"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.git_status.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .git_status = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5ad91c4760bcade2"),
                description: i18n::t!("f645294f8e082773"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.indent_size"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .indent_size
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .indent_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("56e61454db9a0c8c"),
                description: i18n::t!("45ace7bb0f932a59"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_reveal_entries"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_reveal_entries
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_reveal_entries = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5e0500491910a5b9"),
                description: i18n::t!("8ca7c953ba2e1a32"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.starts_open"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .starts_open
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c50dcc2d5c4bb179"),
                description: i18n::t!("b245fd2dc036d084"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_fold_dirs"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_fold_dirs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_fold_dirs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5a350cdeb5c54bd4"),
                description: i18n::t!("8b876aa8fe2342f6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.bold_folder_labels"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .bold_folder_labels
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .bold_folder_labels = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c647d06905390229"),
                description: i18n::t!("de929dfa4f137d44"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.scrollbar.show"),
                    pick: |settings_content| {
                        show_scrollbar_or_editor(settings_content, |settings_content| {
                            settings_content
                                .project_panel
                                .as_ref()?
                                .scrollbar
                                .as_ref()?
                                .show
                                .as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d8e6506f4650c614"),
                description: i18n::t!("dc3ca466a3c0b57b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.scrollbar.horizontal_scroll"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .scrollbar
                            .as_ref()?
                            .horizontal_scroll
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .horizontal_scroll = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("36bca67cb267c768"),
                description: i18n::t!("2bde22e7ea9f98bf"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.show_diagnostics"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .show_diagnostics
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .show_diagnostics = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("bdf863e18d4f1efa"),
                description: i18n::t!("0399266c17412e57"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.diagnostic_badges"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .diagnostic_badges
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .diagnostic_badges = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c045d9b62c44c695"),
                description: i18n::t!("a29ab27a1ea82493"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.git_status_indicator"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .git_status_indicator
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .git_status_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4455a54e440b6bb7"),
                description: i18n::t!("b414b441b1f032fa"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.sticky_scroll"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .sticky_scroll
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .sticky_scroll = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("46f58252bbc80887"),
                description: i18n::t!("44a51aa30f64a2a2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.indent_guides.show"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .indent_guides
                            .as_ref()?
                            .show
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .indent_guides
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1cf6dead9cf5f062"),
                description: i18n::t!("8d1664b1c4d71ace"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.drag_and_drop"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .drag_and_drop
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .drag_and_drop = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ad7ff46e4a5d37e4"),
                description: i18n::t!("cb64fec7fd708ec7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.hide_root"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.hide_root.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .hide_root = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c4eca2d409677f54"),
                description: i18n::t!("5c7aafbcb627a843"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.hide_hidden"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .hide_hidden
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .hide_hidden = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3c1f0a5d3f436693"),
                description: i18n::t!("cfb4ae7a99393e94"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.sort_mode"),
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.sort_mode.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .sort_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a4ab9e7aadd8e3f1"),
                description: i18n::t!("6508435f4a6349bc"),
                field: Box::new(SettingField {
                    organization_override: None,
                    pick: |settings_content| {
                        settings_content.project_panel.as_ref()?.sort_order.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .sort_order = value;
                    },
                    json_path: Some("project_panel.sort_order"),
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0d2ffb71dcad2867"),
                description: i18n::t!("f97a02cd34348aec"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_open.on_create"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_open
                            .as_ref()?
                            .on_create
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_open
                            .get_or_insert_default()
                            .on_create = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a88fd39b7668b177"),
                description: i18n::t!("5aeb9865027da0ca"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_open.on_paste"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_open
                            .as_ref()?
                            .on_paste
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_open
                            .get_or_insert_default()
                            .on_paste = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("599169572965101d"),
                description: i18n::t!("a219144683701419"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("project_panel.auto_open.on_drop"),
                    pick: |settings_content| {
                        settings_content
                            .project_panel
                            .as_ref()?
                            .auto_open
                            .as_ref()?
                            .on_drop
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project_panel
                            .get_or_insert_default()
                            .auto_open
                            .get_or_insert_default()
                            .on_drop = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("10f6c8db8a48e1da"),
                description: i18n::t!("2fceeaa27c6d2f83"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("hidden_files"),
                        pick: |settings_content| {
                            settings_content.project.worktree.hidden_files.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content.project.worktree.hidden_files = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn terminal_panel_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Terminal Panel"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("74e7958da58c458a"),
                description: i18n::t!("8efc9a18df123a4c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.dock"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5e0500491910a5b9"),
                description: i18n::t!("086db6874f5df527"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.starts_open"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.starts_open.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fd019327f574df1e"),
                description: i18n::t!("0bc367ff5446314b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.flexible"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.flexible.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().flexible = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("aad5920997259f70"),
                description: i18n::t!("454f93b4a344bfce"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.show_count_badge"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .show_count_badge
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .show_count_badge = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn outline_panel_section() -> [SettingsPageItem; 12] {
        [
            SettingsPageItem::SectionHeader("Outline Panel"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("91f66898fc193df4"),
                description: i18n::t!("5db7d02f6ebb72b1"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.button"),
                    pick: |settings_content| {
                        settings_content.outline_panel.as_ref()?.button.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9dc2c72bdc49b005"),
                description: i18n::t!("1033b81e3f601e47"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.dock"),
                    pick: |settings_content| settings_content.outline_panel.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.outline_panel.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9106b73f268d304f"),
                description: i18n::t!("81d8a921c3a6f8e9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.default_width"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .default_width
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f78d0dfd6e2b782c"),
                description: i18n::t!("d26938ed8db7fb38"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.file_icons"),
                    pick: |settings_content| {
                        settings_content.outline_panel.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fb360e3288f59da9"),
                description: i18n::t!("41850017763e939d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.folder_indicator"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .folder_indicator
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .folder_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("54030c68e840e87d"),
                description: i18n::t!("26a4282f39316720"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.git_status"),
                    pick: |settings_content| {
                        settings_content.outline_panel.as_ref()?.git_status.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .git_status = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5ad91c4760bcade2"),
                description: i18n::t!("f645294f8e082773"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.indent_size"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .indent_size
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .indent_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("56e61454db9a0c8c"),
                description: i18n::t!("cc8b513037450947"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.auto_reveal_entries"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .auto_reveal_entries
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .auto_reveal_entries = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c50dcc2d5c4bb179"),
                description: i18n::t!("bf6a2e5ffd6dbae7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.auto_fold_dirs"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .auto_fold_dirs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .auto_fold_dirs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                files: USER,
                title: i18n::t!("46f58252bbc80887"),
                description: i18n::t!("9ead645588bf8db6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.indent_guides.show"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .indent_guides
                            .as_ref()?
                            .show
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .indent_guides
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b34f5fc1b520746b"),
                description: i18n::t!("9ec655ec499ba59a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("outline_panel.multi_buffer_hide_symbols"),
                    pick: |settings_content| {
                        settings_content
                            .outline_panel
                            .as_ref()?
                            .multi_buffer_hide_symbols
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .outline_panel
                            .get_or_insert_default()
                            .multi_buffer_hide_symbols = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn git_panel_section() -> [SettingsPageItem; 18] {
        [
            SettingsPageItem::SectionHeader("Git Panel"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b7146d8b96befc16"),
                description: i18n::t!("df2a2022a7625151"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.button"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("522916e808778031"),
                description: i18n::t!("6836da87c818b8c8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.dock"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5e0500491910a5b9"),
                description: i18n::t!("8922a5e142449269"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.starts_open"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.starts_open.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .starts_open = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ad5cf22cd20e03cd"),
                description: i18n::t!("8225941aecbd4d6e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.default_width"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.default_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("11cb320353141dac"),
                description: i18n::t!("07859d05329367c2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.status_style"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.status_style.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .status_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("379d53bc64cfede1"),
                description: i18n::t!("77a711c33b2fd396"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.fallback_branch_name"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .fallback_branch_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .fallback_branch_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1ce6b1d7c95ce2ee"),
                description: i18n::t!("1e65f71321b35885"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.sort_by"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.sort_by.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().sort_by = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("72148c2201764726"),
                description: i18n::t!("629bfdc2013fd82b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.group_by"),
                    pick: |settings_content| settings_content.git_panel.as_ref()?.group_by.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().group_by = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("cb230abecb75e659"),
                description: i18n::t!("fdf261a7d3a65450"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.collapse_untracked_diff"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .collapse_untracked_diff
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .collapse_untracked_diff = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("64f1f87721d5d160"),
                description: i18n::t!("e8d4a7dc8ff8c255"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.tree_view"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.tree_view.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.git_panel.get_or_insert_default().tree_view = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f78d0dfd6e2b782c"),
                description: i18n::t!("23cb492dd5525756"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.file_icons"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.file_icons.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .file_icons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fb360e3288f59da9"),
                description: i18n::t!("5c56c09026010e35"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.folder_indicator"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .folder_indicator
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .folder_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8966b3ba4a5ecea7"),
                description: i18n::t!("f4115a7d484bf52c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.diff_stats"),
                    pick: |settings_content| {
                        settings_content.git_panel.as_ref()?.diff_stats.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .diff_stats = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e259b1a21335a96d"),
                description: i18n::t!("fb743eed213e40d3"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.entry_primary_click_action"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .entry_primary_click_action
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .entry_primary_click_action = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5d20c06ff82bbc4c"),
                description: i18n::t!("8b2f538111c2929a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.show_count_badge"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .show_count_badge
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .show_count_badge = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("248b851da508dbb6"),
                description: i18n::t!("57738efd9d4f064a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.commit_title_max_length"),
                    pick: |settings_content| {
                        settings_content
                            .git_panel
                            .as_ref()?
                            .commit_title_max_length
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .commit_title_max_length = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("53bcc015611bb7fa"),
                description: i18n::t!("8ab140f77a34dbfa"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git_panel.scrollbar.show"),
                    pick: |settings_content| {
                        show_scrollbar_or_editor(settings_content, |settings_content| {
                            settings_content
                                .git_panel
                                .as_ref()?
                                .scrollbar
                                .as_ref()?
                                .show
                                .as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git_panel
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn debugger_panel_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("66c849d89d539e9e")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("765be2fd92c66af9"),
                description: i18n::t!("4455a5b23567c5a4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.dock"),
                    pick: |settings_content| settings_content.debugger.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.debugger.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn collaboration_panel_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("538fd707c82e7e9b")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fefcce3784f47e72"),
                description: i18n::t!("49710fcc320ca5d6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("collaboration_panel.button"),
                    pick: |settings_content| {
                        settings_content
                            .collaboration_panel
                            .as_ref()?
                            .button
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .collaboration_panel
                            .get_or_insert_default()
                            .button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("186d472198e5f531"),
                description: i18n::t!("0e68bcdfaca9c7b5"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("collaboration_panel.dock"),
                    pick: |settings_content| {
                        settings_content.collaboration_panel.as_ref()?.dock.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .collaboration_panel
                            .get_or_insert_default()
                            .dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("97e23d68713e3a21"),
                description: i18n::t!("807b613f5e5e9145"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("collaboration_panel.dock"),
                    pick: |settings_content| {
                        settings_content
                            .collaboration_panel
                            .as_ref()?
                            .default_width
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .collaboration_panel
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn agent_panel_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("3bb0698e654c0693")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1ee4f777a32227c4"),
                description: i18n::t!("db7a6982a63b3e53"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.button"),
                    pick: |settings_content| settings_content.agent.as_ref()?.button.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().button = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("aa82728fc30d680f"),
                description: i18n::t!("bf94af22003a927e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.dock"),
                    pick: |settings_content| settings_content.agent.as_ref()?.dock.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().dock = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("2df257a79e2dbb40"),
                description: i18n::t!("35e7185def0da785"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.flexible"),
                    pick: |settings_content| settings_content.agent.as_ref()?.flexible.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().flexible = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("99de6b0502429398"),
                description: i18n::t!("23ed39607f6a4ef4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.default_width"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.default_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.agent.get_or_insert_default().default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("54c749e086ce046f"),
                description: i18n::t!("68fb222eb91d2723"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.default_height"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.default_height.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .default_height = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("a56775aeae3f1d3b"),
                    description: i18n::t!("0e46e48786fbf355"),
                    field: Box::new(SettingField::<bool> {
                        organization_override: None,
                        json_path: Some("agent.limit_content_width"),
                        pick: |settings_content| {
                            settings_content
                                .agent
                                .as_ref()?
                                .limit_content_width
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .agent
                                .get_or_insert_default()
                                .limit_content_width = value;
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    let enabled = settings_content
                        .agent
                        .as_ref()?
                        .limit_content_width
                        .unwrap_or(true);
                    Some(if enabled { 1 } else { 0 })
                },
                fields: vec![
                    vec![],
                    vec![SettingItem {
                        files: USER,
                        title: i18n::t!("f6fb5b8116113851"),
                        description: i18n::t!("a338a4121e13f4ee"),
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("agent.max_content_width"),
                            pick: |settings_content| {
                                settings_content.agent.as_ref()?.max_content_width.as_ref()
                            },
                            write: |settings_content, value, _| {
                                settings_content
                                    .agent
                                    .get_or_insert_default()
                                    .max_content_width = value;
                            },
                        }),
                        metadata: None,
                    }],
                ],
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("3c8c5939d685319a"),
        items: concat_sections![
            project_panel_section(),
            terminal_panel_section(),
            outline_panel_section(),
            git_panel_section(),
            debugger_panel_section(),
            collaboration_panel_section(),
            agent_panel_section(),
        ],
    }
}

fn debugger_page() -> SettingsPage {
    fn general_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("40fae00b7c6d8ac0")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7ecf48280a19f20b"),
                description: i18n::t!("db54cf415942cd0b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.stepping_granularity"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .stepping_granularity
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .stepping_granularity = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0bec6de8fbcd5bc6"),
                description: i18n::t!("ed80dae8bc3a5109"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.save_breakpoints"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .save_breakpoints
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .save_breakpoints = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e512cf016f960728"),
                description: i18n::t!("c31d73dd415fc283"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.timeout"),
                    pick: |settings_content| settings_content.debugger.as_ref()?.timeout.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.debugger.get_or_insert_default().timeout = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("22218a0cfa089715"),
                description: i18n::t!("53085bfc4e45e95f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.log_dap_communications"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .log_dap_communications
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .log_dap_communications = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f3d44df700bce24e"),
                description: i18n::t!("5997a772f9947dfb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("debugger.format_dap_log_messages"),
                    pick: |settings_content| {
                        settings_content
                            .debugger
                            .as_ref()?
                            .format_dap_log_messages
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .debugger
                            .get_or_insert_default()
                            .format_dap_log_messages = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("20fe3f3e72ca01b8"),
        items: concat_sections![general_section()],
    }
}

fn terminal_page() -> SettingsPage {
    fn environment_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("904dd029c768820d")),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER | PROJECT,
                    title: "Shell",
                    description: i18n::t!("3599a6b9724ffd11"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("terminal.shell$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::Shell>()[settings_content
                                    .terminal
                                    .as_ref()?
                                    .project
                                    .shell
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                if let Some(terminal) = settings_content.terminal.as_mut() {
                                    terminal.project.shell = None;
                                }
                                return;
                            };
                            let settings_value = settings_content
                                .terminal
                                .get_or_insert_default()
                                .project
                                .shell
                                .get_or_insert_with(|| settings::Shell::default());
                            let default_shell = if cfg!(target_os = "windows") {
                                "powershell.exe"
                            } else {
                                "sh"
                            };
                            *settings_value = match value {
                                settings::ShellDiscriminants::System => settings::Shell::System,
                                settings::ShellDiscriminants::Program => {
                                    let program = match settings_value {
                                        settings::Shell::Program(program) => program.clone(),
                                        settings::Shell::WithArguments { program, .. } => {
                                            program.clone()
                                        }
                                        _ => String::from(default_shell),
                                    };
                                    settings::Shell::Program(program)
                                }
                                settings::ShellDiscriminants::WithArguments => {
                                    let (program, args, title_override) = match settings_value {
                                        settings::Shell::Program(program) => {
                                            (program.clone(), vec![], None)
                                        }
                                        settings::Shell::WithArguments {
                                            program,
                                            args,
                                            title_override,
                                        } => {
                                            (program.clone(), args.clone(), title_override.clone())
                                        }
                                        _ => (String::from(default_shell), vec![], None),
                                    };
                                    settings::Shell::WithArguments {
                                        program,
                                        args,
                                        title_override,
                                    }
                                }
                            };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .terminal
                            .as_ref()?
                            .project
                            .shell
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::Shell>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::ShellDiscriminants::System => vec![],
                        settings::ShellDiscriminants::Program => vec![SettingItem {
                            files: USER | PROJECT,
                            title: i18n::t!("5d942dbe52a46039"),
                            description: i18n::t!("3a61a2b4358ea645"),
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("terminal.shell"),
                                pick: |settings_content| match settings_content
                                    .terminal
                                    .as_ref()?
                                    .project
                                    .shell
                                    .as_ref()
                                {
                                    Some(settings::Shell::Program(program)) => Some(program),
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let Some(value) = value else {
                                        return;
                                    };
                                    match settings_content
                                        .terminal
                                        .get_or_insert_default()
                                        .project
                                        .shell
                                        .as_mut()
                                    {
                                        Some(settings::Shell::Program(program)) => *program = value,
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                        settings::ShellDiscriminants::WithArguments => vec![
                            SettingItem {
                                files: USER | PROJECT,
                                title: i18n::t!("5d942dbe52a46039"),
                                description: i18n::t!("2db41c8e0c9e7988"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("terminal.shell.program"),
                                    pick: |settings_content| match settings_content
                                        .terminal
                                        .as_ref()?
                                        .project
                                        .shell
                                        .as_ref()
                                    {
                                        Some(settings::Shell::WithArguments {
                                            program, ..
                                        }) => Some(program),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| {
                                        let Some(value) = value else {
                                            return;
                                        };
                                        match settings_content
                                            .terminal
                                            .get_or_insert_default()
                                            .project
                                            .shell
                                            .as_mut()
                                        {
                                            Some(settings::Shell::WithArguments {
                                                program,
                                                ..
                                            }) => *program = value,
                                            _ => return,
                                        }
                                    },
                                }),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER | PROJECT,
                                title: i18n::t!("9634fb0832be624f"),
                                description: i18n::t!("55b325b4e3795806"),
                                field: Box::new(
                                    SettingField {
                                        organization_override: None,
                                        json_path: Some("terminal.shell.args"),
                                        pick: |settings_content| match settings_content
                                            .terminal
                                            .as_ref()?
                                            .project
                                            .shell
                                            .as_ref()
                                        {
                                            Some(settings::Shell::WithArguments {
                                                args, ..
                                            }) => Some(args),
                                            _ => None,
                                        },
                                        write: |settings_content, value, _| {
                                            let Some(value) = value else {
                                                return;
                                            };
                                            match settings_content
                                                .terminal
                                                .get_or_insert_default()
                                                .project
                                                .shell
                                                .as_mut()
                                            {
                                                Some(settings::Shell::WithArguments {
                                                    args,
                                                    ..
                                                }) => *args = value,
                                                _ => return,
                                            }
                                        },
                                    }
                                    .unimplemented(),
                                ),
                                metadata: None,
                            },
                            SettingItem {
                                files: USER | PROJECT,
                                title: i18n::t!("725e9f0fe15ef49c"),
                                description: i18n::t!("4eab0b9a4f9a1edb"),
                                field: Box::new(SettingField {
                                    organization_override: None,
                                    json_path: Some("terminal.shell.title_override"),
                                    pick: |settings_content| match settings_content
                                        .terminal
                                        .as_ref()?
                                        .project
                                        .shell
                                        .as_ref()
                                    {
                                        Some(settings::Shell::WithArguments {
                                            title_override,
                                            ..
                                        }) => title_override.as_ref().or(DEFAULT_EMPTY_STRING),
                                        _ => None,
                                    },
                                    write: |settings_content, value, _| match settings_content
                                        .terminal
                                        .get_or_insert_default()
                                        .project
                                        .shell
                                        .as_mut()
                                    {
                                        Some(settings::Shell::WithArguments {
                                            title_override,
                                            ..
                                        }) => *title_override = value.filter(|s| !s.is_empty()),
                                        _ => return,
                                    },
                                }),
                                metadata: None,
                            },
                        ],
                    })
                    .collect(),
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER | PROJECT,
                    title: i18n::t!("3db7b06b5f6de0e0"),
                    description: i18n::t!("8aaf537636a72b64"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("terminal.working_directory$"),
                        pick: |settings_content| {
                            Some(
                                &dynamic_variants::<settings::WorkingDirectory>()[settings_content
                                    .terminal
                                    .as_ref()?
                                    .project
                                    .working_directory
                                    .as_ref()?
                                    .discriminant()
                                    as usize],
                            )
                        },
                        write: |settings_content, value, _| {
                            let Some(value) = value else {
                                if let Some(terminal) = settings_content.terminal.as_mut() {
                                    terminal.project.working_directory = None;
                                }
                                return;
                            };
                            let settings_value = settings_content
                                .terminal
                                .get_or_insert_default()
                                .project
                                .working_directory
                                .get_or_insert_with(|| {
                                    settings::WorkingDirectory::CurrentProjectDirectory
                                });
                            *settings_value = match value {
                                    settings::WorkingDirectoryDiscriminants::CurrentFileDirectory => {
                                        settings::WorkingDirectory::CurrentFileDirectory
                                    },
                                    settings::WorkingDirectoryDiscriminants::CurrentProjectDirectory => {
                                        settings::WorkingDirectory::CurrentProjectDirectory
                                    }
                                    settings::WorkingDirectoryDiscriminants::FirstProjectDirectory => {
                                        settings::WorkingDirectory::FirstProjectDirectory
                                    }
                                    settings::WorkingDirectoryDiscriminants::AlwaysHome => {
                                        settings::WorkingDirectory::AlwaysHome
                                    }
                                    settings::WorkingDirectoryDiscriminants::Always => {
                                        let directory = match settings_value {
                                            settings::WorkingDirectory::Always { .. } => return,
                                            _ => String::new(),
                                        };
                                        settings::WorkingDirectory::Always { directory }
                                    }
                                };
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        settings_content
                            .terminal
                            .as_ref()?
                            .project
                            .working_directory
                            .as_ref()?
                            .discriminant() as usize,
                    )
                },
                fields: dynamic_variants::<settings::WorkingDirectory>()
                    .into_iter()
                    .map(|variant| match variant {
                        settings::WorkingDirectoryDiscriminants::CurrentFileDirectory => vec![],
                        settings::WorkingDirectoryDiscriminants::CurrentProjectDirectory => vec![],
                        settings::WorkingDirectoryDiscriminants::FirstProjectDirectory => vec![],
                        settings::WorkingDirectoryDiscriminants::AlwaysHome => vec![],
                        settings::WorkingDirectoryDiscriminants::Always => vec![SettingItem {
                            files: USER | PROJECT,
                            title: i18n::t!("52daa71ebc310581"),
                            description: i18n::t!("0cb9d3da5aa05987"),
                            field: Box::new(SettingField {
                                organization_override: None,
                                json_path: Some("terminal.working_directory.always"),
                                pick: |settings_content| match settings_content
                                    .terminal
                                    .as_ref()?
                                    .project
                                    .working_directory
                                    .as_ref()
                                {
                                    Some(settings::WorkingDirectory::Always { directory }) => {
                                        Some(directory)
                                    }
                                    _ => None,
                                },
                                write: |settings_content, value, _| {
                                    let value = value.unwrap_or_default();
                                    match settings_content
                                        .terminal
                                        .get_or_insert_default()
                                        .project
                                        .working_directory
                                        .as_mut()
                                    {
                                        Some(settings::WorkingDirectory::Always { directory }) => {
                                            *directory = value
                                        }
                                        _ => return,
                                    }
                                },
                            }),
                            metadata: None,
                        }],
                    })
                    .collect(),
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ae27b474ea4d6ee6"),
                description: i18n::t!("b71e9c60d83925ac"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.env"),
                        pick: |settings_content| {
                            settings_content.terminal.as_ref()?.project.env.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .project
                                .env = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("06c1aca06a1a9c16"),
                description: i18n::t!("f0bbe114c1db38e0"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.detect_venv"),
                        pick: |settings_content| {
                            settings_content
                                .terminal
                                .as_ref()?
                                .project
                                .detect_venv
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .project
                                .detect_venv = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn font_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("Font"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0c30c37c6ead953b"),
                description: i18n::t!("2ef749159c9477c1"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.font_size"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|terminal| terminal.font_size.as_ref())
                            .or(settings_content.theme.buffer_font_size.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().font_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("078838da4218490b"),
                description: i18n::t!("ef4960f417a6aef7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.font_family"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()
                            .and_then(|terminal| terminal.font_family.as_ref())
                            .or(settings_content.theme.buffer_font_family.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .font_family = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e037c3476fcbc213"),
                description: i18n::t!("c80d7075ff6299cd"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.font_fallbacks"),
                        pick: |settings_content| {
                            settings_content
                                .terminal
                                .as_ref()
                                .and_then(|terminal| terminal.font_fallbacks.as_ref())
                                .or(settings_content.theme.buffer_font_fallbacks.as_ref())
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .font_fallbacks = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("db0c79d9d7d6c577"),
                description: i18n::t!("c9b5d2c5547ae26d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.font_weight"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.font_weight.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .font_weight = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("cf2674acf2bbad54"),
                description: i18n::t!("63c434526f4662b0"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.font_features"),
                        pick: |settings_content| {
                            settings_content
                                .terminal
                                .as_ref()
                                .and_then(|terminal| terminal.font_features.as_ref())
                                .or(settings_content.theme.buffer_font_features.as_ref())
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .font_features = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn display_settings_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader("Display Settings"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6b44b7ba432abf47"),
                description: i18n::t!("1648db4664860e2d"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("terminal.line_height"),
                        pick: |settings_content| {
                            settings_content.terminal.as_ref()?.line_height.as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .terminal
                                .get_or_insert_default()
                                .line_height = value;
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("98ec5a07a6ee6050"),
                description: i18n::t!("6263f867863ca78f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.cursor_shape"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.cursor_shape.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .cursor_shape = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("9342b40e1c885cee"),
                description: i18n::t!("456ec0e3f9555db9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.blinking"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.blinking.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().blinking = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7083b782cefe7a44"),
                description: i18n::t!("15e5f96ba2225dec"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.alternate_scroll"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .alternate_scroll
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .alternate_scroll = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4d880ba1b2d6976c"),
                description: i18n::t!("d1f168ee46f4af26"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.minimum_contrast"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .minimum_contrast
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .minimum_contrast = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn behavior_settings_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("865b5c594bca761b")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("77c9b917a55647e8"),
                description: i18n::t!("96fd7000e41a2ab0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.option_as_meta"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.option_as_meta.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .option_as_meta = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("082f4459ba955599"),
                description: i18n::t!("e67f8c328cbe6ed5"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.copy_on_select"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.copy_on_select.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .copy_on_select = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a7d13e4290ceda8a"),
                description: i18n::t!("5579f7ed989b9b21"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.keep_selection_on_copy"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .keep_selection_on_copy
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .keep_selection_on_copy = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("cb93cdcdc0b4124b"),
                description: i18n::t!("7c64bd1a094d391f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.open_links_in_mouse_mode"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .open_links_in_mouse_mode
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .open_links_in_mouse_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fd1eae3f0f49ff87"),
                description: i18n::t!("52bff65cee92e46c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.bell"),
                    pick: |settings_content| settings_content.terminal.as_ref()?.bell.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.terminal.get_or_insert_default().bell = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn layout_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("c27e95dbdcae100d")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5d9918272ebc486a"),
                description: i18n::t!("922f474db6ca149c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.default_width"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.default_width.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .default_width = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("3bd02fe3c0363409"),
                description: i18n::t!("614f418b6c3a1e9a"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.default_height"),
                    pick: |settings_content| {
                        settings_content.terminal.as_ref()?.default_height.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .default_height = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn advanced_settings_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("44455611b9108b91")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5a2766516b0c64ee"),
                description: i18n::t!("01351f7658584678"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.max_scroll_history_lines"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .max_scroll_history_lines
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .max_scroll_history_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8f04460e874acb5e"),
                description: i18n::t!("28771ac11b886532"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.scroll_multiplier"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .scroll_multiplier
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .scroll_multiplier = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn toolbar_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("3166d8af51f15eb6")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6c3f7b6a12a97468"),
                description: i18n::t!("f441673b0faef4ec"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.toolbar.breadcrumbs"),
                    pick: |settings_content| {
                        settings_content
                            .terminal
                            .as_ref()?
                            .toolbar
                            .as_ref()?
                            .breadcrumbs
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .toolbar
                            .get_or_insert_default()
                            .breadcrumbs = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn scrollbar_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("53bcc015611bb7fa")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c647d06905390229"),
                description: i18n::t!("5d69e690433ba672"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("terminal.scrollbar.show"),
                    pick: |settings_content| {
                        show_scrollbar_or_editor(settings_content, |settings_content| {
                            settings_content
                                .terminal
                                .as_ref()?
                                .scrollbar
                                .as_ref()?
                                .show
                                .as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .terminal
                            .get_or_insert_default()
                            .scrollbar
                            .get_or_insert_default()
                            .show = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("e2a76ef1f12e147f"),
        items: concat_sections![
            environment_section(),
            font_section(),
            display_settings_section(),
            behavior_settings_section(),
            layout_settings_section(),
            advanced_settings_section(),
            toolbar_section(),
            scrollbar_section(),
        ],
    }
}

fn version_control_page() -> SettingsPage {
    fn git_integration_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("53191ac96b572350")),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("6b116198324968d2"),
                    description: i18n::t!("feb1e5889907f8f0"),
                    field: Box::new(SettingField::<bool> {
                        organization_override: None,
                        json_path: Some("git.disable_git"),
                        pick: |settings_content| {
                            settings_content
                                .git
                                .as_ref()?
                                .enabled
                                .as_ref()?
                                .disable_git
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .git
                                .get_or_insert_default()
                                .enabled
                                .get_or_insert_default()
                                .disable_git = value;
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    let disabled = settings_content
                        .git
                        .as_ref()?
                        .enabled
                        .as_ref()?
                        .disable_git
                        .unwrap_or(false);
                    Some(if disabled { 0 } else { 1 })
                },
                fields: vec![
                    vec![],
                    vec![
                        SettingItem {
                            files: USER,
                            title: i18n::t!("a50f2d9ab0491514"),
                            description: i18n::t!("217ba1f1299a6685"),
                            field: Box::new(SettingField::<bool> {
                                organization_override: None,
                                json_path: Some("git.enable_status"),
                                pick: |settings_content| {
                                    settings_content
                                        .git
                                        .as_ref()?
                                        .enabled
                                        .as_ref()?
                                        .enable_status
                                        .as_ref()
                                },
                                write: |settings_content, value, _| {
                                    settings_content
                                        .git
                                        .get_or_insert_default()
                                        .enabled
                                        .get_or_insert_default()
                                        .enable_status = value;
                                },
                            }),
                            metadata: None,
                        },
                        SettingItem {
                            files: USER,
                            title: i18n::t!("6059874f6c760f94"),
                            description: i18n::t!("f6eb32b31f2ee2a2"),
                            field: Box::new(SettingField::<bool> {
                                organization_override: None,
                                json_path: Some("git.enable_diff"),
                                pick: |settings_content| {
                                    settings_content
                                        .git
                                        .as_ref()?
                                        .enabled
                                        .as_ref()?
                                        .enable_diff
                                        .as_ref()
                                },
                                write: |settings_content, value, _| {
                                    settings_content
                                        .git
                                        .get_or_insert_default()
                                        .enabled
                                        .get_or_insert_default()
                                        .enable_diff = value;
                                },
                            }),
                            metadata: None,
                        },
                    ],
                ],
            }),
        ]
    }

    fn git_gutter_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("3984a1d67996fbf8")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7e228d4688a260d3"),
                description: i18n::t!("eed5e57a734c9b34"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.git_gutter"),
                    pick: |settings_content| settings_content.git.as_ref()?.git_gutter.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().git_gutter = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            // todo(settings_ui): Figure out the right default for this value in default.json
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4c4ed8a01ec725eb"),
                description: i18n::t!("d7848676aa85ee2f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.gutter_debounce"),
                    pick: |settings_content| {
                        settings_content.git.as_ref()?.gutter_debounce.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().gutter_debounce = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn inline_git_blame_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("31bf21d98197fb54")),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    title: i18n::t!("f4f0ead1116b5b62"),
                    description: i18n::t!("2030e4c9853da8f9"),
                    field: Box::new(SettingField {
                        organization_override: None,
                        json_path: Some("git.inline_blame.enabled"),
                        pick: |settings_content| {
                            settings_content
                                .git
                                .as_ref()?
                                .inline_blame
                                .as_ref()?
                                .enabled
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .git
                                .get_or_insert_default()
                                .inline_blame
                                .get_or_insert_default()
                                .enabled = value;
                        },
                    }),
                    metadata: None,
                    files: USER,
                },
                pick_discriminant: |settings_content| {
                    Some(
                        *settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .enabled
                            .as_ref()? as usize,
                    )
                },
                fields: vec![
                    vec![],
                    vec![SettingItem {
                        title: i18n::t!("1fb4d574da92f1c1"),
                        description: i18n::t!("486348116120664f"),
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("git.inline_blame.location"),
                            pick: |settings_content| {
                                settings_content
                                    .git
                                    .as_ref()?
                                    .inline_blame
                                    .as_ref()?
                                    .location
                                    .as_ref()
                            },
                            write: |settings_content, value, _| {
                                settings_content
                                    .git
                                    .get_or_insert_default()
                                    .inline_blame
                                    .get_or_insert_default()
                                    .location = value;
                            },
                        }),
                        metadata: None,
                        files: USER,
                    }],
                ],
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("18045b8c40f135cd"),
                description: i18n::t!("71d970a9d5aeb9d6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.delay_ms"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .delay_ms
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .delay_ms = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c2dc4da52ed35127"),
                description: i18n::t!("96c537fc1762c357"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.padding"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .padding
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .padding = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("dce0114c58004cf4"),
                description: i18n::t!("a28adc541ec7f198"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.min_column"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .min_column
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .min_column = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("afc7a9c4d858fc42"),
                description: i18n::t!("9848898abbe8b4e5"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.inline_blame.show_commit_summary"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .inline_blame
                            .as_ref()?
                            .show_commit_summary
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .inline_blame
                            .get_or_insert_default()
                            .show_commit_summary = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn git_blame_view_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("db4c94b7360c48bb")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("02a53df5f4003a89"),
                description: i18n::t!("a9fc4fd93ec04f63"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.blame.show_avatar"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .blame
                            .as_ref()?
                            .show_avatar
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .blame
                            .get_or_insert_default()
                            .show_avatar = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn branch_picker_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("c8dd9a8ccc750a2c")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("94eb0eb869e5c0ac"),
                description: i18n::t!("1c532c24fea5fbb8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.branch_picker.show_author_name"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .branch_picker
                            .as_ref()?
                            .show_author_name
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .branch_picker
                            .get_or_insert_default()
                            .show_author_name = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn git_hunks_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("fb48af6f4dbc580d")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8f82ddc193d62cac"),
                description: i18n::t!("f0c563dbe81999c0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.hunk_style"),
                    pick: |settings_content| settings_content.git.as_ref()?.hunk_style.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().hunk_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4d49cbee517ccf9b"),
                description: i18n::t!("ac546edaf7f55b98"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.diff_base"),
                    pick: |settings_content| settings_content.git.as_ref()?.diff_base.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().diff_base = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1148ddeaf12dc80d"),
                description: i18n::t!("6a2c48aa52d4eea0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.path_style"),
                    pick: |settings_content| settings_content.git.as_ref()?.path_style.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.git.get_or_insert_default().path_style = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ae66b438017feae4"),
                description: i18n::t!("3561987887316114"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.show_stage_restore_buttons"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .show_stage_restore_buttons
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .show_stage_restore_buttons = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn file_diff_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("8764e343564f13ef")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("62b34050692a4c7d"),
                description: i18n::t!("be7c7ae7b1f367c9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("git.file_diff.show_full_file"),
                    pick: |settings_content| {
                        settings_content
                            .git
                            .as_ref()?
                            .file_diff
                            .as_ref()?
                            .show_full_file
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .git
                            .get_or_insert_default()
                            .file_diff
                            .get_or_insert_default()
                            .show_full_file = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("c031c36068c60f86"),
        items: concat_sections![
            git_integration_section(),
            git_gutter_section(),
            inline_git_blame_section(),
            git_blame_view_section(),
            branch_picker_section(),
            file_diff_section(),
            git_hunks_section(),
        ],
    }
}

fn collaboration_page() -> SettingsPage {
    fn calls_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("3d5b23d1a15d06ae")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("085564ed766f1f44"),
                description: i18n::t!("cb48f988cd1c815d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("calls.mute_on_join"),
                    pick: |settings_content| settings_content.calls.as_ref()?.mute_on_join.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.calls.get_or_insert_default().mute_on_join = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("11ec5eeb88d7fe0d"),
                description: i18n::t!("929e7e972cefac04"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("calls.share_on_join"),
                    pick: |settings_content| {
                        settings_content.calls.as_ref()?.share_on_join.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.calls.get_or_insert_default().share_on_join = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn audio_settings() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::ActionLink(ActionLink {
                title: i18n::t!("dd1df075f74320a4").into(),
                description: Some(i18n::t!("fe80e6f484586b90").into()),
                button_text: i18n::t!("dd1df075f74320a4").into(),
                on_click: Arc::new(|_settings_window, window, cx| {
                    open_audio_test_window(window, cx);
                }),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b79205ef8c43a49f"),
                description: i18n::t!("129ba95513bb085d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("audio.experimental.output_audio_device"),
                    pick: |settings_content| {
                        settings_content
                            .audio
                            .as_ref()?
                            .output_audio_device
                            .as_ref()
                            .or(DEFAULT_EMPTY_AUDIO_OUTPUT)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .audio
                            .get_or_insert_default()
                            .output_audio_device = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("807d643aff00b6be"),
                description: i18n::t!("92ca34ff34daf687"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("audio.experimental.input_audio_device"),
                    pick: |settings_content| {
                        settings_content
                            .audio
                            .as_ref()?
                            .input_audio_device
                            .as_ref()
                            .or(DEFAULT_EMPTY_AUDIO_INPUT)
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .audio
                            .get_or_insert_default()
                            .input_audio_device = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("19bf536853e2e78a"),
        items: concat_sections![calls_section(), audio_settings()],
    }
}

fn code_explanations_section() -> [SettingsPageItem; 12] {
    [
        SettingsPageItem::SectionHeader(i18n::t!("16ece1acc00be44a")),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("24453781bc0e3043"),
            description: i18n::t!("639e8d4930fd5c4a"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.target_language"),
                pick: |content| content.code_explanations.as_ref()?.target_language.as_ref(),
                write: |content, value, _| {
                    content
                        .code_explanations
                        .get_or_insert_default()
                        .target_language = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("4abe198a7a3869f5"),
            description: i18n::t!("359c97c1f596e474"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.provider"),
                pick: |content| content.code_explanations.as_ref()?.provider.as_ref(),
                write: |content, value, _| {
                    content.code_explanations.get_or_insert_default().provider = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("0f16310584adb487"),
            description: i18n::t!("914e936b494e5a19"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.model"),
                pick: |content| content.code_explanations.as_ref()?.model.as_ref(),
                write: |content, value, _| {
                    content.code_explanations.get_or_insert_default().model = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("1af345c17ce268f1"),
            description: i18n::t!("109a36df07e8642e"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.prefer_existing_comments"),
                pick: |content| {
                    content
                        .code_explanations
                        .as_ref()?
                        .prefer_existing_comments
                        .as_ref()
                },
                write: |content, value, _| {
                    content
                        .code_explanations
                        .get_or_insert_default()
                        .prefer_existing_comments = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("087e804ea52c252b"),
            description: i18n::t!("581a074491460678"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.cache_persist"),
                pick: |content| content.code_explanations.as_ref()?.cache_persist.as_ref(),
                write: |content, value, _| {
                    content
                        .code_explanations
                        .get_or_insert_default()
                        .cache_persist = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("bfc1d294381c8179"),
            description: i18n::t!("62704fe91d7b2afb"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.cache_max_bytes"),
                pick: |content| content.code_explanations.as_ref()?.cache_max_bytes.as_ref(),
                write: |content, value, _| {
                    content
                        .code_explanations
                        .get_or_insert_default()
                        .cache_max_bytes = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("769a2f0db1f64f03"),
            description: i18n::t!("9f5eee44d146b69a"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.enabled"),
                pick: |content| content.code_explanations.as_ref()?.enabled.as_ref(),
                write: |content, value, _| {
                    content.code_explanations.get_or_insert_default().enabled = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("4b3c79c7aec094b4"),
            description: i18n::t!("d261d83a4133f761"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.max_function_lines"),
                pick: |content| {
                    content
                        .code_explanations
                        .as_ref()?
                        .max_function_lines
                        .as_ref()
                },
                write: |content, value, _| {
                    content
                        .code_explanations
                        .get_or_insert_default()
                        .max_function_lines = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("babef8581dfa7e5a"),
            description: i18n::t!("c96216bcfe43bca0"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.max_concurrent_requests"),
                pick: |content| {
                    content
                        .code_explanations
                        .as_ref()?
                        .max_concurrent_requests
                        .as_ref()
                },
                write: |content, value, _| {
                    content
                        .code_explanations
                        .get_or_insert_default()
                        .max_concurrent_requests = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("e1b59b01297f7af0"),
            description: i18n::t!("d94b0ed7d0b5b021"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.preload_lines"),
                pick: |content| content.code_explanations.as_ref()?.preload_lines.as_ref(),
                write: |content, value, _| {
                    content
                        .code_explanations
                        .get_or_insert_default()
                        .preload_lines = value
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("d8f750ac52b2f47b"),
            description: i18n::t!("b0105a23c91e5905"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("code_explanations.detailed"),
                pick: |content| content.code_explanations.as_ref()?.detailed.as_ref(),
                write: |content, value, _| {
                    content.code_explanations.get_or_insert_default().detailed = value
                },
            }),
            metadata: None,
            files: USER,
        }),
    ]
}

fn ai_page(cx: &App) -> SettingsPage {
    fn general_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("692873ddd8f40e66")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("af571f248d7bf8f8"),
                description: i18n::t!("9b246e4584710db1"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("disable_ai"),
                    pick: |settings_content| settings_content.project.disable_ai.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.project.disable_ai = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("86837a4449e60b81"),
                description: i18n::t!("53374aa15301e281"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.threads_sidebar.position"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .threads_sidebar
                            .as_ref()?
                            .position
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .set_threads_sidebar_position(value);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a8a43ac220365f28"),
                description: i18n::t!("c598b5661f4eddb4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.threads_sidebar.default_width"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .threads_sidebar
                            .as_ref()?
                            .default_width
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .set_threads_sidebar_default_width(value);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("daa5f89292ed82a9"),
                description: i18n::t!("307b267fe715c9d9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.threads_sidebar.auto_open"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .threads_sidebar
                            .as_ref()?
                            .auto_open
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .set_threads_sidebar_auto_open(value);
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: i18n::t!("1eabe31d01dca963").into(),
                r#type: Default::default(),
                json_path: Some("llm_providers"),
                description: Some(i18n::t!("edaa7302dd7d8531").into()),
                search_aliases: &[
                    "ai",
                    "amazon",
                    "anthropic",
                    "api key",
                    "azure",
                    "bedrock",
                    "chat",
                    "claude",
                    "copilot",
                    "gemini",
                    "github",
                    "google",
                    "gpt",
                    "grok",
                    "llama",
                    "llm",
                    "lm studio",
                    "mistral",
                    "ollama",
                    "openai",
                    "opencode",
                    "provider",
                    "vercel",
                    "xai",
                ],
                in_json: false,
                files: USER,
                render: render_llm_providers_page,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: i18n::t!("8dc040d2283eab03").into(),
                r#type: Default::default(),
                json_path: Some("agent_servers"),
                description: Some(i18n::t!("735025642c0d702c").into()),
                search_aliases: &[
                    "acp",
                    "agent client protocol",
                    "amp",
                    "claude agent",
                    "claude code",
                    "codex",
                    "copilot cli",
                    "cursor",
                    "external agent",
                    "factory droid",
                    "github copilot",
                    "grok build",
                    "junie",
                    "opencode",
                ],
                in_json: false,
                files: USER,
                render: render_external_agents_page,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: i18n::t!("a203f86cf6a6a841").into(),
                r#type: Default::default(),
                json_path: Some("context_servers"),
                description: Some(i18n::t!("02e19b1c0aebe736").into()),
                search_aliases: &["context server", "mcp", "model context protocol"],
                in_json: false,
                files: USER,
                render: render_mcp_servers_page,
            }),
        ]
    }

    fn agent_configuration_section(_cx: &App) -> Box<[SettingsPageItem]> {
        let mut items = vec![SettingsPageItem::SectionHeader(i18n::t!(
            "9c2a11d4e1c407b6"
        ))];

        items.extend([
            SettingsPageItem::SubPageLink(SubPageLink {
                title: i18n::t!("99aea2f9131ad6da").into(),
                r#type: Default::default(),
                json_path: Some(zed_actions::AGENT_SKILLS_SETTINGS_PATH),
                description: Some(i18n::t!("f78eaae8d47089bf").into()),
                search_aliases: &["agent skill", "agent skills", "custom instructions", "skill", "skills"],
                in_json: false,
                files: USER | PROJECT,
                render: render_skills_setup_page,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: i18n::t!("05fdf30411d98d32").into(),
                r#type: Default::default(),
                json_path: Some(zed_actions::AGENT_SANDBOX_SETTINGS_PATH),
                description: Some(
                    "Review and change the elevated terminal sandbox permissions that are always allowed without prompting."
                        .into(),
                ),
                search_aliases: &[
                    "allow",
                    "domain",
                    "filesystem",
                    "network",
                    "sandbox",
                    "unsandboxed",
                    "permissions",
                ],
                in_json: true,
                files: USER,
                render: render_sandbox_settings_page,
            }),
            SettingsPageItem::SubPageLink(SubPageLink {
                title: i18n::t!("a2b60a34a75b7a8e").into(),
                r#type: Default::default(),
                json_path: Some("agent.tool_permissions"),
                description: Some("Set up regex patterns to auto-allow, auto-deny, or always request confirmation, for specific tool inputs.".into()),
                search_aliases: &[],
                in_json: true,
                files: USER,
                render: render_tool_permissions_setup_page,
            }),
        ]);

        items.extend([
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("315377cc99066a95"),
                description: i18n::t!("a69260570adcba4f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.single_file_review"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.single_file_review.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .single_file_review = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("689723ab0d18014d"),
                description: i18n::t!("77e583e55fe84b99"),
                field: Box::new(SettingField {
                    organization_override: Some(|org_config| {
                        if org_config.is_agent_thread_feedback_enabled {
                            None
                        } else {
                            Some(&false)
                        }
                    }),
                    json_path: Some("agent.enable_feedback"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.enable_feedback.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .enable_feedback = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7a5f49b13d03da1b"),
                description: i18n::t!("0daea386c0e32815"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.notify_when_agent_waiting"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .notify_when_agent_waiting
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .notify_when_agent_waiting = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6d61ab97328dafe5"),
                description: i18n::t!("957e458af07aae8c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.play_sound_when_agent_done"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .play_sound_when_agent_done
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .play_sound_when_agent_done = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("05ad0bb85d819ebd"),
                description: i18n::t!("bfe0ee394bf97fbb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.prevent_idle_sleep"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.prevent_idle_sleep.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .prevent_idle_sleep = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("03dff872ae6b9f32"),
                description: i18n::t!("b7fc5f7fef26f494"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.max_idle_retained_threads"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .max_idle_retained_threads
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .max_idle_retained_threads = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("85c01169d106418b"),
                description: i18n::t!("bb6216a31fbe7b0f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.expand_edit_card"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.expand_edit_card.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .expand_edit_card = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5e55a521541bbcd4"),
                description: i18n::t!("61a000e3ba751132"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.expand_terminal_card"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .expand_terminal_card
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .expand_terminal_card = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1c92a6eafef10588"),
                description: i18n::t!("d452e3a8e0083b88"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.terminal_init_command"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .terminal_init_command
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .terminal_init_command = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("e.g. claude"),
                    display_confirm_button: true,
                    display_clear_button: true,
                    confirm_on_focus_out: true,
                    treat_missing_text_as_empty: true,
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("779707adf31b1163"),
                description: i18n::t!("834115938ff3429d"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.thinking_display"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.thinking_display.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .thinking_display = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d4ef5ec56497e679"),
                description: i18n::t!("ddd5d7e735b037fc"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.cancel_generation_on_terminal_stop"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .cancel_generation_on_terminal_stop
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .cancel_generation_on_terminal_stop = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ac5b10d3c5c6b2d8"),
                description: i18n::t!("c56b9a52913af2a4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.use_modifier_to_send"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .use_modifier_to_send
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .use_modifier_to_send = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("19785e06a679ca9a"),
                description: i18n::t!("3047f4cb99c8f4ef"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.message_editor_min_lines"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .message_editor_min_lines
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .message_editor_min_lines = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b64bafd3eb399704"),
                description: i18n::t!("5b5a3e6df5c00289"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.show_turn_stats"),
                    pick: |settings_content| {
                        settings_content.agent.as_ref()?.show_turn_stats.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .show_turn_stats = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f43ccf2b6fc77695"),
                description: i18n::t!("f74bda4a991e7a67"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.show_merge_conflict_indicator"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .show_merge_conflict_indicator
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .show_merge_conflict_indicator = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]);

        items.extend([
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5c8f67f0721e29b7"),
                description: i18n::t!("ad0fb89817253e7b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.auto_compact.enabled"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .auto_compact
                            .as_ref()?
                            .enabled
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .auto_compact
                            .get_or_insert_default()
                            .enabled = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("87b3056583c36c45"),
                description: i18n::t!("5c47bd41e0bb805b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("agent.auto_compact.threshold"),
                    pick: |settings_content| {
                        settings_content
                            .agent
                            .as_ref()?
                            .auto_compact
                            .as_ref()?
                            .threshold
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .agent
                            .get_or_insert_default()
                            .auto_compact
                            .get_or_insert_default()
                            .threshold = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("90%"),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]);

        items.into_boxed_slice()
    }

    fn edit_prediction_display_sub_section() -> [SettingsPageItem; 1] {
        [SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("8b91a131263f2b5f"),
            description: i18n::t!("0182d67ca14ff066"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("edit_prediction.display_mode"),
                pick: |settings_content| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .mode
                        .as_ref()
                },
                write: |settings_content, value, _| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .mode = value;
                },
            }),
            metadata: None,
            files: USER,
        })]
    }

    SettingsPage {
        title: "AI",
        items: concat_sections!(
            @vec,
            general_section(),
            code_explanations_section(),
            agent_configuration_section(cx),
            edit_prediction_language_settings_section(),
            edit_prediction_display_sub_section(),
        )
        .into(),
    }
}

fn network_page() -> SettingsPage {
    fn network_section() -> [SettingsPageItem; 3] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("97b31b5d63f57e51")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5e84ea61e8386af7"),
                description: i18n::t!("1a0081da0b64c4ef"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("proxy"),
                    pick: |settings_content| settings_content.proxy.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.proxy = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("socks5h://localhost:10808"),
                    ..Default::default()
                })),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ee58ddc56a6304aa"),
                description: i18n::t!("1ae1a9c68224b3bc"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("server_url"),
                    pick: |settings_content| settings_content.server_url.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.server_url = value;
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    placeholder: Some("https://zed.dev"),
                    ..Default::default()
                })),
                files: USER,
            }),
        ]
    }

    fn remote_server_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("9beed95cb3cfacc9")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("45507efef78ba917"),
                description: i18n::t!("bd435dd3a1eee5d7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("china_server_adaptation"),
                    pick: |settings_content| {
                        settings_content.remote.china_server_adaptation.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.remote.china_server_adaptation = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    SettingsPage {
        title: i18n::t!("97b31b5d63f57e51"),
        items: concat_sections![network_section(), remote_server_section()],
    }
}

fn language_settings_field<T>(
    settings_content: &SettingsContent,
    get_language_setting_field: fn(&LanguageSettingsContent) -> Option<&T>,
) -> Option<&T> {
    let all_languages = &settings_content.project.all_languages;

    active_language()
        .and_then(|current_language_name| {
            all_languages
                .languages
                .0
                .get(current_language_name.as_ref())
        })
        .and_then(get_language_setting_field)
        .or_else(|| get_language_setting_field(&all_languages.defaults))
}

fn language_settings_field_mut<T>(
    settings_content: &mut SettingsContent,
    value: Option<T>,
    write: fn(&mut LanguageSettingsContent, Option<T>),
) {
    let all_languages = &mut settings_content.project.all_languages;
    let language_content = if let Some(current_language) = active_language() {
        all_languages
            .languages
            .0
            .entry(current_language.to_string())
            .or_default()
    } else {
        &mut all_languages.defaults
    };
    write(language_content, value);
}

fn language_settings_data() -> Box<[SettingsPageItem]> {
    fn indentation_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("66c740387e6b3827")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("376be073a8d2f506"),
                description: i18n::t!("2c5a08e1f9cad17e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).tab_size"), // TODO(cameron): not JQ syntax because not URL-safe
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.tab_size.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.tab_size = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5cabf599ffe684d7"),
                description: i18n::t!("3502ed2c29622228"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).hard_tabs"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.hard_tabs.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.hard_tabs = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e5eea15164c04dbe"),
                description: i18n::t!("ade4f6246c24c68f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).auto_indent"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.auto_indent.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.auto_indent = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("628749307ab6f0b2"),
                description: i18n::t!("7d67d36f71e22cf9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).auto_indent_on_paste"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.auto_indent_on_paste.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.auto_indent_on_paste = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn wrapping_section() -> [SettingsPageItem; 7] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("bd609a8e2d40f3ef")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("77a01689bd6dd157"),
                description: i18n::t!("520243f8eebe1c73"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).soft_wrap"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.soft_wrap.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.soft_wrap = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("70700ef73581f803"),
                description: i18n::t!("e25b84764e5405df"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).soft_wrap_indent"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.soft_wrap_indent.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.soft_wrap_indent = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a422942fcc4a0141"),
                description: i18n::t!("4bd0a2995869f353"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_wrap_guides"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_wrap_guides.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_wrap_guides = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b5b9d9cd79c0285e"),
                description: i18n::t!("6315dab1c7efb456"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).preferred_line_length"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.preferred_line_length.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.preferred_line_length = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("66a6577105c54d61"),
                description: i18n::t!("23a9b0556f89d830"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).wrap_guides"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.wrap_guides.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.wrap_guides = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("286453355ca21440"),
                description: i18n::t!("70ba715c021d728b"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).allow_rewrap"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.allow_rewrap.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.allow_rewrap = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn indent_guides_section() -> [SettingsPageItem; 6] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("6ad938c8c789f951")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("dfb802238b38fbd4"),
                description: i18n::t!("88f25dc7f2c7dcb8"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.enabled.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.indent_guides.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5eb9f4e84a63fb27"),
                description: i18n::t!("efab44b0a6db76aa"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.line_width"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.line_width.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.indent_guides.get_or_insert_default().line_width = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("62e66562fc72c865"),
                description: i18n::t!("d987bf8d5c56aa27"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.active_line_width"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.active_line_width.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .indent_guides
                                .get_or_insert_default()
                                .active_line_width = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7b6fd1f9a75ed3df"),
                description: i18n::t!("179418cb9bffada7"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.coloring"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language
                                .indent_guides
                                .as_ref()
                                .and_then(|indent_guides| indent_guides.coloring.as_ref())
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.indent_guides.get_or_insert_default().coloring = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("98d41705630f0df5"),
                description: i18n::t!("7a2d3d2de3ddfd35"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).indent_guides.background_coloring"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.indent_guides.as_ref().and_then(|indent_guides| {
                                indent_guides.background_coloring.as_ref()
                            })
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .indent_guides
                                .get_or_insert_default()
                                .background_coloring = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn formatting_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader("Formatting"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("23365ae4886a70f9"),
                description: i18n::t!("09512500b9f07089"),
                field: Box::new(
                    // TODO(settings_ui): this setting should just be a bool
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).format_on_save"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.format_on_save.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.format_on_save = value;
                                },
                            )
                        },
                    },
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("805a56f2b74ccc0f"),
                description: i18n::t!("fa0fd2bd3b3b22d6"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).remove_trailing_whitespace_on_save"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.remove_trailing_whitespace_on_save.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.remove_trailing_whitespace_on_save = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("49a57e2bed8b3f99"),
                description: i18n::t!("d205ce1e6cd4951f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).ensure_final_newline_on_save"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.ensure_final_newline_on_save.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.ensure_final_newline_on_save = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("58872d8055045153"),
                description: i18n::t!("19e88cff5b60ca18"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).line_ending"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.line_ending.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.line_ending = value;
                        })
                    },
                }),
                metadata: Some(Box::new(SettingsFieldMetadata {
                    should_do_titlecase: Some(false),
                    ..Default::default()
                })),
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1c968521e85255c7"),
                description: i18n::t!("8762f8c56ce498ef"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).formatter"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.formatter.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.formatter = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("092abf62dce6f65a"),
                description: i18n::t!("e9542ab8829ac793"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).use_on_type_format"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.use_on_type_format.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.use_on_type_format = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0c258d86ce02f0c8"),
                description: i18n::t!("f6c37aafa9999a43"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).code_actions_on_format"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.code_actions_on_format.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.code_actions_on_format = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn autoclose_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Autoclose"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f0a46252de308641"),
                description: i18n::t!("e0114f0db9a492d9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).use_autoclose"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.use_autoclose.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.use_autoclose = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("83241d462b1e1e1a"),
                description: i18n::t!("b6118ebe10455abe"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).use_auto_surround"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.use_auto_surround.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.use_auto_surround = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1852cf079a43ef8a"),
                description: i18n::t!("22bd0a29dfc54759"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).always_treat_brackets_as_autoclosed"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.always_treat_brackets_as_autoclosed.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.always_treat_brackets_as_autoclosed = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("891d310253e5ab81"),
                description: i18n::t!("df481f3d759529db"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).jsx_tag_auto_close"),
                    // TODO(settings_ui): this setting should just be a bool
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.jsx_tag_auto_close.as_ref()?.enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.jsx_tag_auto_close.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn whitespace_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader("Whitespace"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1439be2bfa25c6d3"),
                description: i18n::t!("8f0dd8960ea60bca"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_whitespaces"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_whitespaces.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_whitespaces = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("c14f6dbdc86e4b67"),
                description: i18n::t!("71e80fe90b03c341"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).whitespace_map.space"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.whitespace_map.as_ref()?.space.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.whitespace_map.get_or_insert_default().space = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a5186f4ebfb121a8"),
                description: i18n::t!("8775903c17a7cfd3"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).whitespace_map.tab"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.whitespace_map.as_ref()?.tab.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.whitespace_map.get_or_insert_default().tab = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn completions_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader("Completions"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0757e1c1af31c04e"),
                description: i18n::t!("764934dda34bb2ff"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_completions_on_input"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_completions_on_input.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_completions_on_input = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0557a24b555e76d3"),
                description: i18n::t!("0f386770f5cbee5c"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).show_completion_documentation"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.show_completion_documentation.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.show_completion_documentation = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f3b5980f18ff903e"),
                description: i18n::t!("efd88b03ff8bfaee"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.words"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.words.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.completions.get_or_insert_default().words = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("cbc4c284d1d7f6aa"),
                description: i18n::t!("8c12d10d54bd0201"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.words_min_length"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.words_min_length.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .completions
                                .get_or_insert_default()
                                .words_min_length = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("5a0fca58e2c4cbe2"),
                description: i18n::t!("d8a6bab6cfc71d40"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.completion_menu_scrollbar"),
                    pick: |settings_content| {
                        settings_content.editor.completion_menu_scrollbar.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.completion_menu_scrollbar = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("36f0178704be373c"),
                description: i18n::t!("0c1efa29126e9622"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.completion_detail_alignment"),
                    pick: |settings_content| {
                        settings_content.editor.completion_detail_alignment.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.completion_detail_alignment = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("b77d7a296775eef2"),
                description: i18n::t!("ad53f76e20e346d9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("editor.completion_menu_item_kind"),
                    pick: |settings_content| {
                        settings_content.editor.completion_menu_item_kind.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.completion_menu_item_kind = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    fn inlay_hints_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("b3a81b56d7f63e8c")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("dfb802238b38fbd4"),
                description: i18n::t!("0f076b5e96f758f0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.inlay_hints.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("2ade72ddf164a27f"),
                description: i18n::t!("4e22635a7b598894"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_value_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_value_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .show_value_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fdcb0d42c70ccec2"),
                description: i18n::t!("a9a79f9da735c1d2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_type_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_type_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.inlay_hints.get_or_insert_default().show_type_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ef58f97477e750bf"),
                description: i18n::t!("db287e854fe6cbec"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_parameter_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_parameter_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .show_parameter_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("286fa862654bf77c"),
                description: i18n::t!("38e55af13fd8f0f5"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_other_hints"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_other_hints.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .show_other_hints = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6a35332ddb66fd7f"),
                description: i18n::t!("012de2df1b1a314e"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.show_background"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.show_background.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.inlay_hints.get_or_insert_default().show_background = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("7eb07228ffa8c4d2"),
                description: i18n::t!("2250e63535353c83"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.edit_debounce_ms"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.edit_debounce_ms.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .edit_debounce_ms = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("1eb021c2d86389ab"),
                description: i18n::t!("26215649c4f68872"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).inlay_hints.scroll_debounce_ms"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.inlay_hints.as_ref()?.scroll_debounce_ms.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .inlay_hints
                                .get_or_insert_default()
                                .scroll_debounce_ms = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("246da760a74227b8"),
                description: i18n::t!("67823628b87ea87d"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some(
                            "languages.$(language).inlay_hints.toggle_on_modifiers_press",
                        ),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language
                                    .inlay_hints
                                    .as_ref()?
                                    .toggle_on_modifiers_press
                                    .as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language
                                        .inlay_hints
                                        .get_or_insert_default()
                                        .toggle_on_modifiers_press = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn tasks_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("5253040db8643c85")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("dfb802238b38fbd4"),
                description: i18n::t!("b24886f7f89484fb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).tasks.enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.tasks.as_ref()?.enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.tasks.get_or_insert_default().enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("a772fa4ebe36b63d"),
                description: i18n::t!("28f1dd506aac8cd6"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).tasks.variables"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.tasks.as_ref()?.variables.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.tasks.get_or_insert_default().variables = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4c8852ac266ef37c"),
                description: i18n::t!("8002c8f500d405e9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).tasks.prefer_lsp"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.tasks.as_ref()?.prefer_lsp.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.tasks.get_or_insert_default().prefer_lsp = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn miscellaneous_section() -> [SettingsPageItem; 8] {
        [
            SettingsPageItem::SectionHeader("Miscellaneous"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("bce42437c31f4fc1"),
                description: i18n::t!("df049040f7a79950"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("language_detection"),
                    pick: |settings_content| settings_content.editor.language_detection.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.language_detection = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d3424ce0d61f2b56"),
                description: i18n::t!("a1f966e1383d34d0"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).word_diff_enabled"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.word_diff_enabled.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.word_diff_enabled = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("20fe3f3e72ca01b8"),
                description: i18n::t!("f2fa43f200fb29e7"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).debuggers"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.debuggers.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.debuggers = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("d3f6f1d5395c6598"),
                description: i18n::t!("be3f033b3c65c38f"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).editor.middle_click_paste"),
                    pick: |settings_content| settings_content.editor.middle_click_paste.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.middle_click_paste = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("84e3bc0dad6ccd80"),
                description: i18n::t!("71cb2ed278dbf919"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).extend_comment_on_newline"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.extend_comment_on_newline.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.extend_comment_on_newline = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("23b02c2557996c35"),
                description: i18n::t!("26757e6ec1960e32"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).colorize_brackets"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.colorize_brackets.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.colorize_brackets = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("27547de3cbac3f79"),
                description: i18n::t!("d6b404da9277e4ee"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("modeline_lines"),
                    pick: |settings_content| settings_content.modeline_lines.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.modeline_lines = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn global_only_miscellaneous_sub_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("166ed1e941490fac"),
                description: i18n::t!("189beb5cf4fa55af"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("image_viewer.unit"),
                    pick: |settings_content| {
                        settings_content
                            .image_viewer
                            .as_ref()
                            .and_then(|image_viewer| image_viewer.unit.as_ref())
                    },
                    write: |settings_content, value, _| {
                        settings_content.image_viewer.get_or_insert_default().unit = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ff691edfa683b148"),
                description: i18n::t!("17928e418bae42a9"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("markdown_preview.open_markdown_files_in_preview"),
                    pick: |settings_content| {
                        settings_content
                            .markdown_preview
                            .as_ref()?
                            .open_markdown_files_in_preview
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .markdown_preview
                            .get_or_insert_default()
                            .open_markdown_files_in_preview = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::DynamicItem(DynamicItem {
                discriminant: SettingItem {
                    files: USER,
                    title: i18n::t!("ec048840b3f51843"),
                    description: i18n::t!("ac2527f11f07ede8"),
                    field: Box::new(SettingField::<bool> {
                        organization_override: None,
                        json_path: Some("markdown_preview.limit_content_width"),
                        pick: |settings_content| {
                            settings_content
                                .markdown_preview
                                .as_ref()?
                                .limit_content_width
                                .as_ref()
                        },
                        write: |settings_content, value, _| {
                            settings_content
                                .markdown_preview
                                .get_or_insert_default()
                                .limit_content_width = value;
                        },
                    }),
                    metadata: None,
                },
                pick_discriminant: |settings_content| {
                    let enabled = settings_content
                        .markdown_preview
                        .as_ref()?
                        .limit_content_width
                        .unwrap_or(true);
                    Some(if enabled { 1 } else { 0 })
                },
                fields: vec![
                    vec![],
                    vec![SettingItem {
                        files: USER,
                        title: i18n::t!("d36a6a740ac75f4a"),
                        description: i18n::t!("93c89d04cea63297"),
                        field: Box::new(SettingField {
                            organization_override: None,
                            json_path: Some("markdown_preview.max_width"),
                            pick: |settings_content| {
                                settings_content
                                    .markdown_preview
                                    .as_ref()?
                                    .max_width
                                    .as_ref()
                            },
                            write: |settings_content, value, _| {
                                settings_content
                                    .markdown_preview
                                    .get_or_insert_default()
                                    .max_width = value;
                            },
                        }),
                        metadata: None,
                    }],
                ],
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("76c5a2a8b436ab80"),
                description: i18n::t!("2f4bd1624d786e03"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("drop_target_size"),
                    pick: |settings_content| settings_content.workspace.drop_target_size.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.workspace.drop_target_size = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]
    }

    let is_global = active_language().is_none();

    let code_lens_item = [SettingsPageItem::SettingItem(SettingItem {
        title: i18n::t!("3ca23395b0aaaec2"),
        description: i18n::t!("b0a67da0d72cb736"),
        field: Box::new(SettingField {
            organization_override: None,
            json_path: Some("code_lens"),
            pick: |settings_content| settings_content.editor.code_lens.as_ref(),
            write: |settings_content, value, _| {
                settings_content.editor.code_lens = value;
            },
        }),
        metadata: None,
        files: USER,
    })];

    let lsp_document_colors_item = [SettingsPageItem::SettingItem(SettingItem {
        title: i18n::t!("8c43d04815b785ac"),
        description: i18n::t!("014dde7cfa3083a8"),
        field: Box::new(SettingField {
            organization_override: None,
            json_path: Some("lsp_document_colors"),
            pick: |settings_content| settings_content.editor.lsp_document_colors.as_ref(),
            write: |settings_content, value, _| {
                settings_content.editor.lsp_document_colors = value;
            },
        }),
        metadata: None,
        files: USER,
    })];

    if is_global {
        concat_sections!(
            indentation_section(),
            wrapping_section(),
            indent_guides_section(),
            formatting_section(),
            autoclose_section(),
            whitespace_section(),
            completions_section(),
            inlay_hints_section(),
            code_lens_item,
            lsp_document_colors_item,
            tasks_section(),
            miscellaneous_section(),
            global_only_miscellaneous_sub_section(),
        )
    } else {
        concat_sections!(
            indentation_section(),
            wrapping_section(),
            indent_guides_section(),
            formatting_section(),
            autoclose_section(),
            whitespace_section(),
            completions_section(),
            inlay_hints_section(),
            code_lens_item,
            tasks_section(),
            miscellaneous_section(),
        )
    }
}

/// LanguageSettings items that should be included in the "Languages & Tools" page
/// not the "Editor" page
fn non_editor_language_settings_data() -> Box<[SettingsPageItem]> {
    fn lsp_section() -> [SettingsPageItem; 10] {
        [
            SettingsPageItem::SectionHeader("LSP"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0cd695db93a638f7"),
                description: i18n::t!("543888ff18f9ea98"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).enable_language_server"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.enable_language_server.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.enable_language_server = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("09375000f874c8ec"),
                description: i18n::t!("a878a583d2fcd92b"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).language_servers"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.language_servers.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.language_servers = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("85c8d53febbf47b6"),
                description: i18n::t!("aecb83cd54cd7ddb"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).linked_edits"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.linked_edits.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.linked_edits = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("0dd114a5a9217b68"),
                description: i18n::t!("67e50825b86bb2d4"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("go_to_definition_fallback"),
                    pick: |settings_content| {
                        settings_content.editor.go_to_definition_fallback.as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.go_to_definition_fallback = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("4987408009427d78"),
                description: i18n::t!("7b2b0b4e4f860145"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("go_to_definition_scroll_strategy"),
                    pick: |settings_content| {
                        settings_content
                            .editor
                            .go_to_definition_scroll_strategy
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content.editor.go_to_definition_scroll_strategy = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("fcfed20e9daf9a1b"),
                description: i18n::t!("359e1b4ea0918697"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("lsp_results_location"),
                    pick: |settings_content| settings_content.editor.lsp_results_location.as_ref(),
                    write: |settings_content, value, _| {
                        settings_content.editor.lsp_results_location = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("8e4fcf30ffb033a0"),
                description: {
                    static DESCRIPTION: OnceLock<&'static str> = OnceLock::new();
                    DESCRIPTION.get_or_init(|| {
                        SemanticTokens::VARIANTS
                            .iter()
                            .filter_map(|v| {
                                v.get_documentation().map(|doc| format!("{v:?}: {doc}"))
                            })
                            .join("\n")
                            .leak()
                    })
                },
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).semantic_tokens"),
                    pick: |settings_content| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .semantic_tokens
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .project
                            .all_languages
                            .defaults
                            .semantic_tokens = value;
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("804763e6388801e3"),
                description: i18n::t!("81a936a4848c8835"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).document_folding_ranges"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.document_folding_ranges.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.document_folding_ranges = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("600eddee8d190fbf"),
                description: i18n::t!("fa37699dd5f65d21"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).document_symbols"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.document_symbols.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.document_symbols = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn lsp_completions_section() -> [SettingsPageItem; 4] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("0f5be66b352bfe9b")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("f4f0ead1116b5b62"),
                description: i18n::t!("6dfa8e74e68d2627"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.lsp"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.lsp.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.completions.get_or_insert_default().lsp = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("cab24754d8c3cac7"),
                description: i18n::t!("8a6629862a4123dd"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.lsp_fetch_timeout_ms"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.lsp_fetch_timeout_ms.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language
                                .completions
                                .get_or_insert_default()
                                .lsp_fetch_timeout_ms = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("6d6f240895aca008"),
                description: i18n::t!("020b6f5235aa09a5"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).completions.lsp_insert_mode"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.completions.as_ref()?.lsp_insert_mode.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.completions.get_or_insert_default().lsp_insert_mode = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn debugger_section() -> [SettingsPageItem; 2] {
        [
            SettingsPageItem::SectionHeader(i18n::t!("20fe3f3e72ca01b8")),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("20fe3f3e72ca01b8"),
                description: i18n::t!("f2fa43f200fb29e7"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).debuggers"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.debuggers.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.debuggers = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    fn prettier_section() -> [SettingsPageItem; 5] {
        [
            SettingsPageItem::SectionHeader("Prettier"),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("ce7ef28b670ade58"),
                description: i18n::t!("9ede04faf4154db2"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).prettier.allowed"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.prettier.as_ref()?.allowed.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.prettier.get_or_insert_default().allowed = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("2a06708ff6b14f10"),
                description: i18n::t!("960d5d8bd1729975"),
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).prettier.parser"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.prettier.as_ref()?.parser.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.prettier.get_or_insert_default().parser = value;
                        })
                    },
                }),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("e806fbbe8ec6e82d"),
                description: i18n::t!("590cf39c689d4358"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).prettier.plugins"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.prettier.as_ref()?.plugins.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.prettier.get_or_insert_default().plugins = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: i18n::t!("bb7486f4410fd370"),
                description: i18n::t!("500570e878149ca6"),
                field: Box::new(
                    SettingField {
                        organization_override: None,
                        json_path: Some("languages.$(language).prettier.options"),
                        pick: |settings_content| {
                            language_settings_field(settings_content, |language| {
                                language.prettier.as_ref()?.options.as_ref()
                            })
                        },
                        write: |settings_content, value, _| {
                            language_settings_field_mut(
                                settings_content,
                                value,
                                |language, value| {
                                    language.prettier.get_or_insert_default().options = value;
                                },
                            )
                        },
                    }
                    .unimplemented(),
                ),
                metadata: None,
                files: USER | PROJECT,
            }),
        ]
    }

    concat_sections!(
        lsp_section(),
        lsp_completions_section(),
        debugger_section(),
        prettier_section(),
    )
}

fn edit_prediction_language_settings_section() -> [SettingsPageItem; 5] {
    [
        SettingsPageItem::SectionHeader(i18n::t!("34627253269ac8a6")),
        SettingsPageItem::SubPageLink(SubPageLink {
            title: i18n::t!("4b08b9a69dd7a595").into(),
            r#type: Default::default(),
            json_path: Some("edit_predictions.providers"),
            description: Some(i18n::t!("bc1739d9f184f06d").into()),
            search_aliases: &[],
            in_json: false,
            files: USER,
            render: render_edit_prediction_setup_page,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("7d7b4beebaa2d4a3"),
            description: i18n::t!("537709fe6a4e9547"),
            field: Box::new(SettingField {
                organization_override: Some(|org_settings| {
                    const DATA_COLLECTION_DISABLED: EditPredictionDataCollectionChoice =
                        EditPredictionDataCollectionChoice::No;

                    if !org_settings.edit_prediction.is_feedback_enabled {
                        Some(&DATA_COLLECTION_DISABLED)
                    } else {
                        None
                    }
                }),
                json_path: Some("edit_predictions.allow_data_collection"),
                pick: |settings_content| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .allow_data_collection
                        .as_ref()
                },
                write: |settings_content, value, _app| {
                    settings_content
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .allow_data_collection = value;
                },
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("b7abea77feb0b8b9"),
            description: i18n::t!("b68ba59b36f0d447"),
            field: Box::new(SettingField {
                organization_override: None,
                json_path: Some("languages.$(language).show_edit_predictions"),
                pick: |settings_content| {
                    language_settings_field(settings_content, |language| {
                        language.show_edit_predictions.as_ref()
                    })
                },
                write: |settings_content, value, _| {
                    language_settings_field_mut(settings_content, value, |language, value| {
                        language.show_edit_predictions = value;
                    })
                },
            }),
            metadata: None,
            files: USER | PROJECT,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: i18n::t!("6fe11c1f3b880f90"),
            description: i18n::t!("71fc41d075e084f7"),
            field: Box::new(
                SettingField {
                    organization_override: None,
                    json_path: Some("languages.$(language).edit_predictions_disabled_in"),
                    pick: |settings_content| {
                        language_settings_field(settings_content, |language| {
                            language.edit_predictions_disabled_in.as_ref()
                        })
                    },
                    write: |settings_content, value, _| {
                        language_settings_field_mut(settings_content, value, |language, value| {
                            language.edit_predictions_disabled_in = value;
                        })
                    },
                }
                .unimplemented(),
            ),
            metadata: None,
            files: USER | PROJECT,
        }),
    ]
}

fn show_scrollbar_or_editor(
    settings_content: &SettingsContent,
    show: fn(&SettingsContent) -> Option<&settings::ShowScrollbar>,
) -> Option<&settings::ShowScrollbar> {
    show(settings_content).or(settings_content
        .editor
        .scrollbar
        .as_ref()
        .and_then(|scrollbar| scrollbar.show.as_ref()))
}

fn dynamic_variants<T>() -> &'static [T::Discriminant]
where
    T: strum::IntoDiscriminant,
    T::Discriminant: strum::VariantArray,
{
    <<T as strum::IntoDiscriminant>::Discriminant as strum::VariantArray>::VARIANTS
}

/// Updates the `vim_mode` setting, disabling `helix_mode` if present and
/// `vim_mode` is being enabled.
fn write_vim_mode(settings: &mut SettingsContent, value: Option<bool>, _: &App) {
    write_vim_mode_inner(settings, value);
}

fn write_vim_mode_inner(settings: &mut SettingsContent, value: Option<bool>) {
    if value == Some(true) && settings.helix_mode == Some(true) {
        settings.helix_mode = Some(false);
    }
    settings.vim_mode = value;
}

/// Updates the `helix_mode` setting, disabling `vim_mode` if present and
/// `helix_mode` is being enabled.
fn write_helix_mode(settings: &mut SettingsContent, value: Option<bool>, _: &App) {
    write_helix_mode_inner(settings, value);
}

fn write_helix_mode_inner(settings: &mut SettingsContent, value: Option<bool>) {
    if value == Some(true) && settings.vim_mode == Some(true) {
        settings.vim_mode = Some(false);
    }
    settings.helix_mode = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_vim_helix_mode() {
        // Enabling vim mode while `vim_mode` and `helix_mode` are not yet set
        // should only update the `vim_mode` setting.
        let mut settings = SettingsContent::default();
        write_vim_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.vim_mode, Some(true));
        assert_eq!(settings.helix_mode, None);

        // Enabling helix mode while `vim_mode` and `helix_mode` are not yet set
        // should only update the `helix_mode` setting.
        let mut settings = SettingsContent::default();
        write_helix_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.helix_mode, Some(true));
        assert_eq!(settings.vim_mode, None);

        // Disabling helix mode should only touch `helix_mode` setting when
        // `vim_mode` is not set.
        write_helix_mode_inner(&mut settings, Some(false));
        assert_eq!(settings.helix_mode, Some(false));
        assert_eq!(settings.vim_mode, None);

        // Enabling vim mode should update `vim_mode` but leave `helix_mode`
        // untouched.
        write_vim_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.vim_mode, Some(true));
        assert_eq!(settings.helix_mode, Some(false));

        // Enabling helix mode should update `helix_mode` and disable
        // `vim_mode`.
        write_helix_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.helix_mode, Some(true));
        assert_eq!(settings.vim_mode, Some(false));

        // Enabling vim mode should update `vim_mode` and disable
        // `helix_mode`.
        write_vim_mode_inner(&mut settings, Some(true));
        assert_eq!(settings.vim_mode, Some(true));
        assert_eq!(settings.helix_mode, Some(false));
    }

    #[gpui::test]
    fn test_language_setting_round_trips(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let page = general_page(cx);
            let item = page
                .items
                .iter()
                .find_map(|item| match item {
                    SettingsPageItem::SettingItem(item)
                        if item.title == i18n::t!("3d13868593ae4eeb") =>
                    {
                        Some(item)
                    }
                    _ => None,
                })
                .expect("general page should expose the interface language setting");

            let field = item
                .field
                .as_any()
                .downcast_ref::<SettingField<settings::UiLanguage>>()
                .expect("interface language setting should use the UiLanguage field type");
            assert_eq!(field.json_path, Some("language"));

            let mut content = SettingsContent::default();
            assert!((field.pick)(&content).is_none());

            (field.write)(
                &mut content,
                Some(settings::UiLanguage("en".to_string())),
                cx,
            );
            assert_eq!(
                content.language.as_ref().map(|language| language.as_str()),
                Some("en")
            );
            assert_eq!(
                (field.pick)(&content).map(|language| language.as_str()),
                Some("en")
            );
        });
    }

    #[gpui::test]
    fn test_appearance_indent_guide_background_coloring_round_trips(cx: &mut gpui::TestAppContext) {
        cx.update(|_cx| {
            let page = appearance_page();
            let item = page
                .items
                .iter()
                .find_map(|item| match item {
                    SettingsPageItem::SettingItem(item)
                        if item.field.json_path() == Some("indent_guides.background_coloring") =>
                    {
                        Some(item)
                    }
                    _ => None,
                })
                .expect("appearance page should expose indent guide background coloring");

            let field = item
                .field
                .as_any()
                .downcast_ref::<SettingField<settings::IndentGuideBackgroundColoring>>()
                .expect(
                    "indent guide background coloring should use the \
                     IndentGuideBackgroundColoring field type",
                );

            let mut content = SettingsContent::default();
            assert!((field.pick)(&content).is_none());

            (field.write)(
                &mut content,
                Some(settings::IndentGuideBackgroundColoring::IndentAware),
                _cx,
            );
            assert_eq!(
                content
                    .project
                    .all_languages
                    .defaults
                    .indent_guides
                    .as_ref()
                    .and_then(|indent_guides| indent_guides.background_coloring),
                Some(settings::IndentGuideBackgroundColoring::IndentAware)
            );
            assert_eq!(
                (field.pick)(&content),
                Some(&settings::IndentGuideBackgroundColoring::IndentAware)
            );
        });
    }
}
