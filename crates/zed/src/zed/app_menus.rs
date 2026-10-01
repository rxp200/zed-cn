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
        MenuItem::action("重置所有缩放", zed_actions::ResetAllZoom { persist: false }),
        MenuItem::separator(),
        MenuItem::action("切换左侧面板", workspace::ToggleLeftDock),
        MenuItem::action("切换右侧面板", workspace::ToggleRightDock),
        MenuItem::action("切换底部面板", workspace::ToggleBottomDock),
        MenuItem::action("切换所有面板", workspace::ToggleAllDocks),
        MenuItem::submenu(Menu {
            name: i18n::t!("9099a016b2c604f7").into(),
            disabled: false,
            items: vec![
                MenuItem::action("向上分割", workspace::SplitUp::default()),
                MenuItem::action("向下分割", workspace::SplitDown::default()),
                MenuItem::action("向左分割", workspace::SplitLeft::default()),
                MenuItem::action("向右分割", workspace::SplitRight::default()),
            ],
        }),
        MenuItem::separator(),
        MenuItem::action("项目面板", project_panel::ToggleFocus),
        MenuItem::action("大纲面板", outline_panel::ToggleFocus),
        MenuItem::action("协作面板", collab_panel::ToggleFocus),
        MenuItem::action("终端面板", terminal_panel::Toggle),
        MenuItem::action("调试器面板", debug_panel::ToggleFocus),
    ];

    if !DisableAiSettings::get_global(cx).disable_ai {
        view_items.push(MenuItem::action("Agent 面板", assistant::ToggleFocus));
    }

    view_items.extend([
        MenuItem::action("Git 面板", git_panel::ToggleFocus),
        MenuItem::separator(),
        MenuItem::action("诊断", diagnostics::Deploy),
        MenuItem::separator(),
    ]);

    if ReleaseChannel::try_global(cx) == Some(ReleaseChannel::Dev) {
        view_items.push(MenuItem::action("切换 GPUI 调试器", dev::ToggleInspector));
        view_items.push(MenuItem::separator());
    }

    vec![
        Menu {
            name: "Zed".into(),
            disabled: false,
            items: vec![
                MenuItem::action("关于 Zed", zed_actions::About),
                MenuItem::action("检查更新", auto_update::Check),
                MenuItem::separator(),
                MenuItem::submenu(Menu::new(i18n::t!("df3d58c7d84b85f2")).items([
                    MenuItem::action("打开设置", zed_actions::OpenSettings),
                    MenuItem::action("打开设置文件", super::OpenSettingsFile),
                    MenuItem::action("打开项目设置", zed_actions::OpenProjectSettings),
                    MenuItem::action("打开项目设置文件", super::OpenProjectSettingsFile),
                    MenuItem::action("打开默认设置", super::OpenDefaultSettings),
                    MenuItem::separator(),
                    MenuItem::action("打开键位映射", zed_actions::OpenKeymap),
                    MenuItem::action("打开键位映射文件", zed_actions::OpenKeymapFile),
                    MenuItem::action("打开默认键位绑定", zed_actions::OpenDefaultKeymap),
                    MenuItem::separator(),
                    MenuItem::action("选择主题…", zed_actions::theme_selector::Toggle::default()),
                    MenuItem::action(
                        i18n::t!("b93536f1a11bcf35"),
                        zed_actions::icon_theme_selector::Toggle::default(),
                    ),
                ])),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::os_submenu(i18n::t!("ec309ab207ef7fa3"), gpui::SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("扩展", zed_actions::Extensions::default()),
                #[cfg(not(target_os = "windows"))]
                MenuItem::action("安装 CLI", install_cli::InstallCliBinary),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::action("隐藏 Zed", super::Hide),
                #[cfg(target_os = "macos")]
                MenuItem::action("隐藏其他", super::HideOthers),
                #[cfg(target_os = "macos")]
                MenuItem::action("显示全部", super::ShowAll),
                MenuItem::separator(),
                MenuItem::action("退出 Zed", Quit),
            ],
        },
        Menu {
            name: i18n::t!("39932f24fe11a6ba").into(),
            disabled: false,
            items: vec![
                MenuItem::action("新建", workspace::NewFile),
                MenuItem::action("新建窗口", workspace::NewWindow),
                MenuItem::separator(),
                #[cfg(not(target_os = "macos"))]
                MenuItem::action("打开文件…", workspace::OpenFiles),
                MenuItem::action(
                    if cfg!(not(target_os = "macos")) {
                        i18n::t!("15110e31656a5995")
                    } else {
                        i18n::t!("3429a4778824b823")
                    },
                    workspace::Open::default(),
                ),
                MenuItem::action("最近打开…", zed_actions::OpenRecent::default()),
                MenuItem::action("打开远程…", zed_actions::OpenRemote::default()),
                MenuItem::separator(),
                MenuItem::action("添加文件夹到项目…", workspace::AddFolderToProject),
                MenuItem::separator(),
                MenuItem::action("保存", workspace::Save { save_intent: None }),
                MenuItem::action("另存为…", workspace::SaveAs),
                MenuItem::action("全部保存", workspace::SaveAll { save_intent: None }),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("7951c0e8608ac003"),
                    workspace::CloseActiveItem {
                        save_intent: None,
                        close_pinned: true,
                    },
                ),
                MenuItem::action("关闭项目", workspace::CloseProject),
                MenuItem::action("关闭窗口", workspace::CloseWindow),
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
                MenuItem::action("复制并修剪", editor::actions::CopyAndTrim),
                MenuItem::os_action(
                    i18n::t!("33517926747180e6"),
                    editor::actions::Paste,
                    OsAction::Paste,
                ),
                MenuItem::separator(),
                MenuItem::action("查找", search::buffer_search::Deploy::find()),
                MenuItem::action("在项目中查找", workspace::DeploySearch::default()),
                MenuItem::separator(),
                MenuItem::action("切换行注释", editor::actions::ToggleComments::default()),
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
                MenuItem::action("扩大选择", editor::actions::SelectLargerSyntaxNode),
                MenuItem::action("缩小选择", editor::actions::SelectSmallerSyntaxNode),
                MenuItem::action("选择下一个兄弟节点", editor::actions::SelectNextSyntaxNode),
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
                MenuItem::action("选择所有出现", editor::actions::SelectAllMatches),
                MenuItem::separator(),
                MenuItem::action("上移行", editor::actions::MoveLineUp),
                MenuItem::action("下移行", editor::actions::MoveLineDown),
                MenuItem::action("复制选区", editor::actions::DuplicateLineDown),
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
                MenuItem::action("后退", workspace::GoBack),
                MenuItem::action("前进", workspace::GoForward),
                MenuItem::separator(),
                MenuItem::action("命令面板…", zed_actions::command_palette::Toggle),
                MenuItem::separator(),
                MenuItem::action("转到文件…", workspace::ToggleFileFinder::default()),
                // MenuItem::action("Go to Symbol in Project", project_symbols::Toggle),
                MenuItem::action("转到编辑器中的符号…", zed_actions::outline::ToggleOutline),
                MenuItem::action("转到行/列…", editor::actions::ToggleGoToLine),
                MenuItem::separator(),
                MenuItem::action("转到定义", editor::actions::GoToDefinition::default()),
                MenuItem::action("转到声明", editor::actions::GoToDeclaration::default()),
                MenuItem::action(
                    i18n::t!("7bb5e29bec31f254"),
                    editor::actions::GoToTypeDefinition::default(),
                ),
                MenuItem::action(
                    i18n::t!("48efab5e6cb10205"),
                    editor::actions::FindAllReferences::default(),
                ),
                MenuItem::action("Show Incoming Calls", call_hierarchy::ShowIncomingCalls),
                MenuItem::action("Show Outgoing Calls", call_hierarchy::ShowOutgoingCalls),
                MenuItem::separator(),
                MenuItem::action("下一个问题", editor::actions::GoToDiagnostic::default()),
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
                MenuItem::action("启动调试器", debugger_ui::Start),
                MenuItem::separator(),
                MenuItem::action("编辑 tasks.json…", zed_actions::OpenProjectTasks),
                MenuItem::action("编辑 debug.json…", zed_actions::OpenProjectDebugTasks),
                MenuItem::separator(),
                MenuItem::action("继续", debugger_ui::Continue),
                MenuItem::action("单步跳过", debugger_ui::StepOver),
                MenuItem::action("单步进入", debugger_ui::StepInto),
                MenuItem::action("单步退出", debugger_ui::StepOut),
                MenuItem::separator(),
                MenuItem::action("切换断点", editor::actions::ToggleBreakpoint),
                MenuItem::action("编辑断点", editor::actions::EditLogBreakpoint),
                MenuItem::action("清除所有断点", debugger_ui::ClearAllBreakpoints),
            ],
        },
        Menu {
            name: i18n::t!("9efe01f647d67d91").into(),
            disabled: false,
            items: vec![
                MenuItem::action("最小化", super::Minimize),
                MenuItem::action("缩放", super::Zoom),
                MenuItem::separator(),
            ],
        },
        Menu {
            name: i18n::t!("a57cfcb8428da408").into(),
            disabled: false,
            items: vec![
                MenuItem::action("查看本地发布说明", auto_update_ui::ViewReleaseNotesLocally),
                MenuItem::action("查看遥测", zed_actions::OpenTelemetryLog),
                MenuItem::action("查看依赖许可证", zed_actions::OpenLicenses),
                MenuItem::action("显示欢迎页面", onboarding::ShowWelcome),
                MenuItem::separator(),
                MenuItem::action("提交 Bug 报告…", zed_actions::feedback::FileBugReport),
                MenuItem::action("请求功能…", zed_actions::feedback::RequestFeature),
                MenuItem::action("联系我们…", zed_actions::feedback::EmailZed),
                MenuItem::separator(),
                MenuItem::action(
                    i18n::t!("2687ccdbb1d2288a"),
                    super::OpenBrowser {
                        url: "https://zed.dev/docs".into(),
                    },
                ),
                MenuItem::action("Zed 仓库", feedback::OpenZedRepo),
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
