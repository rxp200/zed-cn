use collab_ui::collab_panel;
use gpui::{App, Menu, MenuItem, OsAction};
use project::DisableAiSettings;
use release_channel::ReleaseChannel;
use settings::Settings;
use terminal_view::terminal_panel;
use zed_actions::{Quit, assistant, debug_panel, dev, git_panel, project_panel};

pub fn app_menus(cx: &mut App) -> Vec<Menu> {
    let mut view_items = vec![
        MenuItem::action(
            i18n::t!("80f8fbcfa0117633"),
            zed_actions::IncreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            i18n::t!("290f68030501cd9c"),
            zed_actions::DecreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            i18n::t!("c9cdc9678c5163e9"),
            zed_actions::ResetBufferFontSize { persist: false },
        ),
        MenuItem::action(
            i18n::t!("9b54505e412a2f03"),
            zed_actions::ResetAllZoom { persist: false },
        ),
        MenuItem::separator(),
        MenuItem::action(i18n::t!("3b587696ae85756d"), workspace::ToggleLeftDock),
        MenuItem::action(i18n::t!("4e5ac32751f928af"), workspace::ToggleRightDock),
        MenuItem::action(i18n::t!("0f87a5037a492f65"), workspace::ToggleBottomDock),
        MenuItem::action(i18n::t!("88c2591b53ed13b6"), workspace::ToggleAllDocks),
        MenuItem::submenu(Menu {
            name: i18n::t!("9099a016b2c604f7").into(),
            disabled: false,
            items: vec![
                MenuItem::action(i18n::t!("8719c337f07261f3"), workspace::SplitUp::default()),
                MenuItem::action(
                    i18n::t!("d7e72a7908f9ae8e"),
                    workspace::SplitDown::default(),
                ),
                MenuItem::action(
                    i18n::t!("4fcfde0c0fafef93"),
                    workspace::SplitLeft::default(),
                ),
                MenuItem::action(
                    i18n::t!("7c9ed6c199718d94"),
                    workspace::SplitRight::default(),
                ),
            ],
        }),
        MenuItem::separator(),
        MenuItem::action(i18n::t!("24c9ed3c6f473104"), project_panel::ToggleFocus),
        MenuItem::action(i18n::t!("b200b18ce8ed1622"), outline_panel::ToggleFocus),
        MenuItem::action(i18n::t!("538fd707c82e7e9b"), collab_panel::ToggleFocus),
        MenuItem::action(i18n::t!("a5a9dd6720c79887"), terminal_panel::Toggle),
        MenuItem::action(i18n::t!("66c849d89d539e9e"), debug_panel::ToggleFocus),
    ];

    if !DisableAiSettings::get_global(cx).disable_ai {
        view_items.push(MenuItem::action(
            i18n::t!("3bb0698e654c0693"),
            assistant::ToggleFocus,
        ));
    }

    view_items.extend([
        MenuItem::action(i18n::t!("b2fe25fd981a562f"), git_panel::ToggleFocus),
        MenuItem::separator(),
        MenuItem::action(i18n::t!("40ff6300f9817deb"), diagnostics::Deploy),
        MenuItem::separator(),
    ]);

    if ReleaseChannel::try_global(cx) == Some(ReleaseChannel::Dev) {
        view_items.push(MenuItem::action(
            i18n::t!("17556ceb6c27e1a7"),
            dev::ToggleInspector,
        ));
        view_items.push(MenuItem::separator());
    }

    vec![
        Menu {
            name: "Zed".into(),
            disabled: false,
            items: vec![
                MenuItem::action(i18n::t!("8b7bb89ee15002d4"), zed_actions::About),
                MenuItem::action(i18n::t!("7f68ebad19ba6bcd"), auto_update::Check),
                MenuItem::separator(),
                MenuItem::submenu(Menu::new(i18n::t!("df3d58c7d84b85f2")).items([
                    MenuItem::action(i18n::t!("37aa6ad6a36d46fa"), zed_actions::OpenSettings),
                    MenuItem::action(i18n::t!("3018cf131663ad3f"), super::OpenSettingsFile),
                    MenuItem::action(
                        i18n::t!("d6967360ec9dd259"),
                        zed_actions::OpenProjectSettings,
                    ),
                    MenuItem::action(i18n::t!("a49b8d95a5bc3b80"), super::OpenProjectSettingsFile),
                    MenuItem::action(i18n::t!("2bdab35aca97b923"), super::OpenDefaultSettings),
                    MenuItem::separator(),
                    MenuItem::action(i18n::t!("f6d844d023d08eac"), zed_actions::OpenKeymap),
                    MenuItem::action(i18n::t!("523507118350e4de"), zed_actions::OpenKeymapFile),
                    MenuItem::action(i18n::t!("3b6facf97e3bc929"), zed_actions::OpenDefaultKeymap),
                    MenuItem::separator(),
                    MenuItem::action(
                        i18n::t!("c1b190813cbfedc9"),
                        zed_actions::theme_selector::Toggle::default(),
                    ),
                    MenuItem::action(
                        i18n::t!("b93536f1a11bcf35"),
                        zed_actions::icon_theme_selector::Toggle::default(),
                    ),
                ])),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::os_submenu(i18n::t!("ec309ab207ef7fa3"), gpui::SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("99a4e1e59743908f"),
                    zed_actions::Extensions::default(),
                ),
                #[cfg(not(target_os = "windows"))]
                MenuItem::action(i18n::t!("4994da47a3359b69"), install_cli::InstallCliBinary),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::action(i18n::t!("cdbf883c4289ab8c"), super::Hide),
                #[cfg(target_os = "macos")]
                MenuItem::action(i18n::t!("d32bd5a0edfe35c3"), super::HideOthers),
                #[cfg(target_os = "macos")]
                MenuItem::action(i18n::t!("84941ac6e844e7af"), super::ShowAll),
                MenuItem::separator(),
                MenuItem::action(i18n::t!("bef628bd072dc985"), Quit),
            ],
        },
        Menu {
            name: i18n::t!("39932f24fe11a6ba").into(),
            disabled: false,
            items: vec![
                MenuItem::action(i18n::t!("50ef2f4cf6a46924"), workspace::NewFile),
                MenuItem::action(i18n::t!("8b78022ec20aa7b7"), workspace::NewWindow),
                MenuItem::separator(),
                #[cfg(not(target_os = "macos"))]
                MenuItem::action(i18n::t!("2df17a66b26e9398"), workspace::OpenFiles),
                MenuItem::action(
                    if cfg!(not(target_os = "macos")) {
                        i18n::t!("15110e31656a5995")
                    } else {
                        i18n::t!("3429a4778824b823")
                    },
                    workspace::Open::default(),
                ),
                MenuItem::action(
                    i18n::t!("88e721e23aa21644"),
                    zed_actions::OpenRecent::default(),
                ),
                MenuItem::action(
                    i18n::t!("0bc7e85def002a5b"),
                    zed_actions::OpenRemote::default(),
                ),
                MenuItem::separator(),
                MenuItem::action(i18n::t!("0fde73d53968b148"), workspace::AddFolderToProject),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("a3030bf8f16dc63c"),
                    workspace::Save { save_intent: None },
                ),
                MenuItem::action(i18n::t!("9016c463977d0128"), workspace::SaveAs),
                MenuItem::action(
                    i18n::t!("592b52ba3cd3cd5a"),
                    workspace::SaveAll { save_intent: None },
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("7951c0e8608ac003"),
                    workspace::CloseActiveItem {
                        save_intent: None,
                        close_pinned: true,
                    },
                ),
                MenuItem::action(i18n::t!("21cc26e21731d303"), workspace::CloseProject),
                MenuItem::action(i18n::t!("1ae6b0a0f8266382"), workspace::CloseWindow),
            ],
        },
        Menu {
            name: i18n::t!("051836569928a9f9").into(),
            disabled: false,
            items: vec![
                MenuItem::os_action(
                    i18n::t!("926a50b98ece2667"),
                    editor::actions::Undo,
                    OsAction::Undo,
                ),
                MenuItem::os_action(
                    i18n::t!("03717b6f10700f87"),
                    editor::actions::Redo,
                    OsAction::Redo,
                ),
                MenuItem::separator(),
                MenuItem::os_action(
                    i18n::t!("410a8e8a6bf253ac"),
                    editor::actions::Cut,
                    OsAction::Cut,
                ),
                MenuItem::os_action(
                    i18n::t!("63d90d977348ab1f"),
                    editor::actions::Copy,
                    OsAction::Copy,
                ),
                MenuItem::action(i18n::t!("3dcafd5ecb9eab87"), editor::actions::CopyAndTrim),
                MenuItem::os_action(
                    i18n::t!("33517926747180e6"),
                    editor::actions::Paste,
                    OsAction::Paste,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("3003318e729a860b"),
                    search::buffer_search::Deploy::find(),
                ),
                MenuItem::action(
                    i18n::t!("b53e6291b4c6c202"),
                    workspace::DeploySearch::default(),
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("af6b13ea4c4cdd98"),
                    editor::actions::ToggleComments::default(),
                ),
            ],
        },
        Menu {
            name: i18n::t!("c11330b85234f9c0").into(),
            disabled: false,
            items: vec![
                MenuItem::os_action(
                    i18n::t!("3a5040b68abf75f9"),
                    editor::actions::SelectAll,
                    OsAction::SelectAll,
                ),
                MenuItem::action(
                    i18n::t!("2f29bf111e8e87ec"),
                    editor::actions::SelectLargerSyntaxNode,
                ),
                MenuItem::action(
                    i18n::t!("830f665278d77343"),
                    editor::actions::SelectSmallerSyntaxNode,
                ),
                MenuItem::action(
                    i18n::t!("86a3f4bb535d7ab9"),
                    editor::actions::SelectNextSyntaxNode,
                ),
                MenuItem::action(
                    i18n::t!("657279064dd60d12"),
                    editor::actions::SelectPreviousSyntaxNode,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("7ca54c1a537fee96"),
                    editor::actions::AddSelectionAbove {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    i18n::t!("6df7d5399d5de448"),
                    editor::actions::AddSelectionBelow {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    i18n::t!("3bb9955c65f66ece"),
                    editor::actions::SelectNext {
                        replace_newest: false,
                    },
                ),
                MenuItem::action(
                    i18n::t!("5abb5cc8496cfe6e"),
                    editor::actions::SelectPrevious {
                        replace_newest: false,
                    },
                ),
                MenuItem::action(
                    i18n::t!("5c90d62786c9e801"),
                    editor::actions::SelectAllMatches,
                ),
                MenuItem::separator(),
                MenuItem::action(i18n::t!("6f075975b6b0d5ff"), editor::actions::MoveLineUp),
                MenuItem::action(i18n::t!("6e4b5b259d25bb8e"), editor::actions::MoveLineDown),
                MenuItem::action(
                    i18n::t!("706db341967b605a"),
                    editor::actions::DuplicateLineDown,
                ),
            ],
        },
        Menu {
            name: i18n::t!("1c5c067138704dda").into(),
            disabled: false,
            items: view_items,
        },
        Menu {
            name: i18n::t!("e72622fe470d04bc").into(),
            disabled: false,
            items: vec![
                MenuItem::action(i18n::t!("2d1d8c1e38956bea"), workspace::GoBack),
                MenuItem::action(i18n::t!("d681c6e2947ae79b"), workspace::GoForward),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("a446dbdddfc5e4a2"),
                    zed_actions::command_palette::Toggle,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("392ad09fd767e1ee"),
                    workspace::ToggleFileFinder::default(),
                ),
                // MenuItem::action("Go to Symbol in Project", project_symbols::Toggle),
                MenuItem::action(
                    i18n::t!("f0c3ccd0163fb006"),
                    zed_actions::outline::ToggleOutline,
                ),
                MenuItem::action(
                    i18n::t!("b1956639fb5f9d58"),
                    editor::actions::ToggleGoToLine,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("8e5ccbf336d8d04e"),
                    editor::actions::GoToDefinition::default(),
                ),
                MenuItem::action(
                    i18n::t!("6c9dbe92f1429674"),
                    editor::actions::GoToDeclaration::default(),
                ),
                MenuItem::action(
                    i18n::t!("7bb5e29bec31f254"),
                    editor::actions::GoToTypeDefinition::default(),
                ),
                MenuItem::action(
                    i18n::t!("48efab5e6cb10205"),
                    editor::actions::FindAllReferences::default(),
                ),
                MenuItem::action(
                    i18n::t!("44a7bfaf8fb2bcb0"),
                    call_hierarchy::ShowIncomingCalls,
                ),
                MenuItem::action(
                    i18n::t!("ee691bb6c8b2d2a1"),
                    call_hierarchy::ShowOutgoingCalls,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("ce4c5d8ca6ff4c9b"),
                    editor::actions::GoToDiagnostic::default(),
                ),
                MenuItem::action(
                    i18n::t!("e175d0cd4920a8d5"),
                    editor::actions::GoToPreviousDiagnostic::default(),
                ),
            ],
        },
        Menu {
            name: i18n::t!("75b269496f698fae").into(),
            disabled: false,
            items: vec![
                MenuItem::action(
                    i18n::t!("05df3be85291fac6"),
                    zed_actions::Spawn::ViaModal {
                        reveal_target: None,
                    },
                ),
                MenuItem::action(i18n::t!("ae2953c58d080c09"), debugger_ui::Start),
                MenuItem::separator(),
                MenuItem::action(i18n::t!("627f1124db9ac67c"), zed_actions::OpenProjectTasks),
                MenuItem::action(
                    i18n::t!("95df649f7b206102"),
                    zed_actions::OpenProjectDebugTasks,
                ),
                MenuItem::separator(),
                MenuItem::action(i18n::t!("7c9691192f1b7340"), debugger_ui::Continue),
                MenuItem::action(i18n::t!("2956fcdee87510db"), debugger_ui::StepOver),
                MenuItem::action(i18n::t!("6bb582c584ad4cce"), debugger_ui::StepInto),
                MenuItem::action(i18n::t!("ddf2c8d621d92ca1"), debugger_ui::StepOut),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("78dfe7874447bc5a"),
                    editor::actions::ToggleBreakpoint,
                ),
                MenuItem::action(
                    i18n::t!("cbd54b0982166d8c"),
                    editor::actions::EditLogBreakpoint,
                ),
                MenuItem::action(
                    i18n::t!("58aafe2d3cd5672f"),
                    debugger_ui::ClearAllBreakpoints,
                ),
            ],
        },
        Menu {
            name: i18n::t!("9efe01f647d67d91").into(),
            disabled: false,
            items: vec![
                MenuItem::action(i18n::t!("ac29e57a46f41c30"), super::Minimize),
                MenuItem::action(i18n::t!("b762aec769a35fff"), super::Zoom),
                MenuItem::separator(),
            ],
        },
        Menu {
            name: i18n::t!("a57cfcb8428da408").into(),
            disabled: false,
            items: vec![
                MenuItem::action(
                    i18n::t!("04326fd79f7525d8"),
                    auto_update_ui::ViewReleaseNotesLocally,
                ),
                MenuItem::action(i18n::t!("dc63eaa94782b519"), zed_actions::OpenTelemetryLog),
                MenuItem::action(i18n::t!("48f7b5e2fabd2ba5"), zed_actions::OpenLicenses),
                MenuItem::action(i18n::t!("ce41742780a18995"), onboarding::ShowWelcome),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("10a0e7d982e0bf39"),
                    zed_actions::feedback::FileBugReport,
                ),
                MenuItem::action(
                    i18n::t!("a538ff61cc388c59"),
                    zed_actions::feedback::RequestFeature,
                ),
                MenuItem::action(
                    i18n::t!("5d161b2a295a8e31"),
                    zed_actions::feedback::EmailZed,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("2687ccdbb1d2288a"),
                    super::OpenBrowser {
                        url: "https://zed.dev/docs".into(),
                    },
                ),
                MenuItem::action(i18n::t!("0ac649a1ed2f603b"), feedback::OpenZedRepo),
                MenuItem::action(
                    "Zed Twitter",
                    super::OpenBrowser {
                        url: "https://twitter.com/zeddotdev".into(),
                    },
                ),
                MenuItem::action(
                    i18n::t!("7f6177618c2af828"),
                    super::OpenBrowser {
                        url: "https://zed.dev/jobs".into(),
                    },
                ),
            ],
        },
    ]
}
