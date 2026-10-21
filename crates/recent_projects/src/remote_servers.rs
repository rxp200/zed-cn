use crate::{
    remote_connections::{
        Connection, RemoteConnectionModal, RemoteConnectionPrompt, RemoteSettings, SshConnection,
        SshConnectionHeader, connect, determine_paths_with_positions, open_remote_project,
    },
    ssh_config::parse_ssh_config_hosts,
};
mod filter;

use dev_container::{
    DevContainerConfig, DevContainerContext, find_devcontainer_configs,
    start_dev_container_with_config,
};
use editor::Editor;
use extension_host::ExtensionStore;
use filter::{FilterData, FilteredServer};
use futures::{FutureExt, StreamExt as _, channel::oneshot, future::Shared};
use gpui::{
    Action, AnyElement, App, ClickEvent, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, FontWeight, PathPromptOptions,
    PromptLevel, Rems, Subscription, Task, TaskExt, WeakEntity, Window,
};
use log::{debug, info};
use open_path_prompt::OpenPathDelegate;
use paths::{global_ssh_config_file, user_ssh_config_file};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use project::{Fs, Project};
use remote::{
    RemoteClient, RemoteConnectionOptions, SshConnectionOptions, WslConnectionOptions,
    remote_client::ConnectionIdentifier,
};
use settings::{
    JumpHost, RemoteProject, RemoteSettingsContent, Settings as _, SettingsStore, SshAuthMethod,
    update_settings_file, watch_config_file,
};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use ui::{
    Checkbox, CommonAnimationExt, ElevationIndex, HighlightedLabel, IconButtonShape, KeyBinding,
    ListItem, ListSeparator, ModalHeader, Navigable, NavigableEntry, TintColor, Tooltip,
    prelude::*,
};
use util::{
    ResultExt,
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
};
use workspace::{
    AppState, DismissDecision, ModalView, MultiWorkspace, OpenLog, OpenOptions, Toast, Workspace,
    notifications::{DetachAndPromptErr, NotificationId},
    open_remote_project_with_existing_connection,
};

struct ManagedSshKeyManagementToast;

/// Max number of recent project locations kept per remote server.
pub(crate) const MAX_RECENT_PROJECTS_PER_SERVER: usize = 5;

/// Records `project` as the most recently used location on a server,
/// deduplicating by value and evicting the least recently used entries once
/// the per-server limit is exceeded. The list is ordered most recently used
/// first.
pub(crate) fn record_remote_project(projects: &mut Vec<RemoteProject>, project: RemoteProject) {
    projects.retain(|existing| existing != &project);
    projects.insert(0, project);
    projects.truncate(MAX_RECENT_PROJECTS_PER_SERVER);
}

pub struct RemoteServerProjects {
    mode: Mode,
    focus_handle: FocusHandle,
    default_picker: Entity<Picker<RemoteServerPickerDelegate>>,
    workspace: WeakEntity<Workspace>,
    retained_connections: Vec<Entity<RemoteClient>>,
    ssh_config_updates: Task<()>,
    ssh_config_servers: BTreeSet<SharedString>,
    create_new_window: bool,
    dev_container_picker: Option<Entity<Picker<DevContainerPickerDelegate>>>,
    _subscriptions: Vec<Subscription>,
    allow_dismissal: bool,
}

struct CreateRemoteServer {
    address_editor: Entity<Editor>,
    address_error: Option<SharedString>,
    ssh_prompt: Option<Entity<RemoteConnectionPrompt>>,
    _creating: Option<Task<Option<()>>>,
}

impl CreateRemoteServer {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let address_editor = cx.new(|cx| Editor::single_line(window, cx));
        address_editor.update(cx, |this, cx| {
            this.focus_handle(cx).focus(window, cx);
        });
        Self {
            address_editor,
            address_error: None,
            ssh_prompt: None,
            _creating: None,
        }
    }
}

fn form_editor(
    placeholder: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    let editor = cx.new(|cx| Editor::single_line(window, cx));
    editor.update(cx, |editor, cx| {
        editor.set_placeholder_text(placeholder, window, cx);
    });
    editor
}

fn trimmed_text(text: String) -> Option<String> {
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn ssh_connection_from_options(options: &remote::SshConnectionOptions) -> SshConnection {
    SshConnection {
        host: options.host.to_string(),
        username: options.username.clone(),
        port: options.port,
        projects: Vec::new(),
        nickname: options.nickname.clone(),
        args: options.args.clone().unwrap_or_default(),
        upload_binary_over_ssh: None,
        port_forwards: options.port_forwards.clone(),
        connection_timeout: options.connection_timeout,
        auth_method: options.auth_method,
        ssh_key_id: options.ssh_key_id.clone(),
        jump_hosts: options.jump_hosts.clone(),
        proxy: options.proxy.clone(),
        proxy_disabled: options.proxy_disabled,
    }
}

fn auth_method_label(method: SshAuthMethod) -> &'static str {
    match method {
        SshAuthMethod::Password => i18n::t!("a621ab606db2a11f"),
        SshAuthMethod::Key => i18n::t!("f67bca8f42bcf073"),
        SshAuthMethod::Interactive => i18n::t!("9fc30835877e4450"),
    }
}

fn jump_host_label(jump_host: &JumpHost) -> String {
    let mut label = String::new();
    if let Some(username) = &jump_host.username {
        label.push_str(username);
        label.push('@');
    }
    label.push_str(&jump_host.host);
    if let Some(port) = jump_host.port {
        label.push(':');
        label.push_str(&port.to_string());
    }
    label
}

/// Wraps a form control in the bordered, recessed surface that makes it read
/// as an input rather than as text sitting on the modal background.
///
/// The single-line `Editor` paints no background of its own, so without this
/// wrapper the field is indistinguishable from the surrounding label text.
fn form_input_surface(editor: &Entity<Editor>, invalid: bool, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let status = cx.theme().status();
    let resting_border = if invalid {
        status.error_border
    } else {
        colors.border
    };
    let focused_border = if invalid {
        status.error_border
    } else {
        colors.border_focused
    };
    let focus_handle = editor.read(cx).focus_handle(cx);

    h_flex()
        .w_full()
        .h(rems_from_px(32_f32))
        .px_2()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(resting_border)
        .bg(colors.editor_background)
        .track_focus(&focus_handle)
        .focus(move |style| style.border_color(focused_border))
        .child(div().flex_1().min_w_0().child(editor.clone()))
        .into_any_element()
}

/// A small pill that marks a field as required.
fn form_badge(label: impl Into<SharedString>, color: Color, cx: &App) -> AnyElement {
    let tint = color.color(cx);
    div()
        .px_1()
        .rounded_sm()
        .bg(tint.opacity(0.14))
        .child(Label::new(label).size(LabelSize::XSmall).color(color))
        .into_any_element()
}

/// The label above a form control, optionally carrying a trailing note.
fn form_label(label: impl Into<SharedString>, note: Option<SharedString>) -> AnyElement {
    h_flex()
        .w_full()
        .justify_between()
        .gap_2()
        .child(Label::new(label))
        .when_some(note, |this, note| {
            this.child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted))
        })
        .into_any_element()
}

/// A section heading that separates the connection fields from the advanced
/// ones, with an optional note on the right.
fn form_section_head(title: &'static str, note: Option<SharedString>, cx: &App) -> AnyElement {
    h_flex()
        .w_full()
        .justify_between()
        .gap_2()
        .pb_1()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Label::new(title)
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .weight(FontWeight::SEMIBOLD),
        )
        .when_some(note, |this, note| {
            this.child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted))
        })
        .into_any_element()
}

/// One mutually exclusive choice in a segmented control.
fn form_segment(
    id: SharedString,
    label: &'static str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    _cx: &App,
) -> AnyElement {
    // 28px segments inside a container with 2px padding fill the same 32px
    // control row as the inputs beside them.
    Button::new(id, label)
        .style(ButtonStyle::Subtle)
        .size(ButtonSize::Medium)
        .label_size(LabelSize::Small)
        .toggle_state(selected)
        .selected_style(ButtonStyle::Filled)
        .layer(ElevationIndex::Surface)
        .color(if selected {
            Color::Default
        } else {
            Color::Muted
        })
        .on_click(move |event, window, cx| on_click(event, window, cx))
        .into_any_element()
}

/// The validated contents of [`CreateRemoteServerForm`] at submission time.
struct RemoteServerFormDraft {
    nickname: Option<String>,
    host: String,
    port: Option<u16>,
    username: Option<String>,
    auth_method: SshAuthMethod,
    key_file: Option<PathBuf>,
    existing_key_id: Option<String>,
    jump_hosts: Vec<JumpHost>,
    proxy: Option<String>,
    proxy_disabled: bool,
    connection_timeout: Option<u16>,
}

/// The fill-in-the-blanks SSH server form.
///
/// The one-line `ssh` command entry and this form both persist the same
/// [`SshConnection`], so the list view, connection flow and remote-server
/// deployment behave identically afterwards.
struct CreateRemoteServerForm {
    nickname_editor: Entity<Editor>,
    host_editor: Entity<Editor>,
    port_editor: Entity<Editor>,
    username_editor: Entity<Editor>,
    proxy_editor: Entity<Editor>,
    timeout_editor: Entity<Editor>,
    auth_method: SshAuthMethod,
    /// A client-local key file that is imported when the form is submitted.
    key_file: Option<PathBuf>,
    /// A key that was already imported for this form, reused on retry.
    key_id: Option<String>,
    jump_hosts: Vec<JumpHost>,
    jump_host_picker: Option<Entity<Picker<JumpHostPickerDelegate>>>,
    proxy_disabled: bool,
    /// Whether the advanced section is expanded. Only affects the current
    /// form; nothing here is persisted.
    advanced_open: bool,
    /// Inline validation message for the host field, so the error appears next
    /// to the field instead of only in the footer.
    host_error: Option<SharedString>,
    /// Settings index of the entry this form already saved, so a retry updates
    /// that entry instead of adding a duplicate.
    saved_index: Option<SshServerIndex>,
    error: Option<SharedString>,
    ssh_prompt: Option<Entity<RemoteConnectionPrompt>>,
    _creating: Option<Task<Option<()>>>,
}

impl CreateRemoteServerForm {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let nickname_editor = form_editor("", window, cx);
        let host_editor = form_editor(i18n::t!("3fe29ba5aa6ffd60"), window, cx);
        let port_editor = form_editor(i18n::t!("68450ee4ee5c3d6b"), window, cx);
        let username_editor = form_editor(i18n::t!("c6e8dbb3a8be616c"), window, cx);
        let proxy_editor = form_editor(i18n::t!("41c7caab02b48d59"), window, cx);
        let timeout_editor = form_editor("", window, cx);
        host_editor.update(cx, |editor, cx| {
            editor.focus_handle(cx).focus(window, cx);
        });

        Self {
            nickname_editor,
            host_editor,
            port_editor,
            username_editor,
            proxy_editor,
            timeout_editor,
            auth_method: SshAuthMethod::Password,
            key_file: None,
            key_id: None,
            jump_hosts: Vec::new(),
            jump_host_picker: None,
            proxy_disabled: false,
            advanced_open: false,
            host_error: None,
            saved_index: None,
            error: None,
            ssh_prompt: None,
            _creating: None,
        }
    }

    /// A summary of the non-default advanced values, shown while the section
    /// is collapsed so nothing silently disappears from view.
    fn advanced_summary(&self, cx: &App) -> Option<SharedString> {
        let mut parts = Vec::new();
        if !self.jump_hosts.is_empty() {
            parts.push(format!(
                "{} {}",
                i18n::t!("1031cfcb802afd94"),
                self.jump_hosts.len()
            ));
        }
        if trimmed_text(self.proxy_editor.read(cx).text(cx)).is_some() {
            parts.push(i18n::t!("48b970b568a7f8f9").to_string());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · ").into())
        }
    }

    /// The `ssh` invocation this form will use, so the jump-host order and the
    /// proxy precedence become visible before connecting.
    fn ssh_preview(&self, cx: &App) -> Option<SharedString> {
        let host = self.host_editor.read(cx).text(cx);
        let host = host.trim();
        if host.is_empty() {
            return None;
        }

        let username = trimmed_text(self.username_editor.read(cx).text(cx));
        let mut preview = String::from("ssh ");
        if let Some(username) = username {
            preview.push_str(&username);
            preview.push('@');
        }
        preview.push_str(host);

        if let Some(port) = trimmed_text(self.port_editor.read(cx).text(cx)) {
            preview.push_str(" -p ");
            preview.push_str(&port);
        }

        let proxy = trimmed_text(self.proxy_editor.read(cx).text(cx));
        if let Some(proxy) = proxy
            && !self.proxy_disabled
        {
            preview.push_str(" (");
            preview.push_str(&proxy);
            preview.push(')');
        }

        if !self.jump_hosts.is_empty() {
            preview.push_str(" (");
            preview.push_str(
                &self
                    .jump_hosts
                    .iter()
                    .map(jump_host_label)
                    .collect::<Vec<_>>()
                    .join(" → "),
            );
            preview.push(')');
        }

        Some(preview.into())
    }
}

struct JumpHostCandidate {
    label: SharedString,
    host: String,
    username: Option<String>,
    port: Option<u16>,
}

impl JumpHostCandidate {
    fn jump_host(&self) -> JumpHost {
        JumpHost {
            host: self.host.clone(),
            username: self.username.clone(),
            port: self.port,
        }
    }

    fn detail(&self) -> String {
        let mut detail = String::new();
        if let Some(username) = &self.username {
            detail.push_str(username);
            detail.push('@');
        }
        detail.push_str(&self.host);
        if let Some(port) = self.port {
            detail.push(':');
            detail.push_str(&port.to_string());
        }
        detail
    }
}

struct JumpHostPickerDelegate {
    parent_form: WeakEntity<RemoteServerProjects>,
    candidates: Vec<JumpHostCandidate>,
    matching: Vec<usize>,
    selected_index: usize,
}

impl JumpHostPickerDelegate {
    fn new(
        candidates: Vec<JumpHostCandidate>,
        parent_form: WeakEntity<RemoteServerProjects>,
    ) -> Self {
        let matching = (0..candidates.len()).collect();
        Self {
            parent_form,
            candidates,
            matching,
            selected_index: 0,
        }
    }
}

impl PickerDelegate for JumpHostPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "remote jump host picker"
    }

    fn match_count(&self) -> usize {
        self.matching.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        i18n::t!("16dde1c78091301a").into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matching = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                query.is_empty()
                    || candidate.label.to_lowercase().contains(&query)
                    || candidate.detail().to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matching.len().saturating_sub(1));
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(candidate) = self
            .matching
            .get(self.selected_index)
            .and_then(|index| self.candidates.get(*index))
        else {
            return;
        };
        let jump_host = candidate.jump_host();
        self.parent_form
            .update(cx, |modal, cx| {
                if let Mode::CreateRemoteServerForm(form) = &mut modal.mode {
                    form.jump_hosts.push(jump_host);
                    form.jump_host_picker = None;
                }
                cx.notify();
            })
            .log_err();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.parent_form
            .update(cx, |modal, cx| {
                if let Mode::CreateRemoteServerForm(form) = &mut modal.mode {
                    form.jump_host_picker = None;
                }
                cx.notify();
            })
            .log_err();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let candidate_index = *self.matching.get(index)?;
        let candidate = self.candidates.get(candidate_index)?;
        Some(
            ListItem::new(SharedString::from(format!("jump-host-{candidate_index}")))
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::Server).color(Color::Muted))
                .child(
                    v_flex()
                        .child(Label::new(candidate.label.clone()))
                        .child(
                            Label::new(candidate.detail())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        if !self.candidates.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .p_2()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new(i18n::t!("7285fbff8b0f4a6e"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DevContainerCreationProgress {
    SelectingConfig,
    Creating,
    Error(String),
}

#[derive(Clone)]
struct CreateRemoteDevContainer {
    view_logs_entry: NavigableEntry,
    back_entry: NavigableEntry,
    progress: DevContainerCreationProgress,
}

impl CreateRemoteDevContainer {
    fn new(progress: DevContainerCreationProgress, cx: &mut Context<RemoteServerProjects>) -> Self {
        let view_logs_entry = NavigableEntry::focusable(cx);
        let back_entry = NavigableEntry::focusable(cx);
        Self {
            view_logs_entry,
            back_entry,
            progress,
        }
    }
}

#[cfg(target_os = "windows")]
struct AddWslDistro {
    picker: Entity<Picker<crate::wsl_picker::WslPickerDelegate>>,
    connection_prompt: Option<Entity<RemoteConnectionPrompt>>,
    _creating: Option<Task<()>>,
}

#[cfg(target_os = "windows")]
impl AddWslDistro {
    fn new(window: &mut Window, cx: &mut Context<RemoteServerProjects>) -> Self {
        use crate::wsl_picker::{WslDistroSelected, WslPickerDelegate, WslPickerDismissed};

        let delegate = WslPickerDelegate::new();
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());

        cx.subscribe_in(
            &picker,
            window,
            |this, _, _: &WslDistroSelected, window, cx| {
                this.confirm(&menu::Confirm, window, cx);
            },
        )
        .detach();

        cx.subscribe_in(
            &picker,
            window,
            |this, _, _: &WslPickerDismissed, window, cx| {
                this.cancel(&menu::Cancel, window, cx);
            },
        )
        .detach();

        AddWslDistro {
            picker,
            connection_prompt: None,
            _creating: None,
        }
    }
}

enum ProjectPickerData {
    Ssh {
        connection_string: SharedString,
        nickname: Option<SharedString>,
    },
    Wsl {
        distro_name: SharedString,
    },
}

struct ProjectPicker {
    data: ProjectPickerData,
    picker: Entity<Picker<OpenPathDelegate>>,
    _path_task: Shared<Task<Option<()>>>,
}

struct EditNicknameState {
    index: SshServerIndex,
    editor: Entity<Editor>,
}

struct DevContainerPickerDelegate {
    selected_index: usize,
    candidates: Vec<DevContainerConfig>,
    matching_candidates: Vec<DevContainerConfig>,
    parent_modal: WeakEntity<RemoteServerProjects>,
}
impl DevContainerPickerDelegate {
    fn new(
        candidates: Vec<DevContainerConfig>,
        parent_modal: WeakEntity<RemoteServerProjects>,
    ) -> Self {
        Self {
            selected_index: 0,
            matching_candidates: candidates.clone(),
            candidates,
            parent_modal,
        }
    }
}

impl PickerDelegate for DevContainerPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "remote dev container picker"
    }

    fn match_count(&self) -> usize {
        self.matching_candidates.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        i18n::t!("88a364330a40384d").into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query_lower = query.to_lowercase();
        self.matching_candidates = self
            .candidates
            .iter()
            .filter(|c| {
                c.name.to_lowercase().contains(&query_lower)
                    || c.config_path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query_lower)
            })
            .cloned()
            .collect();

        self.selected_index = std::cmp::min(
            self.selected_index,
            self.matching_candidates.len().saturating_sub(1),
        );

        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let selected_config = self.matching_candidates.get(self.selected_index).cloned();
        self.parent_modal
            .update(cx, move |modal, cx| {
                if secondary {
                    modal.edit_in_dev_container_json(selected_config.clone(), window, cx);
                } else if let Some((app_state, context)) = modal
                    .workspace
                    .read_with(cx, |workspace, cx| {
                        let app_state = workspace.app_state().clone();
                        let context = DevContainerContext::from_workspace(workspace, cx)?;
                        Some((app_state, context))
                    })
                    .ok()
                    .flatten()
                {
                    modal.open_dev_container(selected_config, app_state, context, window, cx);
                    modal.view_in_progress_dev_container(window, cx);
                } else {
                    log::error!("No active project directory for Dev Container");
                }
            })
            .ok();
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.parent_modal
            .update(cx, |modal, cx| {
                modal.cancel(&menu::Cancel, window, cx);
            })
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate = self.matching_candidates.get(ix)?;
        let config_path = candidate.config_path.display().to_string();
        Some(
            ListItem::new(SharedString::from(format!("li-devcontainer-config-{}", ix)))
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::FileToml).color(Color::Muted))
                .child(
                    v_flex().child(Label::new(candidate.name.clone())).child(
                        Label::new(config_path)
                            .size(ui::LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .justify_start()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("run-action", i18n::t!("7a11dde8106f4719"))
                        .key_binding(
                            KeyBinding::for_action(&menu::Confirm, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                        }),
                )
                .child(
                    Button::new("run-action-secondary", i18n::t!("22079da1a18a4968"))
                        .key_binding(
                            KeyBinding::for_action(&menu::SecondaryConfirm, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                        }),
                )
                .into_any_element(),
        )
    }
}

impl EditNicknameState {
    fn new(index: SshServerIndex, window: &mut Window, cx: &mut App) -> Self {
        let this = Self {
            index,
            editor: cx.new(|cx| Editor::single_line(window, cx)),
        };
        let starting_text = RemoteSettings::get_global(cx)
            .ssh_connections()
            .nth(index.0)
            .and_then(|state| state.nickname)
            .filter(|text| !text.is_empty());
        this.editor.update(cx, |this, cx| {
            this.set_placeholder_text(i18n::t!("bc7d2a6beb35891e"), window, cx);
            if let Some(starting_text) = starting_text {
                this.set_text(starting_text, window, cx);
            }
        });
        this.editor.focus_handle(cx).focus(window, cx);
        this
    }
}

impl Focusable for ProjectPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ProjectPicker {
    fn new(
        create_new_window: bool,
        index: ServerIndex,
        connection: RemoteConnectionOptions,
        project: Entity<Project>,
        home_dir: RemotePathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<RemoteServerProjects>,
    ) -> Entity<Self> {
        let (tx, rx) = oneshot::channel();
        let lister = project::DirectoryLister::Project(project.clone());
        let delegate = open_path_prompt::OpenPathDelegate::new(tx, lister, false, cx).show_hidden();

        let picker = cx.new(|cx| {
            let picker = Picker::uniform_list(delegate, window, cx).embedded();
            picker.set_query(&home_dir.to_string(), window, cx);
            picker
        });

        let data = match &connection {
            RemoteConnectionOptions::Ssh(connection) => ProjectPickerData::Ssh {
                connection_string: connection.connection_string().into(),
                nickname: connection.nickname.clone().map(|nick| nick.into()),
            },
            RemoteConnectionOptions::Wsl(connection) => ProjectPickerData::Wsl {
                distro_name: connection.distro_name.clone().into(),
            },
            RemoteConnectionOptions::Docker(_) => ProjectPickerData::Ssh {
                // Not implemented as a project picker at this time
                connection_string: "".into(),
                nickname: None,
            },
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(options) => ProjectPickerData::Ssh {
                connection_string: format!("mock-{}", options.id).into(),
                nickname: None,
            },
        };
        let _path_task = cx
            .spawn_in(window, {
                let workspace = workspace;
                async move |this, cx| {
                    let Ok(Some(paths)) = rx.await else {
                        workspace
                            .update_in(cx, |workspace, window, cx| {
                                let fs = workspace.project().read(cx).fs().clone();
                                let weak = cx.entity().downgrade();
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    RemoteServerProjects::new(
                                        create_new_window,
                                        fs,
                                        window,
                                        weak,
                                        cx,
                                    )
                                });
                            })
                            .log_err()?;
                        return None;
                    };

                    let app_state = workspace
                        .read_with(cx, |workspace, _| workspace.app_state().clone())
                        .ok()?;

                    let remote_connection = project.read_with(cx, |project, cx| {
                        project.remote_client()?.read(cx).connection()
                    })?;

                    let (paths, paths_with_positions) =
                        determine_paths_with_positions(&remote_connection, paths).await;

                    cx.update(|_, cx| {
                        let fs = app_state.fs.clone();
                        update_settings_file(fs, cx, {
                            let paths = paths
                                .iter()
                                .map(|path| path.to_string_lossy().into_owned())
                                .collect();
                            move |settings, _| match index {
                                ServerIndex::Ssh(index) => {
                                    if let Some(server) = settings
                                        .remote
                                        .ssh_connections
                                        .as_mut()
                                        .and_then(|connections| connections.get_mut(index.0))
                                    {
                                        record_remote_project(
                                            &mut server.projects,
                                            RemoteProject { paths },
                                        );
                                    };
                                }
                                ServerIndex::Wsl(index) => {
                                    if let Some(server) = settings
                                        .remote
                                        .wsl_connections
                                        .as_mut()
                                        .and_then(|connections| connections.get_mut(index.0))
                                    {
                                        record_remote_project(
                                            &mut server.projects,
                                            RemoteProject { paths },
                                        );
                                    };
                                }
                            }
                        });
                    })
                    .log_err();

                    let window = if create_new_window {
                        let options = cx
                            .update(|_, cx| (app_state.build_window_options)(None, cx))
                            .log_err()?;
                        cx.open_window(options, |window, cx| {
                            let workspace = cx.new(|cx| {
                                telemetry::event!("SSH Project Created");
                                Workspace::new(None, project.clone(), app_state.clone(), window, cx)
                            });
                            cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                        })
                        .log_err()
                    } else {
                        cx.window_handle().downcast::<MultiWorkspace>()
                    }?;

                    let items = open_remote_project_with_existing_connection(
                        connection, project, paths, app_state, window, None, None, cx,
                    )
                    .await
                    .log_err()
                    .map(|(_workspace, items)| items);

                    if let Some(items) = items {
                        for (item, path) in items.into_iter().zip(paths_with_positions) {
                            let Some(item) = item else {
                                continue;
                            };
                            let Some(row) = path.row else {
                                continue;
                            };
                            if let Some(active_editor) = item.downcast::<Editor>() {
                                window
                                    .update(cx, |_, window, cx| {
                                        active_editor.update(cx, |editor, cx| {
                                            let row = row.saturating_sub(1);
                                            let col = path.column.unwrap_or(0).saturating_sub(1);
                                            let Some(buffer) =
                                                editor.buffer().read(cx).as_singleton()
                                            else {
                                                return;
                                            };
                                            let buffer_snapshot = buffer.read(cx).snapshot();
                                            let point =
                                                buffer_snapshot.point_from_external_input(row, col);
                                            editor.go_to_singleton_buffer_point(point, window, cx);
                                        });
                                    })
                                    .ok();
                            }
                        }
                    }

                    this.update(cx, |_, cx| {
                        cx.emit(DismissEvent);
                    })
                    .ok();
                    Some(())
                }
            })
            .shared();
        cx.new(|_| Self {
            _path_task,
            picker,
            data,
        })
    }
}

impl gpui::Render for ProjectPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .child(match &self.data {
                ProjectPickerData::Ssh {
                    connection_string,
                    nickname,
                } => SshConnectionHeader {
                    connection_string: connection_string.clone(),
                    paths: Default::default(),
                    nickname: nickname.clone(),
                    is_wsl: false,
                    is_devcontainer: false,
                }
                .render(window, cx),
                ProjectPickerData::Wsl { distro_name } => SshConnectionHeader {
                    connection_string: distro_name.clone(),
                    paths: Default::default(),
                    nickname: None,
                    is_wsl: true,
                    is_devcontainer: false,
                }
                .render(window, cx),
            })
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.picker.clone()),
            )
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct SshServerIndex(usize);
impl std::fmt::Display for SshServerIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct WslServerIndex(usize);
impl std::fmt::Display for WslServerIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
enum ServerIndex {
    Ssh(SshServerIndex),
    Wsl(WslServerIndex),
}
impl From<SshServerIndex> for ServerIndex {
    fn from(index: SshServerIndex) -> Self {
        Self::Ssh(index)
    }
}
impl From<WslServerIndex> for ServerIndex {
    fn from(index: WslServerIndex) -> Self {
        Self::Wsl(index)
    }
}

#[derive(Clone)]
struct ProjectEntry {
    project: RemoteProject,
}

#[derive(Clone)]
enum RemoteEntry {
    Project {
        projects: Vec<ProjectEntry>,
        connection: Connection,
        index: ServerIndex,
    },
    SshConfig {
        host: SharedString,
    },
}

#[derive(Clone, PartialEq)]
enum ServerIdentity {
    /// A server stored in the settings file (SSH or WSL connection).
    Settings(ServerIndex),
    /// A host that only comes from the SSH config file.
    SshConfig(SharedString),
}

impl RemoteEntry {
    fn identity(&self) -> ServerIdentity {
        match self {
            RemoteEntry::Project { index, .. } => ServerIdentity::Settings(*index),
            RemoteEntry::SshConfig { host } => ServerIdentity::SshConfig(host.clone()),
        }
    }

    fn display_host(&self) -> &str {
        match self {
            Self::Project { connection, .. } => match connection {
                Connection::Ssh(c) => c.nickname.as_deref().unwrap_or(&c.host),
                Connection::Wsl(c) => &c.distro_name,
                Connection::DevContainer(c) => &c.name,
            },
            Self::SshConfig { host, .. } => host,
        }
    }

    /// Extra text to match against that isn't shown in the primary label.
    /// When an SSH connection has a nickname, [`display_host`] surfaces the
    /// nickname and the real host is only shown as a muted aux label, so we
    /// index the host here to keep it searchable.
    fn host_alias(&self) -> Option<&str> {
        match self {
            Self::Project {
                connection: Connection::Ssh(c),
                ..
            } if c.nickname.is_some() => Some(&c.host),
            _ => None,
        }
    }

    fn connection(&self) -> Cow<'_, Connection> {
        match self {
            Self::Project { connection, .. } => Cow::Borrowed(connection),
            Self::SshConfig { host, .. } => Cow::Owned(
                SshConnection {
                    host: host.to_string(),
                    ..SshConnection::default()
                }
                .into(),
            ),
        }
    }
}

#[derive(Clone)]
struct DefaultState {
    servers: Vec<RemoteEntry>,
    /// `None` when no filter is active; `Some` carries the fuzzy match results
    /// (server/project indices plus highlight positions) sorted by score.
    filtered_servers: Option<Vec<FilteredServer>>,
    filter_data: Arc<FilterData>,
}

impl DefaultState {
    fn new(ssh_config_servers: &BTreeSet<SharedString>, cx: &mut App) -> Self {
        let ssh_settings = RemoteSettings::get_global(cx);
        let read_ssh_config = ssh_settings.read_ssh_config;

        let ssh_servers = ssh_settings
            .ssh_connections()
            .enumerate()
            .map(|(index, connection)| {
                let projects = connection
                    .projects
                    .iter()
                    .take(MAX_RECENT_PROJECTS_PER_SERVER)
                    .map(|project| ProjectEntry {
                        project: project.clone(),
                    })
                    .collect();
                RemoteEntry::Project {
                    projects,
                    index: ServerIndex::Ssh(SshServerIndex(index)),
                    connection: connection.into(),
                }
            });

        let wsl_servers = ssh_settings
            .wsl_connections()
            .enumerate()
            .map(|(index, connection)| {
                let projects = connection
                    .projects
                    .iter()
                    .take(MAX_RECENT_PROJECTS_PER_SERVER)
                    .map(|project| ProjectEntry {
                        project: project.clone(),
                    })
                    .collect();
                RemoteEntry::Project {
                    projects,
                    index: ServerIndex::Wsl(WslServerIndex(index)),
                    connection: connection.into(),
                }
            });

        let mut servers = ssh_servers.chain(wsl_servers).collect::<Vec<RemoteEntry>>();

        if read_ssh_config {
            let mut extra_servers_from_config = ssh_config_servers.clone();
            for server in &servers {
                if let RemoteEntry::Project {
                    connection: Connection::Ssh(ssh_options),
                    ..
                } = server
                {
                    extra_servers_from_config.remove(&SharedString::new(ssh_options.host.clone()));
                }
            }
            servers.extend(
                extra_servers_from_config
                    .into_iter()
                    .map(|host| RemoteEntry::SshConfig { host }),
            );
        }

        let filter_data = Arc::new(FilterData::build(&servers));
        Self {
            servers,
            filtered_servers: None,
            filter_data,
        }
    }

    fn filter_sync(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_servers = None;
            return;
        }
        self.filtered_servers = Some(filter::run_sync(&self.filter_data, query));
    }
}

#[derive(Clone)]
enum ViewServerOptionsState {
    Ssh {
        connection: SshConnectionOptions,
        server_index: SshServerIndex,
        entries: [NavigableEntry; 4],
    },
    Wsl {
        connection: WslConnectionOptions,
        server_index: WslServerIndex,
        entries: [NavigableEntry; 2],
    },
}

impl ViewServerOptionsState {
    fn entries(&self) -> &[NavigableEntry] {
        match self {
            Self::Ssh { entries, .. } => entries,
            Self::Wsl { entries, .. } => entries,
        }
    }
}

enum Mode {
    Default,
    ViewServerOptions(ViewServerOptionsState),
    EditNickname(EditNicknameState),
    ProjectPicker(Entity<ProjectPicker>),
    CreateRemoteServer(CreateRemoteServer),
    CreateRemoteServerForm(CreateRemoteServerForm),
    CreateRemoteDevContainer(CreateRemoteDevContainer),
    #[cfg(target_os = "windows")]
    AddWslDistro(AddWslDistro),
}

impl Mode {
    /// The default mode is backed by [`RemoteServerProjects::default_picker`],
    /// which is rebuilt from settings independently, so this just selects the
    /// variant and ignores its arguments.
    fn default_mode(_ssh_config_servers: &BTreeSet<SharedString>, _cx: &mut App) -> Self {
        Self::Default
    }
}

enum RemoteMatch {
    AddServer,
    AddServerForm,
    AddDevContainer,
    AddWsl,
    EditSshConfig,
    ManageSshKeys,
    PredownloadRemoteServer,
    Separator,
    /// A selectable server row. In the default view it is the only row for a
    /// server; while searching it stays the only selectable row of its group,
    /// with `host_positions` carrying the fuzzy highlight, so `↑`/`↓` keep
    /// switching between servers. Confirming it opens the server's own view.
    Server {
        server: usize,
        host_positions: Vec<usize>,
    },
    /// The non-selectable title row of a server's own view.
    FocusedServerHeader {
        server: usize,
    },
    Project {
        server: usize,
        project: usize,
        positions: Vec<usize>,
    },
    OpenFolder {
        server: usize,
    },
    ViewServerOptions {
        server: usize,
    },
    RemoteTerminal {
        server: usize,
    },
}

impl RemoteMatch {
    /// Whether the row participates in keyboard selection. While searching,
    /// every row below a server is a read-only preview of that server's own
    /// view, so only the server rows themselves can be selected and `↑`/`↓`
    /// keep switching between servers.
    fn is_selectable(&self, searching: bool) -> bool {
        match self {
            RemoteMatch::Separator | RemoteMatch::FocusedServerHeader { .. } => false,
            RemoteMatch::Project { .. }
            | RemoteMatch::OpenFolder { .. }
            | RemoteMatch::ViewServerOptions { .. }
            | RemoteMatch::RemoteTerminal { .. }
                if searching =>
            {
                false
            }
            _ => true,
        }
    }
}

/// A picker row resolved to the settings entry its per-server actions operate
/// on. A host that only exists in the SSH config file has no entry yet, so it
/// carries its host name and is saved before the action runs; otherwise
/// choosing a Remote Server source or viewing server options would have no
/// entry to persist against.
enum PickerServer {
    Saved {
        index: ServerIndex,
        connection: Connection,
    },
    UnsavedSshConfig {
        host: SharedString,
    },
}

struct RemoteServerPickerDelegate {
    remote_server_projects: WeakEntity<RemoteServerProjects>,
    state: DefaultState,
    /// The server whose dedicated view is currently shown, if any, identified
    /// both by its position in `state.servers` and by a stable identity so it
    /// can survive settings reloads that reorder the server list.
    focused_server: Option<(usize, ServerIdentity)>,
    matches: Vec<RemoteMatch>,
    selected_index: usize,
    query: String,
    has_open_project: bool,
    is_local: bool,
}

impl RemoteServerPickerDelegate {
    fn new(
        remote_server_projects: WeakEntity<RemoteServerProjects>,
        ssh_config_servers: &BTreeSet<SharedString>,
        has_open_project: bool,
        is_local: bool,
        cx: &mut App,
    ) -> Self {
        let mut this = Self {
            remote_server_projects,
            state: DefaultState::new(ssh_config_servers, cx),
            focused_server: None,
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
            has_open_project,
            is_local,
        };
        this.rebuild_matches();
        this
    }

    fn reload(
        &mut self,
        ssh_config_servers: &BTreeSet<SharedString>,
        has_open_project: bool,
        is_local: bool,
        cx: &mut App,
    ) {
        self.has_open_project = has_open_project;
        self.is_local = is_local;
        self.state = DefaultState::new(ssh_config_servers, cx);
        self.resolve_focus_after_reload();
        // Settings/ssh-config changes are rare, so re-applying the active query
        // synchronously here is fine; the per-keystroke path filters off-thread.
        self.state.filter_sync(self.query.trim());
        self.rebuild_matches();
    }

    /// Re-resolves the focused server after `state.servers` was rebuilt:
    /// prefers the remembered position, then falls back to finding the same
    /// server by identity, and drops the focus when the server is gone.
    fn resolve_focus_after_reload(&mut self) {
        let Some((index, identity)) = self.focused_server.take() else {
            return;
        };
        self.focused_server = if self
            .state
            .servers
            .get(index)
            .is_some_and(|server| server.identity() == identity)
        {
            Some((index, identity))
        } else {
            self.state
                .servers
                .iter()
                .position(|server| server.identity() == identity)
                .map(|index| (index, identity))
        };
    }

    /// Shows the dedicated view for the given server.
    fn focus_server(&mut self, server: usize) {
        if let Some(entry) = self.state.servers.get(server) {
            let identity = entry.identity();
            self.focused_server = Some((server, identity));
            self.rebuild_matches();
        }
    }

    /// Resolves a picker row to the settings identity its per-server actions
    /// need, reporting an SSH-config-only host that still has to be saved.
    fn picker_server(&self, server: usize) -> Option<PickerServer> {
        let server_entry = self.state.servers.get(server)?;
        Some(match server_entry {
            RemoteEntry::Project {
                index, connection, ..
            } => PickerServer::Saved {
                index: *index,
                connection: connection.clone(),
            },
            RemoteEntry::SshConfig { host } => {
                PickerServer::UnsavedSshConfig { host: host.clone() }
            }
        })
    }

    /// Leaves the per-server view, returning `true` when it was active.
    fn dismiss_focused_server(&mut self) -> bool {
        if self.focused_server.take().is_some() {
            self.rebuild_matches();
            true
        } else {
            false
        }
    }

    /// True while the fuzzy results across every server are shown. In this mode
    /// only the server rows can be selected; the rows below each server are
    /// read-only previews.
    fn is_searching(&self) -> bool {
        self.focused_server.is_none() && !self.query.trim().is_empty()
    }

    /// The matching locations of one server while a query is active. `None`
    /// means no query is active, so the server's full recent list is shown.
    fn filtered_project_matches(
        &self,
        server_index: usize,
        query: &str,
    ) -> Option<Vec<(usize, Vec<usize>)>> {
        if query.is_empty() {
            return None;
        }
        let Some(results) = self.state.filtered_servers.as_ref() else {
            return Some(Vec::new());
        };
        Some(
            results
                .iter()
                .find(|filtered| filtered.server_index == server_index)
                .map(|filtered| {
                    filtered
                        .project_matches
                        .iter()
                        .map(|project| (project.project_index, project.path_positions.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    /// Flattens the current state into the picker's match list. Three layouts
    /// exist: a server's own view (a server is focused), the default view with
    /// one row per server (empty query, no focus), and the fuzzy filtered view
    /// across all servers (non-empty query, no focus). The last one mirrors the
    /// content of each server's own view, but only its server row is selectable
    /// so `↑`/`↓` keep switching between servers and confirming one enters it.
    fn rebuild_matches(&mut self) {
        let has_open_project = self.has_open_project;
        let is_local = self.is_local;
        let query = self.query.trim().to_string();

        let focused_index = self.focused_server.as_ref().map(|(index, _)| *index);
        let focused = focused_index.filter(|index| *index < self.state.servers.len());
        if focused_index.is_some() && focused.is_none() {
            self.focused_server = None;
        }

        let mut matches = Vec::new();
        if let Some(server_index) = focused {
            // A server's own view: Open Folder first, then its recent
            // locations, then the per-server actions. When the view was entered
            // from a search, the query stays active and only matching locations
            // are listed.
            matches.push(RemoteMatch::FocusedServerHeader {
                server: server_index,
            });
            let project_matches = self.filtered_project_matches(server_index, &query);
            if let Some(server) = self.state.servers.get(server_index) {
                match server {
                    RemoteEntry::Project {
                        projects,
                        connection,
                        ..
                    } => {
                        matches.push(RemoteMatch::OpenFolder {
                            server: server_index,
                        });
                        match &project_matches {
                            Some(project_matches) => {
                                for (project, positions) in project_matches {
                                    matches.push(RemoteMatch::Project {
                                        server: server_index,
                                        project: *project,
                                        positions: positions.clone(),
                                    });
                                }
                            }
                            None => {
                                for project in 0..projects.len() {
                                    matches.push(RemoteMatch::Project {
                                        server: server_index,
                                        project,
                                        positions: Vec::new(),
                                    });
                                }
                            }
                        }
                        matches.push(RemoteMatch::Separator);
                        matches.push(RemoteMatch::ViewServerOptions {
                            server: server_index,
                        });
                        if matches!(connection, Connection::Ssh(_)) {
                            matches.push(RemoteMatch::RemoteTerminal {
                                server: server_index,
                            });
                        }
                    }
                    RemoteEntry::SshConfig { .. } => {
                        matches.push(RemoteMatch::OpenFolder {
                            server: server_index,
                        });
                        matches.push(RemoteMatch::Separator);
                        matches.push(RemoteMatch::ViewServerOptions {
                            server: server_index,
                        });
                        matches.push(RemoteMatch::RemoteTerminal {
                            server: server_index,
                        });
                    }
                }
            }
        } else if query.is_empty() {
            matches.push(RemoteMatch::AddServer);
            matches.push(RemoteMatch::AddServerForm);
            if has_open_project && is_local {
                matches.push(RemoteMatch::AddDevContainer);
            }
            if cfg!(target_os = "windows") {
                matches.push(RemoteMatch::AddWsl);
            }
            matches.push(RemoteMatch::EditSshConfig);
            matches.push(RemoteMatch::ManageSshKeys);
            matches.push(RemoteMatch::PredownloadRemoteServer);
            if !self.state.servers.is_empty() {
                matches.push(RemoteMatch::Separator);
                for server_index in 0..self.state.servers.len() {
                    matches.push(RemoteMatch::Server {
                        server: server_index,
                        host_positions: Vec::new(),
                    });
                }
            }
        } else {
            if let Some(results) = &self.state.filtered_servers {
                for filtered in results {
                    let server_index = filtered.server_index;
                    let Some(server) = self.state.servers.get(server_index) else {
                        continue;
                    };
                    if !matches.is_empty() {
                        matches.push(RemoteMatch::Separator);
                    }
                    matches.push(RemoteMatch::Server {
                        server: server_index,
                        host_positions: filtered.host_positions.clone(),
                    });
                    match server {
                        RemoteEntry::Project { connection, .. } => {
                            matches.push(RemoteMatch::OpenFolder {
                                server: server_index,
                            });
                            for project in &filtered.project_matches {
                                matches.push(RemoteMatch::Project {
                                    server: server_index,
                                    project: project.project_index,
                                    positions: project.path_positions.clone(),
                                });
                            }
                            matches.push(RemoteMatch::Separator);
                            matches.push(RemoteMatch::ViewServerOptions {
                                server: server_index,
                            });
                            if matches!(connection, Connection::Ssh(_)) {
                                matches.push(RemoteMatch::RemoteTerminal {
                                    server: server_index,
                                });
                            }
                        }
                        RemoteEntry::SshConfig { .. } => {
                            matches.push(RemoteMatch::OpenFolder {
                                server: server_index,
                            });
                            matches.push(RemoteMatch::Separator);
                            matches.push(RemoteMatch::ViewServerOptions {
                                server: server_index,
                            });
                            matches.push(RemoteMatch::RemoteTerminal {
                                server: server_index,
                            });
                        }
                    }
                }
            }
        }

        self.matches = matches;
        let searching = self.is_searching();
        // Entering a server from a search keeps the query active, so put the
        // cursor on the matched location instead of on Open Folder.
        let matched_location = if focused.is_some() && !query.is_empty() {
            self.matches
                .iter()
                .position(|entry| matches!(entry, RemoteMatch::Project { .. }))
        } else {
            None
        };
        self.selected_index = matched_location
            .or_else(|| {
                self.matches
                    .iter()
                    .position(|entry| entry.is_selectable(searching))
            })
            .unwrap_or(0);
    }

    /// A selectable server row; confirming it opens the server's own view.
    fn render_server_row(
        &self,
        ix: usize,
        server_index: usize,
        host_positions: &[usize],
        selected: bool,
    ) -> Option<AnyElement> {
        let server = self.state.servers.get(server_index)?;
        let connection = server.connection().into_owned();
        let (main_label, aux_label, is_wsl) = server_labels(&connection);
        Some(
            ListItem::new(("remote-server", ix))
                .toggle_state(selected)
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .start_slot(Icon::new(IconName::Server).color(Color::Muted))
                .child(
                    h_flex()
                        .debug_selector(|| format!("remote-server-{}", server.display_host()))
                        .gap_1()
                        .overflow_hidden()
                        .text_ellipsis()
                        .when(is_wsl, |this| {
                            this.child(
                                Label::new(i18n::t!("41ed0a615790bb95")).color(Color::Muted),
                            )
                        })
                        .child(HighlightedLabel::new(main_label, host_positions.to_vec()))
                        .children(aux_label.map(|label| Label::new(label).color(Color::Muted))),
                )
                .end_slot(Icon::new(IconName::ChevronRight).color(Color::Muted))
                .into_any_element(),
        )
    }

    /// The title row of a server's own view, with a button that returns to
    /// the server list.
    fn render_focused_server_header(
        &self,
        server_index: usize,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let server = self.state.servers.get(server_index)?;
        let connection = server.connection().into_owned();
        let (main_label, aux_label, is_wsl) = server_labels(&connection);
        Some(
            h_flex()
                .w_full()
                .pt_1()
                .pl_1()
                .pr_3()
                .gap_1()
                .overflow_hidden()
                .child(
                    IconButton::new("back-to-server-list", IconName::ArrowLeft)
                        .icon_size(IconSize::Small)
                        .shape(IconButtonShape::Square)
                        .tooltip(Tooltip::text(i18n::t!("572cf45ba43634b3")))
                        .on_click(cx.listener(|picker, _, _, cx| {
                            if picker.delegate.dismiss_focused_server() {
                                cx.notify();
                            }
                        })),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .overflow_hidden()
                        .text_ellipsis()
                        .when(is_wsl, |this| {
                            this.child(
                                Label::new(i18n::t!("41ed0a615790bb95"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .child(
                            Label::new(main_label)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .children(aux_label.map(|label| {
                            Label::new(label).size(LabelSize::Small).color(Color::Muted)
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_action_item(
        &self,
        ix: usize,
        icon: IconName,
        label: &'static str,
        selected: bool,
    ) -> AnyElement {
        ListItem::new(("remote-action", ix))
            .toggle_state(selected)
            .inset(true)
            .spacing(ui::ListItemSpacing::Sparse)
            .start_slot(Icon::new(icon).color(Color::Muted))
            .child(Label::new(label))
            .into_any_element()
    }
}

/// The display labels for a connection: primary label, an optional auxiliary
/// label (the real host when a nickname hides it), and whether it's WSL.
fn server_labels(connection: &Connection) -> (String, Option<SharedString>, bool) {
    match connection {
        Connection::Ssh(connection) => {
            if let Some(nickname) = connection.nickname.clone() {
                let aux_label = SharedString::from(format!("({})", connection.host));
                (nickname, Some(aux_label), false)
            } else {
                (connection.host.clone(), None, false)
            }
        }
        Connection::Wsl(connection) => (connection.distro_name.clone(), None, true),
        Connection::DevContainer(connection) => (connection.name.clone(), None, false),
    }
}

impl PickerDelegate for RemoteServerPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "RemoteServerPicker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        let searching = self.is_searching();
        self.matches
            .get(ix)
            .is_some_and(|entry| entry.is_selectable(searching))
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::Start
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        i18n::t!("6ca8ee311f9672f0").into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(i18n::t!("9d6ff14ffd52289c").into())
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        let query = self.query.trim().to_string();

        if query.is_empty() {
            self.state.filtered_servers = None;
            self.rebuild_matches();
            cx.notify();
            return Task::ready(());
        }

        // A query filters the current scope: the global server list, or the
        // focused server's own view when one has been entered.

        let filter_data = self.state.filter_data.clone();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            // A fresh, never-set cancel flag: stale runs are abandoned when the
            // Picker drops this task on the next keystroke, so out-of-order
            // results can't be applied (mirrors `command_palette`).
            let cancel = AtomicBool::new(false);
            let Some(results) = filter::run_async(&filter_data, &query, &cancel, executor).await
            else {
                return;
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.state.filtered_servers = Some(results);
                    picker.delegate.rebuild_matches();
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.matches.get(self.selected_index) else {
            return;
        };
        let remote_server_projects = self.remote_server_projects.clone();
        match entry {
            RemoteMatch::Separator | RemoteMatch::FocusedServerHeader { .. } => {}
            RemoteMatch::Server { server, .. } => {
                let server = *server;
                self.focus_server(server);
                cx.notify();
            }
            RemoteMatch::AddServer => {
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.mode = Mode::CreateRemoteServer(CreateRemoteServer::new(window, cx));
                        cx.notify();
                    })
                    .ok();
            }
            RemoteMatch::AddServerForm => {
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.mode =
                            Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
                        cx.notify();
                    })
                    .ok();
            }
            RemoteMatch::AddDevContainer => {
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.init_dev_container_mode(window, cx);
                    })
                    .ok();
            }
            RemoteMatch::AddWsl => {
                #[cfg(target_os = "windows")]
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.mode = Mode::AddWslDistro(AddWslDistro::new(window, cx));
                        cx.notify();
                    })
                    .ok();
            }
            RemoteMatch::EditSshConfig => {
                remote_server_projects
                    .update(cx, |this, cx| this.edit_local_ssh_config(window, cx))
                    .log_err();
            }
            RemoteMatch::PredownloadRemoteServer => {
                window.dispatch_action(crate::PredownloadRemoteServer.boxed_clone(), cx);
            }
            RemoteMatch::ManageSshKeys => {
                remote_server_projects
                    .update(cx, |this, cx| this.manage_ssh_keys(window, cx))
                    .log_err();
            }
            RemoteMatch::Project {
                server, project, ..
            } => {
                let Some(RemoteEntry::Project {
                    connection,
                    index,
                    projects,
                    ..
                }) = self.state.servers.get(*server)
                else {
                    return;
                };
                let Some(project_entry) = projects.get(*project) else {
                    return;
                };
                let connection = connection.clone();
                let index = *index;
                let project = project_entry.project.clone();
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.open_remote_project_entry(
                            index, project, connection, secondary, window, cx,
                        );
                    })
                    .ok();
            }
            RemoteMatch::OpenFolder { server } => {
                let Some(server_entry) = self.state.servers.get(*server) else {
                    return;
                };
                // The picker-level dismissal below must close the modal rather
                // than step back to the server list.
                self.focused_server = None;
                cx.emit(DismissEvent);
                match server_entry {
                    RemoteEntry::Project {
                        connection, index, ..
                    } => {
                        let connection = connection.clone();
                        let index = *index;
                        remote_server_projects
                            .update(cx, |this, cx| {
                                this.create_remote_project(index, connection.into(), window, cx);
                            })
                            .ok();
                    }
                    RemoteEntry::SshConfig { host, .. } => {
                        let host = host.clone();
                        let connection = server_entry.connection().into_owned();
                        remote_server_projects
                            .update(cx, |this, cx| {
                                let new_ix = this.create_host_from_ssh_config(&host, cx);
                                this.create_remote_project(
                                    new_ix.into(),
                                    connection.into(),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }
                }
            }
            RemoteMatch::RemoteTerminal { server } => {
                let Some(picker_server) = self.picker_server(*server) else {
                    return;
                };
                remote_server_projects
                    .update(cx, |this, cx| {
                        let (index, connection) = this.save_picker_server(picker_server, cx);
                        let (ServerIndex::Ssh(ssh_index), Connection::Ssh(connection)) =
                            (index, connection)
                        else {
                            return;
                        };
                        this.open_remote_terminal_workspace(
                            ssh_index, connection, secondary, window, cx,
                        );
                    })
                    .log_err();
            }
            RemoteMatch::ViewServerOptions { server } => {
                let Some(picker_server) = self.picker_server(*server) else {
                    return;
                };
                remote_server_projects
                    .update(cx, |this, cx| {
                        let (index, connection) = this.save_picker_server(picker_server, cx);
                        this.view_server_options((index, connection.into()), window, cx);
                    })
                    .ok();
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn select_child(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        let Some(RemoteMatch::Server { server, .. }) = self.matches.get(self.selected_index) else {
            return None;
        };
        let server = *server;
        self.focus_server(server);
        cx.notify();
        // Keep the active query when drilling in so a location matched from the
        // search view stays filtered and highlighted inside the server.
        Some(self.query.clone())
    }

    fn select_parent(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        if self.dismiss_focused_server() {
            cx.notify();
            Some(String::new())
        } else {
            None
        }
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        match entry {
            RemoteMatch::Separator => Some(div().child(ListSeparator).into_any_element()),
            RemoteMatch::Server {
                server,
                host_positions,
            } => self.render_server_row(ix, *server, host_positions, selected),
            RemoteMatch::FocusedServerHeader { server } => {
                self.render_focused_server_header(*server, cx)
            }
            RemoteMatch::AddServer => {
                Some(self.render_action_item(
                    ix,
                    IconName::Plus,
                    i18n::t!("591480f799bfdc33"),
                    selected,
                ))
            }
            RemoteMatch::AddServerForm => Some(self.render_action_item(
                ix,
                IconName::Plus,
                i18n::t!("c362a97d314389cd"),
                selected,
            )),
            RemoteMatch::AddDevContainer => {
                Some(self.render_action_item(
                    ix,
                    IconName::Plus,
                    i18n::t!("8115d17d2638aa80"),
                    selected,
                ))
            }
            RemoteMatch::AddWsl => {
                Some(self.render_action_item(
                    ix,
                    IconName::Plus,
                    i18n::t!("52b35c9c3eff543a"),
                    selected,
                ))
            }
            RemoteMatch::EditSshConfig => Some(self.render_action_item(
                ix,
                IconName::Settings,
                i18n::t!("f3d694312a38e70c"),
                selected,
            )),
            RemoteMatch::PredownloadRemoteServer => Some(self.render_action_item(
                ix,
                IconName::Server,
                i18n::t!("eeaf60325cae1c33"),
                selected,
            )),
            RemoteMatch::ManageSshKeys => Some(self.render_action_item(
                ix,
                IconName::Server,
                i18n::t!("deb79a677604de8b"),
                selected,
            )),
            RemoteMatch::OpenFolder { .. } => Some(self.render_action_item(
                ix,
                IconName::Plus,
                i18n::t!("68b3f4bff6de90ae"),
                selected,
            )),
            RemoteMatch::RemoteTerminal { .. } => Some(self.render_action_item(
                ix,
                IconName::Terminal,
                i18n::t!("136cbc80612aee9b"),
                selected,
            )),
            RemoteMatch::ViewServerOptions { .. } => Some(self.render_action_item(
                ix,
                IconName::Settings,
                i18n::t!("a441806afc2c82d7"),
                selected,
            )),
            RemoteMatch::Project {
                server,
                project,
                positions,
            } => {
                let server_entry = self.state.servers.get(*server)?;
                let RemoteEntry::Project {
                    projects, index, ..
                } = server_entry
                else {
                    return None;
                };
                let project_entry = projects.get(*project)?;
                let paths = project_entry.project.paths.clone();

                if self.is_searching() {
                    // A read-only preview of a matched location under its
                    // server: the highlight is shown but the row can't be
                    // selected or deleted without entering the server first.
                    return Some(
                        ListItem::new(("remote-project-preview", ix))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(
                                Icon::new(IconName::Folder)
                                    .color(Color::Muted)
                                    .size(IconSize::Small),
                            )
                            .child(
                                HighlightedLabel::new(paths.join(", "), positions.clone())
                                    .truncate_start(),
                            )
                            .tooltip(Tooltip::text(paths.join("\n")))
                            .into_any_element(),
                    );
                }

                let server_ix = *index;
                let remote_project = project_entry.project.clone();
                let remote_server_projects = self.remote_server_projects.clone();

                Some(
                    ListItem::new(("remote-project", ix))
                        .toggle_state(selected)
                        .inset(true)
                        .spacing(ui::ListItemSpacing::Sparse)
                        .start_slot(
                            Icon::new(IconName::Folder)
                                .color(Color::Muted)
                                .size(IconSize::Small),
                        )
                        .child(
                            HighlightedLabel::new(paths.join(", "), positions.clone())
                                .truncate_start(),
                        )
                        .tooltip(Tooltip::text(paths.join("\n")))
                        .end_slot(
                            div().mr_2().child(
                                IconButton::new("remove-remote-project", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .shape(IconButtonShape::Square)
                                    .size(ButtonSize::Large)
                                    .tooltip(Tooltip::text(i18n::t!("cde89afaae2fa759")))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let remote_project = remote_project.clone();
                                        remote_server_projects
                                            .update(cx, |this, cx| {
                                                this.delete_remote_project(
                                                    server_ix,
                                                    &remote_project,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                    })),
                            ),
                        )
                        .show_end_slot_on_hover()
                        .into_any_element(),
                )
            }
        }
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let is_project_selected = matches!(
            self.matches.get(self.selected_index),
            Some(RemoteMatch::Project { .. })
        );

        let confirm_button = |label: SharedString| {
            Button::new("select", label)
                .key_binding(KeyBinding::for_action(&menu::Confirm, cx))
                .on_click(|_, window, cx| window.dispatch_action(menu::Confirm.boxed_clone(), cx))
        };

        let buttons = if is_project_selected {
            h_flex()
                .gap_1()
                .child(
                    Button::new("open_new_window", i18n::t!("1a1281a5e5c48811"))
                        .key_binding(KeyBinding::for_action(&menu::SecondaryConfirm, cx))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                        }),
                )
                .child(confirm_button(i18n::t!("c771248e511fbf93").into()))
                .into_any_element()
        } else {
            confirm_button(i18n::t!("c11330b85234f9c0").into()).into_any_element()
        };

        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(buttons)
                .into_any(),
        )
    }
}

impl RemoteServerProjects {
    #[cfg(target_os = "windows")]
    pub fn wsl(
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            Mode::AddWslDistro(AddWslDistro::new(window, cx)),
            create_new_window,
            fs,
            window,
            workspace,
            cx,
        )
    }

    pub fn new(
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            Mode::default_mode(&BTreeSet::new(), cx),
            create_new_window,
            fs,
            window,
            workspace,
            cx,
        )
    }

    /// Creates a new RemoteServerProjects modal that opens directly in dev container creation mode.
    /// Used when suggesting dev container connection from toast notification.
    pub fn new_dev_container(
        fs: Arc<dyn Fs>,
        configs: Vec<DevContainerConfig>,
        app_state: Arc<AppState>,
        dev_container_context: Option<DevContainerContext>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_mode = if configs.len() > 1 {
            DevContainerCreationProgress::SelectingConfig
        } else {
            DevContainerCreationProgress::Creating
        };

        let mut this = Self::new_inner(
            Mode::CreateRemoteDevContainer(CreateRemoteDevContainer::new(initial_mode, cx)),
            false,
            fs,
            window,
            workspace,
            cx,
        );

        if configs.len() > 1 {
            let delegate = DevContainerPickerDelegate::new(configs, cx.weak_entity());
            this.dev_container_picker =
                Some(cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded()));
        } else if let Some(context) = dev_container_context {
            let config = configs.into_iter().next();
            this.open_dev_container(config, app_state, context, window, cx);
            this.view_in_progress_dev_container(window, cx);
        } else {
            log::error!("No active project directory for Dev Container");
        }

        this
    }

    pub fn popover(
        fs: Arc<dyn Fs>,
        workspace: WeakEntity<Workspace>,
        create_new_window: Option<bool>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let create_new_window =
            create_new_window.unwrap_or_else(|| crate::default_open_in_new_window(cx));
        cx.new(|cx| {
            let server = Self::new(create_new_window, fs, window, workspace, cx);
            server.focus_handle(cx).focus(window, cx);
            server
        })
    }

    fn new_inner(
        mode: Mode,
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let remote_server_projects = cx.weak_entity();
        // The modal is constructed inside a `workspace.update`, so the workspace
        // entity can't be read here; start with conservative defaults and refresh
        // the real flags via `defer_in` once construction completes.
        let default_picker = cx.new(|cx| {
            let delegate = RemoteServerPickerDelegate::new(
                remote_server_projects,
                &BTreeSet::new(),
                false,
                true,
                cx,
            );
            Picker::list(delegate, window, cx).embedded()
        });
        let mut read_ssh_config = RemoteSettings::get_global(cx).read_ssh_config;
        let ssh_config_updates = if read_ssh_config {
            spawn_ssh_config_watch(fs.clone(), window, cx)
        } else {
            Task::ready(())
        };

        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |recent_projects, window, cx| {
                let new_read_ssh_config = RemoteSettings::get_global(cx).read_ssh_config;
                if read_ssh_config != new_read_ssh_config {
                    read_ssh_config = new_read_ssh_config;
                    if read_ssh_config {
                        recent_projects.ssh_config_updates =
                            spawn_ssh_config_watch(fs.clone(), window, cx);
                    } else {
                        recent_projects.ssh_config_servers.clear();
                        recent_projects.ssh_config_updates = Task::ready(());
                    }
                }
                recent_projects.refresh_default_picker(window, cx);
            });

        let dismiss_subscription = cx.subscribe(&default_picker, |_, picker, _, cx| {
            // When a server's own view is showing, dismissal first steps back
            // to the server list instead of closing the modal.
            let went_back = picker.update(cx, |picker, cx| {
                let went_back = picker.delegate.dismiss_focused_server();
                if went_back {
                    cx.notify();
                }
                went_back
            });
            if !went_back {
                cx.emit(DismissEvent);
            }
        });

        cx.defer_in(window, |this, window, cx| {
            this.refresh_default_picker(window, cx);
        });

        Self {
            mode,
            focus_handle,
            default_picker,
            workspace,
            retained_connections: Vec::new(),
            ssh_config_updates,
            ssh_config_servers: BTreeSet::new(),
            create_new_window,
            dev_container_picker: None,
            _subscriptions: vec![settings_subscription, dismiss_subscription],
            allow_dismissal: true,
        }
    }

    fn project_picker(
        create_new_window: bool,
        index: ServerIndex,
        connection_options: remote::RemoteConnectionOptions,
        project: Entity<Project>,
        home_dir: RemotePathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let fs = project.read(cx).fs().clone();
        let mut this = Self::new(create_new_window, fs, window, workspace.clone(), cx);
        this.mode = Mode::ProjectPicker(ProjectPicker::new(
            create_new_window,
            index,
            connection_options,
            project,
            home_dir,
            workspace,
            window,
            cx,
        ));
        cx.notify();

        this
    }

    fn create_ssh_server(
        &mut self,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = get_text(&editor, cx);
        if input.is_empty() {
            return;
        }

        let connection_options = match SshConnectionOptions::parse_command_line(&input) {
            Ok(c) => c,
            Err(e) => {
                self.mode = Mode::CreateRemoteServer(CreateRemoteServer {
                    address_editor: editor,
                    address_error: Some(format!("could not parse: {:?}", e).into()),
                    ssh_prompt: None,
                    _creating: None,
                });
                return;
            }
        };
        let ssh_prompt = cx.new(|cx| {
            RemoteConnectionPrompt::new(
                connection_options.connection_string(),
                connection_options.nickname.clone(),
                false,
                false,
                window,
                cx,
            )
        });

        let connection = connect(
            ConnectionIdentifier::setup(),
            RemoteConnectionOptions::Ssh(connection_options.clone()),
            ssh_prompt.clone(),
            window,
            cx,
        )
        .prompt_err(i18n::t!("46c9f3b7520fde53"), window, cx, |_, _, _| None);

        let address_editor = editor.clone();
        let creating = cx.spawn_in(window, async move |this, cx| {
            match connection.await {
                Some(Some(client)) => this
                    .update_in(cx, |this, window, cx| {
                        info!("ssh server created");
                        telemetry::event!("SSH Server Created");
                        this.retained_connections.push(client);
                        this.add_ssh_server(connection_options, cx);
                        this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                        this.focus_handle(cx).focus(window, cx);
                        cx.notify()
                    })
                    .log_err(),
                _ => this
                    .update(cx, |this, cx| {
                        address_editor.update(cx, |this, _| {
                            this.set_read_only(false);
                        });
                        this.mode = Mode::CreateRemoteServer(CreateRemoteServer {
                            address_editor,
                            address_error: None,
                            ssh_prompt: None,
                            _creating: None,
                        });
                        cx.notify()
                    })
                    .log_err(),
            };
            None
        });

        editor.update(cx, |this, _| {
            this.set_read_only(true);
        });
        self.mode = Mode::CreateRemoteServer(CreateRemoteServer {
            address_editor: editor,
            address_error: None,
            ssh_prompt: Some(ssh_prompt),
            _creating: Some(creating),
        });
    }

    #[cfg(target_os = "windows")]
    fn connect_wsl_distro(
        &mut self,
        picker: Entity<Picker<crate::wsl_picker::WslPickerDelegate>>,
        distro: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection_options = WslConnectionOptions {
            distro_name: distro,
            user: None,
        };

        let prompt = cx.new(|cx| {
            RemoteConnectionPrompt::new(
                connection_options.distro_name.clone(),
                None,
                true,
                false,
                window,
                cx,
            )
        });
        let connection = connect(
            ConnectionIdentifier::setup(),
            connection_options.clone().into(),
            prompt.clone(),
            window,
            cx,
        )
        .prompt_err(i18n::t!("46c9f3b7520fde53"), window, cx, |_, _, _| None);

        let wsl_picker = picker.clone();
        let creating = cx.spawn_in(window, async move |this, cx| {
            match connection.await {
                Some(Some(client)) => this.update_in(cx, |this, window, cx| {
                    telemetry::event!("WSL Distro Added");
                    this.retained_connections.push(client);
                    let Some(fs) = this
                        .workspace
                        .read_with(cx, |workspace, cx| {
                            workspace.project().read(cx).fs().clone()
                        })
                        .log_err()
                    else {
                        return;
                    };

                    crate::add_wsl_distro(fs, &connection_options, cx);
                    this.mode = Mode::default_mode(&BTreeSet::new(), cx);
                    this.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }),
                _ => this.update(cx, |this, cx| {
                    this.mode = Mode::AddWslDistro(AddWslDistro {
                        picker: wsl_picker,
                        connection_prompt: None,
                        _creating: None,
                    });
                    cx.notify();
                }),
            }
            .log_err();
        });

        self.mode = Mode::AddWslDistro(AddWslDistro {
            picker,
            connection_prompt: Some(prompt),
            _creating: Some(creating),
        });
    }

    fn view_server_options(
        &mut self,
        (server_index, connection): (ServerIndex, RemoteConnectionOptions),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = Mode::ViewServerOptions(match (server_index, connection) {
            (ServerIndex::Ssh(server_index), RemoteConnectionOptions::Ssh(connection)) => {
                ViewServerOptionsState::Ssh {
                    connection,
                    server_index,
                    entries: std::array::from_fn(|_| NavigableEntry::focusable(cx)),
                }
            }
            (ServerIndex::Wsl(server_index), RemoteConnectionOptions::Wsl(connection)) => {
                ViewServerOptionsState::Wsl {
                    connection,
                    server_index,
                    entries: std::array::from_fn(|_| NavigableEntry::focusable(cx)),
                }
            }
            _ => {
                log::error!("server index and connection options mismatch");
                self.mode = Mode::default_mode(&BTreeSet::default(), cx);
                return;
            }
        });
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn view_in_progress_dev_container(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.allow_dismissal = false;
        self.mode = Mode::CreateRemoteDevContainer(CreateRemoteDevContainer::new(
            DevContainerCreationProgress::Creating,
            cx,
        ));
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn create_remote_project(
        &mut self,
        index: ServerIndex,
        connection_options: RemoteConnectionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let create_new_window = self.create_new_window;
        workspace.update(cx, |_, cx| {
            cx.defer_in(window, move |workspace, window, cx| {
                let app_state = workspace.app_state().clone();
                workspace.toggle_modal(window, cx, |window, cx| {
                    RemoteConnectionModal::new(&connection_options, Vec::new(), window, cx)
                });
                // can be None if another copy of this modal opened in the meantime
                let Some(modal) = workspace.active_modal::<RemoteConnectionModal>(cx) else {
                    return;
                };
                let prompt = modal.read(cx).prompt.clone();

                let connect = connect(
                    ConnectionIdentifier::setup(),
                    connection_options.clone(),
                    prompt,
                    window,
                    cx,
                )
                .prompt_err(i18n::t!("46c9f3b7520fde53"), window, cx, |_, _, _| None);

                cx.spawn_in(window, async move |workspace, cx| {
                    let session = connect.await;

                    workspace.update(cx, |workspace, cx| {
                        if let Some(prompt) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                            prompt.update(cx, |prompt, cx| prompt.finished(cx))
                        }
                    })?;

                    let Some(Some(session)) = session else {
                        return workspace.update_in(cx, |workspace, window, cx| {
                            let weak = cx.entity().downgrade();
                            let fs = workspace.project().read(cx).fs().clone();
                            workspace.toggle_modal(window, cx, |window, cx| {
                                RemoteServerProjects::new(create_new_window, fs, window, weak, cx)
                            });
                        });
                    };

                    let (path_style, project) = cx.update(|_, cx| {
                        (
                            session.read(cx).path_style(),
                            project::Project::remote(
                                session,
                                app_state.client.clone(),
                                app_state.node_runtime.clone(),
                                app_state.user_store.clone(),
                                app_state.languages.clone(),
                                app_state.fs.clone(),
                                true,
                                cx,
                            ),
                        )
                    })?;

                    let home_dir = project
                        .read_with(cx, |project, cx| project.resolve_abs_path("~", cx))
                        .await
                        .and_then(|path| path.into_abs_path())
                        .map(|path| RemotePathBuf::new(path, path_style))
                        .unwrap_or_else(|| match path_style {
                            PathStyle::Unix => RemotePathBuf::from_str("/", PathStyle::Unix),
                            PathStyle::Windows => {
                                RemotePathBuf::from_str("C:\\", PathStyle::Windows)
                            }
                        });

                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            let weak = cx.entity().downgrade();
                            workspace.toggle_modal(window, cx, |window, cx| {
                                RemoteServerProjects::project_picker(
                                    create_new_window,
                                    index,
                                    connection_options,
                                    project,
                                    home_dir,
                                    window,
                                    cx,
                                    weak,
                                )
                            });
                        })
                        .ok();
                    Ok(())
                })
                .detach();
            })
        })
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Default | Mode::ViewServerOptions(_) => {}
            Mode::ProjectPicker(_) => {}
            Mode::CreateRemoteServer(state) => {
                if let Some(prompt) = state.ssh_prompt.as_ref() {
                    prompt.update(cx, |prompt, cx| {
                        prompt.confirm(window, cx);
                    });
                    return;
                }

                self.create_ssh_server(state.address_editor.clone(), window, cx);
            }
            Mode::CreateRemoteServerForm(state) => {
                if let Some(prompt) = state.ssh_prompt.as_ref() {
                    prompt.update(cx, |prompt, cx| {
                        prompt.confirm(window, cx);
                    });
                    return;
                }

                self.confirm_create_remote_server_form(window, cx);
            }
            Mode::CreateRemoteDevContainer(_) => {}
            Mode::EditNickname(state) => {
                let text = Some(state.editor.read(cx).text(cx)).filter(|text| !text.is_empty());
                let index = state.index;
                self.update_settings_file(cx, move |setting, _| {
                    if let Some(connections) = setting.ssh_connections.as_mut()
                        && let Some(connection) = connections.get_mut(index.0)
                    {
                        connection.nickname = text;
                    }
                });
                self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
                self.focus_handle.focus(window, cx);
            }
            #[cfg(target_os = "windows")]
            Mode::AddWslDistro(state) => {
                let delegate = &state.picker.read(cx).delegate;
                let distro = delegate.selected_distro().unwrap();
                self.connect_wsl_distro(state.picker.clone(), distro, window, cx);
            }
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Cancelling the password prompt aborts the connection attempt but keeps
        // the form filled in, so the user can retry or fix a field.
        if matches!(&self.mode, Mode::CreateRemoteServerForm(state) if state.ssh_prompt.is_some()) {
            if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
                state.ssh_prompt = None;
                state._creating = None;
                state.error = None;
            }
            cx.notify();
            return;
        }

        match &self.mode {
            Mode::Default => {
                cx.emit(DismissEvent);
            }
            Mode::CreateRemoteServer(state) if state.ssh_prompt.is_some() => {
                let new_state = CreateRemoteServer::new(window, cx);
                let old_prompt = state.address_editor.read(cx).text(cx);
                new_state.address_editor.update(cx, |this, cx| {
                    this.set_text(old_prompt, window, cx);
                });

                self.mode = Mode::CreateRemoteServer(new_state);
                cx.notify();
            }
            Mode::CreateRemoteDevContainer(CreateRemoteDevContainer {
                progress: DevContainerCreationProgress::Error(_),
                ..
            }) => {
                cx.emit(DismissEvent);
            }
            _ => {
                self.allow_dismissal = true;
                self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
                self.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
        }
    }

    /// Rebuilds the default picker's data from the latest settings/ssh-config
    /// and re-applies the current filter query.
    fn workspace_flags(workspace: &WeakEntity<Workspace>, cx: &App) -> (bool, bool) {
        let has_open_project = workspace
            .upgrade()
            .map(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .next()
                    .is_some()
            })
            .unwrap_or(false);
        let is_local = workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().read(cx).is_local())
            .unwrap_or(true);
        (has_open_project, is_local)
    }

    fn refresh_default_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ssh_config_servers = self.ssh_config_servers.clone();
        let (has_open_project, is_local) = Self::workspace_flags(&self.workspace, cx);
        self.default_picker.update(cx, |picker, cx| {
            picker
                .delegate
                .reload(&ssh_config_servers, has_open_project, is_local, cx);
            picker.refresh(window, cx);
        });
    }

    /// Moves the opened project to the front of the server's recent list,
    /// applying the same least-recently-used bound as newly recorded projects.
    fn record_opened_project(
        &mut self,
        index: ServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            let projects = match index {
                ServerIndex::Ssh(index) => setting
                    .ssh_connections
                    .as_mut()
                    .and_then(|connections| connections.get_mut(index.0))
                    .map(|server| &mut server.projects),
                ServerIndex::Wsl(index) => setting
                    .wsl_connections
                    .as_mut()
                    .and_then(|connections| connections.get_mut(index.0))
                    .map(|server| &mut server.projects),
            };
            if let Some(projects) = projects {
                record_remote_project(projects, project);
            }
        });
    }

    /// Opens a saved remote project, mirroring whether a new window should be
    /// created based on the modal's `create_new_window` preference and whether
    /// the confirm was a secondary (platform-modifier) confirm.
    fn open_remote_project_entry(
        &mut self,
        index: ServerIndex,
        project: RemoteProject,
        connection: Connection,
        secondary_confirm: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(app_state) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().clone())
            .log_err()
        else {
            return;
        };
        self.record_opened_project(index, &project, cx);
        let create_new_window = self.create_new_window;
        cx.emit(DismissEvent);

        let replace_window = match (create_new_window, secondary_confirm) {
            (true, false) | (false, true) => None,
            (true, true) | (false, false) => window.window_handle().downcast::<MultiWorkspace>(),
        };

        cx.spawn_in(window, async move |_, cx| {
            let result = open_remote_project(
                connection.into(),
                project.paths.into_iter().map(PathBuf::from).collect(),
                app_state,
                OpenOptions {
                    requesting_window: replace_window,
                    ..OpenOptions::default()
                },
                cx,
            )
            .await;
            if let Err(e) = result {
                log::error!("Failed to connect: {e:#}");
                cx.prompt(
                    gpui::PromptLevel::Critical,
                    i18n::t!("46c9f3b7520fde53"),
                    Some(&e.to_string()),
                    &[i18n::t!("fac2a67ad87807c4")],
                )
                .await
                .ok();
            }
        })
        .detach();
    }

    fn open_remote_terminal_workspace(
        &mut self,
        _index: SshServerIndex,
        connection: SshConnection,
        secondary_confirm: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(app_state) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().clone())
            .log_err()
        else {
            return;
        };
        let create_new_window = self.create_new_window;
        cx.emit(DismissEvent);

        let replace_window = match (create_new_window, secondary_confirm) {
            (true, false) | (false, true) => None,
            (true, true) | (false, false) => window.window_handle().downcast::<MultiWorkspace>(),
        };

        cx.spawn_in(window, async move |_, cx| {
            let result = crate::remote_terminal_workspace::open_remote_terminal_workspace(
                RemoteConnectionOptions::Ssh(connection.clone().into()),
                app_state,
                OpenOptions {
                    requesting_window: replace_window,
                    ..OpenOptions::default()
                },
                cx,
            )
            .await;
            if let Err(e) = result {
                log::error!("Failed to open remote terminal workspace: {e:#}");
                cx.prompt(
                    gpui::PromptLevel::Critical,
                    i18n::t!("3e282fc5e171f21e"),
                    Some(&e.to_string()),
                    &[i18n::t!("fac2a67ad87807c4")],
                )
                .await
                .ok();
            }
        })
        .detach();
    }

    fn update_settings_file(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut RemoteSettingsContent, &App) + Send + Sync + 'static,
    ) {
        let Some(fs) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .log_err()
        else {
            return;
        };
        update_settings_file(fs, cx, move |setting, cx| f(&mut setting.remote, cx));
    }

    fn delete_ssh_server(&mut self, server: SshServerIndex, cx: &mut Context<Self>) {
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.ssh_connections.as_mut()
                && connections.get(server.0).is_some()
            {
                connections.remove(server.0);
            }
        });
    }

    fn delete_remote_project(
        &mut self,
        server: ServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        match server {
            ServerIndex::Ssh(server) => {
                self.delete_ssh_project(server, project, cx);
            }
            ServerIndex::Wsl(server) => {
                self.delete_wsl_project(server, project, cx);
            }
        }
    }

    fn delete_ssh_project(
        &mut self,
        server: SshServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(server) = setting
                .ssh_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server.0))
            {
                server.projects.retain(|existing| existing != &project);
            }
        });
    }

    fn delete_wsl_project(
        &mut self,
        server: WslServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(server) = setting
                .wsl_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server.0))
            {
                server.projects.retain(|existing| existing != &project);
            }
        });
    }

    fn delete_wsl_distro(&mut self, server: WslServerIndex, cx: &mut Context<Self>) {
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.wsl_connections.as_mut() {
                connections.remove(server.0);
            }
        });
    }

    fn add_ssh_server(
        &mut self,
        connection_options: remote::SshConnectionOptions,
        cx: &mut Context<Self>,
    ) {
        let connection = ssh_connection_from_options(&connection_options);
        self.update_settings_file(cx, move |setting, _| {
            setting
                .ssh_connections
                .get_or_insert(Default::default())
                .push(connection)
        });
    }

    fn set_form_error(&mut self, error: SharedString, cx: &mut Context<Self>) {
        if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
            state.error = Some(error);
            state.ssh_prompt = None;
            state._creating = None;
        }
        cx.notify();
    }

    fn set_form_auth_method(&mut self, method: SshAuthMethod, cx: &mut Context<Self>) {
        if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
            state.auth_method = method;
            state.error = None;
        }
        cx.notify();
    }

    fn set_form_key_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
            state.key_file = Some(path);
            state.key_id = None;
            state.auth_method = SshAuthMethod::Key;
            state.error = None;
        }
        cx.notify();
    }

    fn prompt_for_key_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(i18n::t!("75141f859dc99aa2").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.set_form_key_file(path, cx))
                    .log_err();
            }
            None::<()>
        })
        .detach();
    }

    fn remove_form_jump_host(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Mode::CreateRemoteServerForm(state) = &mut self.mode
            && index < state.jump_hosts.len()
        {
            state.jump_hosts.remove(index);
        }
        cx.notify();
    }

    fn open_jump_host_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let candidates = RemoteSettings::get_global(cx)
            .ssh_connections()
            .map(|connection| JumpHostCandidate {
                label: connection
                    .nickname
                    .clone()
                    .unwrap_or_else(|| connection.host.clone())
                    .into(),
                host: connection.host.clone(),
                username: connection.username.clone(),
                port: connection.port,
            })
            .collect::<Vec<_>>();
        let delegate = JumpHostPickerDelegate::new(candidates, cx.entity().downgrade());
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());
        if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
            state.jump_host_picker = Some(picker);
        }
        cx.notify();
    }

    fn read_form_draft(&self, cx: &App) -> Result<RemoteServerFormDraft, SharedString> {
        let Mode::CreateRemoteServerForm(state) = &self.mode else {
            return Err(SharedString::from(i18n::t!("a4339e03ef00d39d")));
        };

        let host = state.host_editor.read(cx).text(cx).trim().to_string();
        if host.is_empty() {
            return Err(SharedString::from(i18n::t!("a4339e03ef00d39d")));
        }
        if host.starts_with('-')
            || host
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(SharedString::from(i18n::t!("e69dbd37f66bcdd1")));
        }

        let username = trimmed_text(state.username_editor.read(cx).text(cx));
        if let Some(username) = &username
            && (username.contains('@')
                || username.contains(':')
                || username
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control()))
        {
            return Err(SharedString::from(i18n::t!("185b7c4cd938c1bd")));
        }

        let port = match trimmed_text(state.port_editor.read(cx).text(cx)) {
            Some(port) => Some(
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| SharedString::from(i18n::t!("cc7fcd5fae4acbbf")))?,
            ),
            None => None,
        };

        let connection_timeout = match trimmed_text(state.timeout_editor.read(cx).text(cx)) {
            Some(timeout) => Some(
                timeout
                    .parse::<u16>()
                    .ok()
                    .filter(|timeout| *timeout != 0)
                    .ok_or_else(|| SharedString::from(i18n::t!("31084cd9d2a74fed")))?,
            ),
            None => None,
        };

        let proxy = trimmed_text(state.proxy_editor.read(cx).text(cx));
        if let Some(proxy) = &proxy
            && !state.proxy_disabled
            && let Err(error) = util::ssh_proxy::ProxyEndpoint::parse(proxy)
        {
            return Err(SharedString::from(i18n::t_args!(
                "51d23ada472b3ed3",
                format!("{error:#}")
            )));
        }

        Ok(RemoteServerFormDraft {
            nickname: trimmed_text(state.nickname_editor.read(cx).text(cx)),
            host,
            port,
            username,
            auth_method: state.auth_method,
            key_file: state.key_file.clone(),
            existing_key_id: state.key_id.clone(),
            jump_hosts: state.jump_hosts.clone(),
            proxy,
            proxy_disabled: state.proxy_disabled,
            connection_timeout,
        })
    }

    fn confirm_create_remote_server_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = match self.read_form_draft(cx) {
            Ok(draft) => draft,
            Err(error) => {
                if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
                    state.host_error = Some(error.clone());
                    state.advanced_open = true;
                }
                self.set_form_error(error, cx);
                return;
            }
        };

        let saved_index = match &self.mode {
            Mode::CreateRemoteServerForm(state) => state.saved_index,
            _ => None,
        };

        if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
            state.error = None;
            state.host_error = None;
        }
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let mut import_failed = false;
            let key_id = match (draft.existing_key_id.clone(), draft.key_file.clone()) {
                (Some(key_id), _) => Some(key_id),
                (None, Some(path)) => match remote::import_managed_ssh_key(
                    path,
                    draft.host.clone(),
                    draft.port.unwrap_or(22),
                    draft.username.clone(),
                    cx,
                )
                .await
                {
                    Ok(imported) => Some(imported.record.key_id),
                    Err(error) => {
                        let message =
                            SharedString::from(i18n::t_args!("41b86c2b700f0a24", format!("{error:#}")));
                        this.update(cx, |this, cx| this.set_form_error(message, cx))
                            .log_err();
                        import_failed = true;
                        None
                    }
                },
                (None, None) => None,
            };
            if import_failed {
                return None::<()>;
            }

            this.update_in(cx, |this, window, cx| {
                // The user may have left the form while the key was importing.
                if !matches!(this.mode, Mode::CreateRemoteServerForm(_)) {
                    return;
                }

                let connection_options = remote::SshConnectionOptions {
                    host: draft.host.clone().into(),
                    username: draft.username.clone(),
                    port: draft.port,
                    password: None,
                    args: Some(Vec::new()),
                    port_forwards: None,
                    connection_timeout: draft.connection_timeout,
                    nickname: draft.nickname.clone(),
                    upload_binary_over_ssh: true,
                    auth_method: Some(draft.auth_method),
                    ssh_key_id: key_id.clone(),
                    jump_hosts: draft.jump_hosts.clone(),
                    proxy: draft.proxy.clone(),
                    proxy_disabled: draft.proxy_disabled,
                };

                let index =
                    this.persist_form_connection(&connection_options, saved_index, cx);
                if let Mode::CreateRemoteServerForm(state) = &mut this.mode {
                    state.saved_index = Some(index);
                    state.key_id = key_id;
                    state.key_file = None;
                }
                this.begin_form_connect(connection_options, window, cx);
            })
            .log_err();
            None::<()>
        })
        .detach();
    }

    /// Saves the form entry, replacing the entry a previous attempt created so
    /// retrying never produces duplicates.
    /// Saves the form entry, replacing the entry a previous attempt created so
    /// retrying never produces duplicates.
    fn persist_form_connection(
        &mut self,
        connection_options: &remote::SshConnectionOptions,
        existing_index: Option<SshServerIndex>,
        cx: &mut Context<Self>,
    ) -> SshServerIndex {
        let connection = ssh_connection_from_options(connection_options);
        let index = existing_index.unwrap_or_else(|| {
            SshServerIndex(RemoteSettings::get_global(cx).ssh_connections().count())
        });
        let target = index.0;
        self.update_settings_file(cx, move |setting, _| {
            let connections = setting.ssh_connections.get_or_insert(Default::default());
            if target < connections.len() {
                connections[target] = connection;
            } else {
                connections.push(connection);
            }
        });
        index
    }

    fn begin_form_connect(
        &mut self,
        connection_options: remote::SshConnectionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ssh_prompt = cx.new(|cx| {
            RemoteConnectionPrompt::new(
                connection_options.connection_string(),
                connection_options.nickname.clone(),
                false,
                false,
                window,
                cx,
            )
        });

        let connection = connect(
            ConnectionIdentifier::setup(),
            RemoteConnectionOptions::Ssh(connection_options),
            ssh_prompt.clone(),
            window,
            cx,
        )
        .prompt_err(i18n::t!("46c9f3b7520fde53"), window, cx, |_, _, _| None);

        let task = cx.spawn_in(window, async move |this, cx| {
            match connection.await {
                Some(Some(client)) => this
                    .update_in(cx, |this, window, cx| {
                        info!("ssh server created from form");
                        telemetry::event!("SSH Server Created");
                        this.retained_connections.push(client);
                        this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                        this.focus_handle(cx).focus(window, cx);
                        cx.notify()
                    })
                    .log_err(),
                _ => this
                    .update_in(cx, |this, _, cx| {
                        if let Mode::CreateRemoteServerForm(state) = &mut this.mode {
                            state.ssh_prompt = None;
                            state._creating = None;
                        }
                        cx.notify()
                    })
                    .log_err(),
            };
            None
        });

        if let Mode::CreateRemoteServerForm(state) = &mut self.mode {
            state.ssh_prompt = Some(ssh_prompt);
            state.error = None;
            state._creating = Some(task);
        }
        cx.notify();
    }

    fn edit_in_dev_container_json(
        &mut self,
        config: Option<DevContainerConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            cx.emit(DismissEvent);
            cx.notify();
            return;
        };

        let config_path = config
            .map(|c| c.config_path)
            .unwrap_or_else(|| PathBuf::from(".devcontainer/devcontainer.json"));

        workspace.update(cx, |workspace, cx| {
            let project = workspace.project().clone();

            let worktree = project
                .read(cx)
                .visible_worktrees(cx)
                .find_map(|tree| tree.read(cx).root_entry()?.is_dir().then_some(tree));

            if let Some(worktree) = worktree {
                let tree_id = worktree.read(cx).id();
                let devcontainer_path =
                    match RelPath::new(&config_path, util::paths::PathStyle::Unix) {
                        Ok(path) => path.into_owned(),
                        Err(error) => {
                            log::error!(
                                "Invalid devcontainer path: {} - {}",
                                config_path.display(),
                                error
                            );
                            return;
                        }
                    };
                cx.spawn_in(window, async move |workspace, cx| {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(
                                (tree_id, devcontainer_path),
                                None,
                                true,
                                window,
                                cx,
                            )
                        })?
                        .await
                })
                .detach();
            } else {
                return;
            }
        });
        cx.emit(DismissEvent);
        cx.notify();
    }

    fn init_dev_container_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let configs = self
            .workspace
            .read_with(cx, |workspace, cx| find_devcontainer_configs(workspace, cx))
            .unwrap_or_default();

        if configs.len() > 1 {
            let delegate = DevContainerPickerDelegate::new(configs, cx.weak_entity());
            self.dev_container_picker =
                Some(cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded()));

            let state =
                CreateRemoteDevContainer::new(DevContainerCreationProgress::SelectingConfig, cx);
            self.mode = Mode::CreateRemoteDevContainer(state);
            cx.notify();
        } else if let Some((app_state, context)) = self
            .workspace
            .read_with(cx, |workspace, cx| {
                let app_state = workspace.app_state().clone();
                let context = DevContainerContext::from_workspace(workspace, cx)?;
                Some((app_state, context))
            })
            .ok()
            .flatten()
        {
            let config = configs.into_iter().next();
            self.open_dev_container(config, app_state, context, window, cx);
            self.view_in_progress_dev_container(window, cx);
        } else {
            log::error!("No active project directory for Dev Container");
        }
    }

    fn open_dev_container(
        &self,
        config: Option<DevContainerConfig>,
        app_state: Arc<AppState>,
        context: DevContainerContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let replace_window = window.window_handle().downcast::<MultiWorkspace>();
        let app_state = Arc::downgrade(&app_state);

        cx.spawn_in(window, async move |entity, cx| {
            let environment = context.environment(cx).await;

            let (dev_container_connection, starting_dir) =
                match start_dev_container_with_config(context, config, environment).await {
                    Ok((c, s)) => (c, s),
                    Err(e) => {
                        log::error!("Failed to start dev container: {:?}", e);
                        cx.prompt(
                            gpui::PromptLevel::Critical,
                            "Failed to start Dev Container. See logs for details",
                            Some(&format!("{e}")),
                            &[i18n::t!("fac2a67ad87807c4")],
                        )
                        .await
                        .ok();
                        entity
                            .update_in(cx, |remote_server_projects, window, cx| {
                                remote_server_projects.allow_dismissal = true;
                                remote_server_projects.mode =
                                    Mode::CreateRemoteDevContainer(CreateRemoteDevContainer::new(
                                        DevContainerCreationProgress::Error(format!("{e}")),
                                        cx,
                                    ));
                                remote_server_projects.focus_handle(cx).focus(window, cx);
                            })
                            .ok();
                        return;
                    }
                };
            cx.update(|_, cx| {
                ExtensionStore::global(cx).update(cx, |this, cx| {
                    for extension in &dev_container_connection.extension_ids {
                        log::info!("Installing extension {extension} from devcontainer");
                        this.install_latest_extension(Arc::from(extension.clone()), cx);
                    }
                })
            })
            .log_err();

            entity
                .update(cx, |this, cx| {
                    this.allow_dismissal = true;
                    cx.emit(DismissEvent);
                })
                .log_err();

            let Some(app_state) = app_state.upgrade() else {
                return;
            };
            let result = open_remote_project(
                Connection::DevContainer(dev_container_connection).into(),
                vec![starting_dir].into_iter().map(PathBuf::from).collect(),
                app_state,
                OpenOptions {
                    requesting_window: replace_window,
                    ..OpenOptions::default()
                },
                cx,
            )
            .await;
            if let Err(e) = result {
                log::error!("Failed to connect: {e:#}");
                cx.prompt(
                    gpui::PromptLevel::Critical,
                    i18n::t!("46c9f3b7520fde53"),
                    Some(&e.to_string()),
                    &[i18n::t!("fac2a67ad87807c4")],
                )
                .await
                .ok();
            }
        })
        .detach();
    }

    fn render_create_dev_container(
        &self,
        state: &CreateRemoteDevContainer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        match &state.progress {
            DevContainerCreationProgress::Error(message) => {
                let view = Navigable::new(
                    div()
                        .child(
                            div().track_focus(&self.focus_handle(cx)).size_full().child(
                                v_flex().py_1().child(
                                    ListItem::new("Error")
                                        .inset(true)
                                        .selectable(false)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::XCircle).color(Color::Error),
                                        )
                                        .child(Label::new(i18n::t!("a9d49e3feeb54a6a")))
                                        .child(Label::new(message).buffer_font(cx)),
                                ),
                            ),
                        )
                        .child(ListSeparator)
                        .child(
                            div()
                                .id("devcontainer-see-log")
                                .track_focus(&state.view_logs_entry.focus_handle)
                                .on_action(cx.listener(|_, _: &menu::Confirm, window, cx| {
                                    window.dispatch_action(Box::new(OpenLog), cx);
                                    cx.emit(DismissEvent);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("li-devcontainer-see-log")
                                        .toggle_state(
                                            state
                                                .view_logs_entry
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::File)
                                                .color(Color::Muted)
                                                .size(IconSize::Small),
                                        )
                                        .child(Label::new(i18n::t!("19051a62bc3d1963")))
                                        .on_click(cx.listener(|_, _, window, cx| {
                                            window.dispatch_action(Box::new(OpenLog), cx);
                                            cx.emit(DismissEvent);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("devcontainer-go-back")
                                .track_focus(&state.back_entry.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.cancel(&menu::Cancel, window, cx);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("li-devcontainer-go-back")
                                        .toggle_state(
                                            state
                                                .back_entry
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Exit)
                                                .color(Color::Muted)
                                                .size(IconSize::Small),
                                        )
                                        .child(Label::new(i18n::t!("498e1d59b4d787ee")))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .into_any_element(),
                )
                .entry(state.view_logs_entry.clone())
                .entry(state.back_entry.clone());
                view.render(window, cx).into_any_element()
            }
            DevContainerCreationProgress::SelectingConfig => {
                self.render_config_selection(window, cx).into_any_element()
            }
            DevContainerCreationProgress::Creating => {
                self.focus_handle(cx).focus(window, cx);
                div()
                    .track_focus(&self.focus_handle(cx))
                    .size_full()
                    .child(
                        v_flex()
                            .pb_1()
                            .child(
                                ModalHeader::new().child(
                                    Headline::new(i18n::t!("5d76904f98924950"))
                                        .size(HeadlineSize::XSmall),
                                ),
                            )
                            .child(ListSeparator)
                            .child(
                                ListItem::new("creating")
                                    .inset(true)
                                    .spacing(ui::ListItemSpacing::Sparse)
                                    .disabled(true)
                                    .start_slot(
                                        Icon::new(IconName::ArrowCircle)
                                            .color(Color::Muted)
                                            .with_rotate_animation(2),
                                    )
                                    .child(
                                        h_flex()
                                            .opacity(0.6)
                                            .gap_1()
                                            .child(Label::new(i18n::t!("1157e08be113dd47")))
                                            .child(LoadingLabel::new("")),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_config_selection(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(picker) = &self.dev_container_picker else {
            return div().into_any_element();
        };

        let content = v_flex().pb_1().child(picker.clone().into_any_element());

        picker.focus_handle(cx).focus(window, cx);

        content.into_any_element()
    }

    fn render_create_remote_server(
        &self,
        state: &CreateRemoteServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ssh_prompt = state.ssh_prompt.clone();

        state.address_editor.update(cx, |editor, cx| {
            if editor.text(cx).is_empty() {
                editor.set_placeholder_text("ssh user@example -p 2222", window, cx);
            }
        });

        let theme = cx.theme();

        v_flex()
            .track_focus(&self.focus_handle(cx))
            .id("create-remote-server")
            .overflow_hidden()
            .size_full()
            .flex_1()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(state.address_editor.clone()),
            )
            .child(
                h_flex()
                    .bg(theme.colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .map(|this| {
                        if let Some(ssh_prompt) = ssh_prompt {
                            this.child(h_flex().w_full().child(ssh_prompt))
                        } else if let Some(address_error) = &state.address_error {
                            this.child(
                                h_flex().p_2().w_full().gap_2().child(
                                    Label::new(address_error.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                ),
                            )
                        } else {
                            this.child(
                                h_flex()
                                    .p_2()
                                    .w_full()
                                    .gap_1()
                                    .child(
                                        Label::new(i18n::t!("37e81904c66f0280"))
                                            .color(Color::Muted)
                                            .size(LabelSize::Small),
                                    )
                                    .child(
                                        Button::new("learn-more", i18n::t!("ca66c2da6f5bf825"))
                                            .label_size(LabelSize::Small)
                                            .end_icon(
                                                Icon::new(IconName::ArrowUpRight)
                                                    .size(IconSize::XSmall),
                                            )
                                            .on_click(|_, _, cx| {
                                                cx.open_url(
                                                    "https://zed.dev/docs/remote-development",
                                                );
                                            }),
                                    ),
                            )
                        }
                    }),
            )
    }

    fn render_create_remote_server_form(
        &self,
        state: &CreateRemoteServerForm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(prompt) = state.ssh_prompt.clone() {
            return v_flex()
                .track_focus(&self.focus_handle(cx))
                .id("create-remote-server-form-prompt")
                .size_full()
                .child(prompt)
                .into_any_element();
        }

        let colors = cx.theme().colors().clone();
        let border_color = colors.border_variant;
        let auth_hint = match state.auth_method {
            SshAuthMethod::Password => i18n::t!("4347df11b74e14ea"),
            SshAuthMethod::Key => i18n::t!("429db636b4adcef2"),
            SshAuthMethod::Interactive => i18n::t!("ca192a6339891b48"),
        };
        let key_label = state
            .key_file
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| i18n::t_args!("bbe0fecf4af46fe9", name.to_string_lossy()));

        let host_error = state.host_error.clone();
        let advanced_summary = state.advanced_summary(cx);

        let mut connection_fields = v_flex()
            .w_full()
            .gap_3()
            .child(form_section_head(
                i18n::t!("82479cb6ca73042d"),
                Some(i18n::t!("aded7effd1660dad").into()),
                cx,
            ))
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(form_label(
                        i18n::t!("d44e9b3d3b31d37b"),
                        Some(i18n::t!("f73049dceb2f0281").into()),
                    ))
                    .child(
                        div()
                            .debug_selector(|| "form-input-nickname".into())
                            .child(form_input_surface(&state.nickname_editor, false, cx)),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Label::new(i18n::t!("317c133e7a877caf")))
                                    .child(form_badge(
                                        i18n::t!("11da9dc4428537c4"),
                                        Color::Accent,
                                        cx,
                                    )),
                            )
                            .child(div().debug_selector(|| "form-input-host".into()).child(
                                form_input_surface(&state.host_editor, host_error.is_some(), cx),
                            ))
                            .when_some(host_error, |this, error| {
                                this.child(
                                    Label::new(error)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Error),
                                )
                            }),
                    )
                    .child(
                        v_flex()
                            .w(rems_from_px(88_f32))
                            .flex_none()
                            .gap_1()
                            .child(Label::new(i18n::t!("e71ac32b544b0ebf")))
                            .child(
                                div()
                                    .debug_selector(|| "form-input-port".into())
                                    .child(form_input_surface(&state.port_editor, false, cx)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(form_label(
                                i18n::t!("5e75d10975e8ce94"),
                                Some(i18n::t!("4eca75ab4b1fc398").into()),
                            ))
                            .child(
                                div()
                                    .debug_selector(|| "form-input-username".into())
                                    .child(form_input_surface(&state.username_editor, false, cx)),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(Label::new(i18n::t!("52373e4342173705")))
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_0p5()
                                    .p_0p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border)
                                    .bg(colors.editor_background)
                                    .children(
                                        [
                                            SshAuthMethod::Password,
                                            SshAuthMethod::Key,
                                            SshAuthMethod::Interactive,
                                        ]
                                        .map(|method| {
                                            form_segment(
                                                SharedString::from(match method {
                                                    SshAuthMethod::Password => {
                                                        "auth-method-password"
                                                    }
                                                    SshAuthMethod::Key => "auth-method-key",
                                                    SshAuthMethod::Interactive => {
                                                        "auth-method-interactive"
                                                    }
                                                }),
                                                auth_method_label(method),
                                                state.auth_method == method,
                                                cx.listener(move |this, _, _, cx| {
                                                    this.set_form_auth_method(method, cx)
                                                }),
                                                cx,
                                            )
                                        }),
                                    ),
                            ),
                    ),
            )
            .child(
                Label::new(auth_hint)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when(state.auth_method == SshAuthMethod::Key, |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .border_1()
                                .border_dashed()
                                .border_color(colors.border)
                                .child(
                                    Icon::new(IconName::Lock)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(i18n::t!("0b26467a5e72a15d"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .flex_1(),
                                )
                                .child(
                                    Button::new("choose-key-file", i18n::t!("75141f859dc99aa2"))
                                        .size(ButtonSize::Default)
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.prompt_for_key_file(window, cx)
                                        })),
                                ),
                        )
                        .when(state.key_file.is_none() && state.key_id.is_none(), |this| {
                            this.child(
                                div()
                                    .debug_selector(|| "form-key-default-hint".into())
                                    .child(
                                        Label::new(i18n::t!("e02fc8f1ec338629"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                        })
                        .when_some(key_label.clone(), |this, label| {
                            this.child(
                                Label::new(label)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .child(
                            Label::new(i18n::t!("20199ba6524b09f6"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            });

        if let Some(preview) = state.ssh_preview(cx) {
            connection_fields = connection_fields.child(
                h_flex()
                    .debug_selector(|| "form-ssh-preview".into())
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(border_color)
                    .child(
                        div()
                            .flex_none()
                            .mr_2()
                            .pr_2()
                            .border_r_1()
                            .border_color(border_color)
                            .child(
                                Label::new(i18n::t!("b25c0dac0eddd103"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        Label::new(preview)
                            .size(LabelSize::XSmall)
                            .color(Color::Default)
                            .truncate(),
                    ),
            );
        }

        let advanced = v_flex()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(border_color)
            .overflow_hidden()
            .child(
                h_flex()
                    .id("toggle-advanced-options")
                    .debug_selector(|| "form-advanced-toggle".into())
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Mode::CreateRemoteServerForm(state) = &mut this.mode {
                            state.advanced_open = !state.advanced_open;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if state.advanced_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(i18n::t!("91a61fdde856cd9e"))
                            .size(LabelSize::Small)
                            .flex_none(),
                    )
                    .child(
                        Label::new(i18n::t!("db639f3485a635fc"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .flex_1()
                            .truncate(),
                    )
                    .when_some(advanced_summary, |this, summary| {
                        this.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(colors.text_accent.opacity(0.14))
                                .child(
                                    Label::new(summary)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                ),
                        )
                    }),
            )
            .when(state.advanced_open, |this| {
                this.child(
                    v_flex()
                        .debug_selector(|| "form-advanced-body".into())
                        .w_full()
                        .gap_3()
                        .px_2()
                        .pt_2()
                        .pb_2()
                        .border_t_1()
                        .border_color(border_color)
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .items_start()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_1()
                                        .child(form_label(
                                            i18n::t!("3899218664bb77ca"),
                                            Some(i18n::t!("30928008029cdf5f").into()),
                                        ))
                                        .child(
                                            div().debug_selector(|| "form-input-timeout".into()).child(
                                                form_input_surface(&state.timeout_editor, false, cx),
                                            ),
                                        ),
                                ),
                        )
                        .child(
                            v_flex()
                                .w_full()
                                .gap_1()
                                .child(form_label(
                                    i18n::t!("4f6e412d26ed86b2"),
                                    Some(i18n::t!("db639f3485a635fc").into()),
                                ))
                                .children(state.jump_hosts.iter().enumerate().map(
                                    |(index, jump_host)| {
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(colors.border)
                                            .bg(colors.editor_background)
                                            .child(
                                                div()
                                                    .size(rems_from_px(16_f32))
                                                    .flex_none()
                                                    .rounded_full()
                                                    .bg(colors.text_accent.opacity(0.14))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(
                                                        Label::new((index + 1).to_string())
                                                            .size(LabelSize::XSmall)
                                                            .color(Color::Accent),
                                                    ),
                                            )
                                            .child(
                                                Label::new(jump_host_label(jump_host))
                                                    .size(LabelSize::Small)
                                                    .flex_1()
                                                    .truncate(),
                                            )
                                            .child(
                                                IconButton::new(
                                                    SharedString::from(format!(
                                                        "remove-jump-host-{index}"
                                                    )),
                                                    IconName::Close,
                                                )
                                                .icon_size(IconSize::XSmall)
                                                .icon_color(Color::Muted)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.remove_form_jump_host(index, cx)
                                                })),
                                            )
                                            .into_any_element()
                                    },
                                ))
                                .when(state.jump_hosts.is_empty(), |this| {
                                    this.child(
                                        Label::new(i18n::t!("98466a48cb101185"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                })
                                .child(
                                    Button::new(
                                        "add-jump-host",
                                        i18n::t!("a1e9b8ca998838e8"),
                                    )
                                    .size(ButtonSize::Default)
                                    .label_size(LabelSize::Small)
                                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_jump_host_picker(window, cx)
                                    })),
                                )
                                .when_some(state.jump_host_picker.clone(), |this, picker| {
                                    this.child(
                                        div()
                                            .h(rems(12.))
                                            .border_1()
                                            .border_color(border_color)
                                            .rounded_md()
                                            .overflow_hidden()
                                            .child(picker),
                                    )
                                }),
                        )
                        .child(
                            v_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .gap_2()
                                        .child(Label::new(i18n::t!("48b970b568a7f8f9")))
                                        .child(
                                            Checkbox::new(
                                                "disable-proxy",
                                                ToggleState::from(state.proxy_disabled),
                                            )
                                            .label(i18n::t!("6b8f6c6cb99b1f89"))
                                            .label_size(LabelSize::XSmall)
                                            .on_click(cx.listener(
                                                |this, toggle: &ToggleState, _, cx| {
                                                    if let Mode::CreateRemoteServerForm(state) =
                                                        &mut this.mode
                                                    {
                                                        state.proxy_disabled = toggle.selected();
                                                    }
                                                    cx.notify();
                                                },
                                            )),
                                        ),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "form-input-proxy".into())
                                        .child(form_input_surface(&state.proxy_editor, false, cx)),
                                )
                                .child(
                                    Label::new(i18n::t!("df7f8656cf9d962c"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        ),
                )
            });

        let banner = h_flex()
            .w_full()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .when_some(state.error.clone(), |this, error| {
                this.bg(cx.theme().status().error_background)
                    .border_1()
                    .border_color(cx.theme().status().error_border)
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(error)
                            .size(LabelSize::Small)
                            .color(Color::Error)
                            .flex_1(),
                    )
            });

        let footer = h_flex()
            .w_full()
            .p_2()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(border_color)
            .child(div().flex_1())
            .child(
                Button::new("back-from-form", i18n::t!("572cf45ba43634b3"))
                    .style(ButtonStyle::Transparent)
                    .color(Color::Muted)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                        this.focus_handle(cx).focus(window, cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("connect-form", i18n::t!("a5574109f0208e89"))
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .start_icon(Icon::new(IconName::ArrowRight).size(IconSize::XSmall))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.confirm_create_remote_server_form(window, cx)
                    })),
            );

        v_flex()
            .track_focus(&self.focus_handle(cx))
            .id("create-remote-server-form")
            .size_full()
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(path) = paths.paths().first() {
                    this.set_form_key_file(path.clone(), cx);
                }
            }))
            .child(
                div()
                    .id("create-remote-server-form-fields")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .p_3()
                            .child(connection_fields)
                            .child(advanced)
                            .child(banner),
                    ),
            )
            .child(footer)
            .into_any_element()
    }

    #[cfg(target_os = "windows")]
    fn render_add_wsl_distro(
        &self,
        state: &AddWslDistro,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let connection_prompt = state.connection_prompt.clone();

        state.picker.update(cx, |picker, cx| {
            picker.focus_handle(cx).focus(window, cx);
        });

        v_flex()
            .id("add-wsl-distro")
            .overflow_hidden()
            .size_full()
            .flex_1()
            .map(|this| {
                if let Some(connection_prompt) = connection_prompt {
                    this.child(connection_prompt)
                } else {
                    this.child(state.picker.clone())
                }
            })
    }

    fn render_view_options(
        &mut self,
        options: ViewServerOptionsState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let last_entry = options.entries().last().unwrap();

        let mut view = Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(match &options {
                    ViewServerOptionsState::Ssh { connection, .. } => SshConnectionHeader {
                        connection_string: connection.host.to_string().into(),
                        paths: Default::default(),
                        nickname: connection.nickname.clone().map(|s| s.into()),
                        is_wsl: false,
                        is_devcontainer: false,
                    }
                    .render(window, cx)
                    .into_any_element(),
                    ViewServerOptionsState::Wsl { connection, .. } => SshConnectionHeader {
                        connection_string: connection.distro_name.clone().into(),
                        paths: Default::default(),
                        nickname: None,
                        is_wsl: true,
                        is_devcontainer: false,
                    }
                    .render(window, cx)
                    .into_any_element(),
                })
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .map(|this| match &options {
                            ViewServerOptionsState::Ssh {
                                connection,
                                entries,
                                server_index,
                            } => this.child(self.render_edit_ssh(
                                connection,
                                *server_index,
                                entries,
                                window,
                                cx,
                            )),
                            ViewServerOptionsState::Wsl {
                                connection,
                                entries,
                                server_index,
                            } => this.child(self.render_edit_wsl(
                                connection,
                                *server_index,
                                entries,
                                window,
                                cx,
                            )),
                        })
                        .child(ListSeparator)
                        .child({
                            div()
                                .id("ssh-options-copy-server-address")
                                .track_focus(&last_entry.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                                    cx.focus_self(window);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("go-back")
                                        .toggle_state(
                                            last_entry.focus_handle.contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ArrowLeft).color(Color::Muted),
                                        )
                                        .child(Label::new(i18n::t!("572cf45ba43634b3")))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.mode =
                                                Mode::default_mode(&this.ssh_config_servers, cx);
                                            cx.focus_self(window);
                                            cx.notify()
                                        })),
                                )
                        }),
                )
                .into_any_element(),
        );

        for entry in options.entries() {
            view = view.entry(entry.clone());
        }

        view.render(window, cx).into_any_element()
    }

    fn render_edit_wsl(
        &self,
        connection: &WslConnectionOptions,
        index: WslServerIndex,
        entries: &[NavigableEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let distro_name = SharedString::new(connection.distro_name.clone());

        v_flex().child({
            fn remove_wsl_distro(
                remote_servers: Entity<RemoteServerProjects>,
                index: WslServerIndex,
                distro_name: SharedString,
                window: &mut Window,
                cx: &mut App,
            ) {
                let prompt_message = i18n::t_args!("20e976b1990be449", distro_name);

                let confirmation = window.prompt(
                    PromptLevel::Warning,
                    &prompt_message,
                    None,
                    &[i18n::t!("a0d088db90385b56"), i18n::t!("beb38893b76fefe3")],
                    cx,
                );

                cx.spawn(async move |cx| {
                    if confirmation.await.ok() == Some(0) {
                        remote_servers.update(cx, |this, cx| {
                            this.delete_wsl_distro(index, cx);
                        });
                        remote_servers.update(cx, |this, cx| {
                            this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                            cx.notify();
                        });
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
            div()
                .id("wsl-options-remove-distro")
                .track_focus(&entries[0].focus_handle)
                .on_action(cx.listener({
                    let distro_name = distro_name.clone();
                    move |_, _: &menu::Confirm, window, cx| {
                        remove_wsl_distro(cx.entity(), index, distro_name.clone(), window, cx);
                    }
                }))
                .child(
                    ListItem::new("remove-distro")
                        .toggle_state(entries[0].focus_handle.contains_focused(window, cx))
                        .inset(true)
                        .spacing(ui::ListItemSpacing::Sparse)
                        .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                        .child(Label::new(i18n::t!("c49c4ea4805cfae2")).color(Color::Error))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            remove_wsl_distro(cx.entity(), index, distro_name.clone(), window, cx);
                        })),
                )
        })
    }

    fn render_edit_ssh(
        &self,
        connection: &SshConnectionOptions,
        index: SshServerIndex,
        entries: &[NavigableEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let connection_string = SharedString::new(connection.host.to_string());

        v_flex()
            .child({
                let label = if connection.nickname.is_some() {
                    i18n::t!("6e34485973735c6d")
                } else {
                    i18n::t!("26146c158993c858")
                };
                div()
                    .id("ssh-options-add-nickname")
                    .track_focus(&entries[0].focus_handle)
                    .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                        this.mode = Mode::EditNickname(EditNicknameState::new(index, window, cx));
                        cx.notify();
                    }))
                    .child(
                        ListItem::new("add-nickname")
                            .toggle_state(entries[0].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Pencil).color(Color::Muted))
                            .child(Label::new(label))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.mode =
                                    Mode::EditNickname(EditNicknameState::new(index, window, cx));
                                cx.notify();
                            })),
                    )
            })
            .child({
                let workspace = self.workspace.clone();
                fn callback(
                    workspace: WeakEntity<Workspace>,
                    connection_string: SharedString,
                    cx: &mut App,
                ) {
                    cx.write_to_clipboard(ClipboardItem::new_string(connection_string.to_string()));
                    workspace
                        .update(cx, |this, cx| {
                            struct SshServerAddressCopiedToClipboard;
                            let notification =
                                i18n::t_args!("4d975328c6b9d747", connection_string);

                            this.show_toast(
                                Toast::new(
                                    NotificationId::composite::<SshServerAddressCopiedToClipboard>(
                                        connection_string.clone(),
                                    ),
                                    notification,
                                )
                                .autohide(),
                                cx,
                            );
                        })
                        .ok();
                }
                div()
                    .id("ssh-options-copy-server-address")
                    .track_focus(&entries[1].focus_handle)
                    .on_action({
                        let connection_string = connection_string.clone();
                        let workspace = self.workspace.clone();
                        move |_: &menu::Confirm, _, cx| {
                            callback(workspace.clone(), connection_string.clone(), cx);
                        }
                    })
                    .child(
                        ListItem::new("copy-server-address")
                            .toggle_state(entries[1].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Copy).color(Color::Muted))
                            .child(Label::new(i18n::t!("4c2614bc71a0cb77")))
                            .end_slot(Label::new(connection_string.clone()).color(Color::Muted))
                            .show_end_slot_on_hover()
                            .on_click({
                                let connection_string = connection_string.clone();
                                move |_, _, cx| {
                                    callback(workspace.clone(), connection_string.clone(), cx);
                                }
                            }),
                    )
            })
            .child({
                fn remove_ssh_server(
                    remote_servers: Entity<RemoteServerProjects>,
                    index: SshServerIndex,
                    connection_string: SharedString,
                    window: &mut Window,
                    cx: &mut App,
                ) {
                    let prompt_message = i18n::t_args!("d1fd2f8a98a29fca", connection_string);

                    let confirmation = window.prompt(
                        PromptLevel::Warning,
                        &prompt_message,
                        None,
                        &[i18n::t!("a0d088db90385b56"), i18n::t!("beb38893b76fefe3")],
                        cx,
                    );

                    cx.spawn(async move |cx| {
                        if confirmation.await.ok() == Some(0) {
                            remote_servers.update(cx, |this, cx| {
                                this.delete_ssh_server(index, cx);
                            });
                            remote_servers.update(cx, |this, cx| {
                                this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                                cx.notify();
                            });
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }
                div()
                    .id("ssh-options-copy-server-address")
                    .track_focus(&entries[2].focus_handle)
                    .on_action(cx.listener({
                        let connection_string = connection_string.clone();
                        move |_, _: &menu::Confirm, window, cx| {
                            remove_ssh_server(
                                cx.entity(),
                                index,
                                connection_string.clone(),
                                window,
                                cx,
                            );
                        }
                    }))
                    .child(
                        ListItem::new("remove-server")
                            .toggle_state(entries[2].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                            .child(Label::new(i18n::t!("cc98dc08f5e9a890")).color(Color::Error))
                            .on_click(cx.listener(move |_, _, window, cx| {
                                remove_ssh_server(
                                    cx.entity(),
                                    index,
                                    connection_string.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
            })
    }

    fn render_edit_nickname(
        &self,
        state: &EditNicknameState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(connection) = RemoteSettings::get_global(cx)
            .ssh_connections()
            .nth(state.index.0)
        else {
            return v_flex()
                .id("ssh-edit-nickname")
                .track_focus(&self.focus_handle(cx));
        };

        let connection_string = connection.host.clone();
        let nickname = connection.nickname.map(|s| s.into());

        v_flex()
            .id("ssh-edit-nickname")
            .track_focus(&self.focus_handle(cx))
            .child(
                SshConnectionHeader {
                    connection_string: connection_string.into(),
                    paths: Default::default(),
                    nickname,
                    is_wsl: false,
                    is_devcontainer: false,
                }
                .render(window, cx),
            )
            .child(
                h_flex()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(state.editor.clone()),
            )
    }

    fn render_default(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .min_h(rems(20.))
            .size_full()
            .child(self.default_picker.clone())
            .into_any_element()
    }

    fn manage_ssh_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let keys = match remote::list_managed_ssh_keys() {
            Ok(keys) => keys,
            Err(error) => {
                let confirmation = window.prompt(
                    PromptLevel::Critical,
                    i18n::t!("93d0989ced68c7dc"),
                    Some(&error.to_string()),
                    &[i18n::t!("fac2a67ad87807c4")],
                    cx,
                );
                cx.spawn(async move |_, _| {
                    confirmation.await.ok();
                })
                .detach();
                return;
            }
        };
        if keys.is_empty() {
            let confirmation = window.prompt(
                PromptLevel::Info,
                i18n::t!("641dd3307518e1b9"),
                Some(i18n::t!("5021a7dc57179f13")),
                &[i18n::t!("fac2a67ad87807c4")],
                cx,
            );
            cx.spawn(async move |_, _| {
                confirmation.await.ok();
            })
            .detach();
            return;
        }

        let summary = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                i18n::t_args!(
                    "7ba7b5706ee084b5",
                    index + 1,
                    key.remote_username,
                    key.host,
                    key.port,
                    key.created_at,
                    key.last_used_at
                        .as_deref()
                        .unwrap_or(i18n::t!("5594f4797ae55d1a")),
                    key.key_id,
                    match key.deployment_state {
                        remote::ManagedSshKeyDeploymentState::Pending =>
                            i18n::t!("76a2e875d985af84"),
                        remote::ManagedSshKeyDeploymentState::Verified =>
                            i18n::t!("0a1b6f1b57f5198e"),
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let mut target_buttons = vec![i18n::t!("3fd47edce45b3603").to_string()];
            target_buttons.extend(
                keys.iter()
                    .map(|key| format!("{}@{}:{}", key.remote_username, key.host, key.port)),
            );
            let target_button_refs = target_buttons
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let selected = cx
                .prompt(
                    PromptLevel::Info,
                    i18n::t!("9d823228218052ab"),
                    Some(&summary),
                    &target_button_refs,
                )
                .await?;
            if selected == 0 {
                return Ok::<(), anyhow::Error>(());
            }
            let Some(key) = keys.get(selected - 1) else {
                anyhow::bail!(i18n::t!("28d8d3cf78592487"));
            };
            let answer = cx
                .prompt(
                    PromptLevel::Warning,
                    &i18n::t_args!("e56ec9128d0390c3", key.remote_username, key.host, key.port),
                    Some(i18n::t!("d0b7e7ca2da217f2")),
                    &[
                        i18n::t!("2cd0f3be8738a86c"),
                        i18n::t!("6f0bde02c11998ef"),
                        i18n::t!("56b27cbf4eea18e2"),
                    ],
                )
                .await?;
            match answer {
                1 => remote::revoke_and_delete_managed_ssh_key(&key.key_id, cx).await?,
                2 => remote::delete_local_managed_ssh_key(&key.key_id, cx).await?,
                _ => return Ok(()),
            }
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<ManagedSshKeyManagementToast>(),
                            i18n::t!("ef5c1f17c13b506f"),
                        )
                        .autohide(),
                        cx,
                    );
                });
            }
            Ok(())
        })
        .detach_and_prompt_err(i18n::t!("d8732bec6533c2cc"), window, cx, |_, _, _| None);
    }

    fn edit_local_ssh_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        cx.spawn_in(window, async move |_, cx| {
            let path = user_ssh_config_file();
            let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone())?;
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!(i18n::t!("7381f51442c15e86")))?;
            fs.create_dir(parent).await?;
            fs.create_file(
                &path,
                fs::CreateOptions {
                    ignore_if_exists: true,
                    ..Default::default()
                },
            )
            .await?;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.with_local_workspace(window, cx, move |workspace, window, cx| {
                        workspace.open_abs_path(
                            path,
                            OpenOptions {
                                visible: Some(workspace::OpenVisible::None),
                                ..Default::default()
                            },
                            window,
                            cx,
                        )
                    })
                })?
                .await?
                .await?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err(i18n::t!("1f90e2131dc4784b"), window, cx, |_, _, _| None);
    }

    /// Settings identity of a picker row, saving an SSH-config-only host first
    /// so its per-server actions have an entry to read and write.
    fn save_picker_server(
        &mut self,
        picker_server: PickerServer,
        cx: &mut Context<'_, Self>,
    ) -> (ServerIndex, Connection) {
        match picker_server {
            PickerServer::Saved { index, connection } => (index, connection),
            PickerServer::UnsavedSshConfig { host } => {
                let connection = Connection::Ssh(SshConnection {
                    host: host.to_string(),
                    ..SshConnection::default()
                });
                let index = self.create_host_from_ssh_config(&host, cx);
                (index.into(), connection)
            }
        }
    }

    fn create_host_from_ssh_config(
        &mut self,
        ssh_config_host: &SharedString,
        cx: &mut Context<'_, Self>,
    ) -> SshServerIndex {
        let new_ix = RemoteSettings::get_global(cx).ssh_connections().count();

        self.add_ssh_server(
            SshConnectionOptions {
                host: ssh_config_host.to_string().into(),
                ..SshConnectionOptions::default()
            },
            cx,
        );
        self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
        SshServerIndex(new_ix)
    }
}

fn spawn_ssh_config_watch(
    fs: Arc<dyn Fs>,
    window: &mut Window,
    cx: &Context<RemoteServerProjects>,
) -> Task<()> {
    enum ConfigSource {
        User(String),
        Global(String),
    }

    let mut streams = Vec::new();
    let mut tasks = Vec::new();

    // Setup User Watcher
    let user_path = user_ssh_config_file();
    info!("SSH: Watching User Config at: {:?}", user_path);

    // We clone 'fs' here because we might need it again for the global watcher.
    let (user_s, user_t) = watch_config_file(cx.background_executor(), fs.clone(), user_path);
    streams.push(user_s.map(ConfigSource::User).boxed());
    tasks.push(user_t);

    // Setup Global Watcher
    if let Some(gp) = global_ssh_config_file() {
        info!("SSH: Watching Global Config at: {:?}", gp);
        let (global_s, global_t) =
            watch_config_file(cx.background_executor(), fs, gp.to_path_buf());
        streams.push(global_s.map(ConfigSource::Global).boxed());
        tasks.push(global_t);
    } else {
        debug!("SSH: No Global Config defined.");
    }

    // Combine into a single stream so that only one is parsed at once.
    let mut merged_stream = futures::stream::select_all(streams);

    cx.spawn_in(window, async move |remote_server_projects, cx| {
        let _tasks = tasks; // Keeps the background watchers alive
        let mut global_hosts = BTreeSet::default();
        let mut user_hosts = BTreeSet::default();

        while let Some(event) = merged_stream.next().await {
            match event {
                ConfigSource::Global(content) => {
                    global_hosts = parse_ssh_config_hosts(&content);
                }
                ConfigSource::User(content) => {
                    user_hosts = parse_ssh_config_hosts(&content);
                }
            }

            // Sync to Model
            if remote_server_projects
                .update_in(cx, |project, window, cx| {
                    project.ssh_config_servers = global_hosts
                        .iter()
                        .chain(user_hosts.iter())
                        .map(SharedString::from)
                        .collect();
                    project.refresh_default_picker(window, cx);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        }
    })
}

fn get_text(element: &Entity<Editor>, cx: &mut App) -> String {
    element.read(cx).text(cx).trim().to_string()
}

impl ModalView for RemoteServerProjects {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(self.allow_dismissal)
    }
}

impl Focusable for RemoteServerProjects {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.mode {
            Mode::Default => self.default_picker.focus_handle(cx),
            Mode::ProjectPicker(picker) => picker.focus_handle(cx),
            _ => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for RemoteServerProjects {}

impl RemoteServerProjects {
    /// The two-column add-server form needs the wider modal, while every other
    /// mode renders a picker or a single column at the standard picker width.
    /// Every mode shares one container, so a width set for the form would leave
    /// the remaining modes with an empty band on the right.
    fn modal_width(&self) -> Rems {
        match &self.mode {
            Mode::CreateRemoteServerForm(_) => rems(40.),
            _ => picker::DEFAULT_MODAL_WIDTH,
        }
    }
}

impl Render for RemoteServerProjects {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .elevation_3(cx)
            .w(self.modal_width())
            .key_context("RemoteServerModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if matches!(this.mode, Mode::Default) {
                    cx.emit(DismissEvent)
                }
            }))
            .child(match &self.mode {
                Mode::Default => self.render_default(window, cx).into_any_element(),
                Mode::ViewServerOptions(state) => self
                    .render_view_options(state.clone(), window, cx)
                    .into_any_element(),
                Mode::ProjectPicker(element) => element.clone().into_any_element(),
                Mode::CreateRemoteServer(state) => self
                    .render_create_remote_server(state, window, cx)
                    .into_any_element(),
                Mode::CreateRemoteServerForm(state) => self
                    .render_create_remote_server_form(state, window, cx)
                    .into_any_element(),
                Mode::CreateRemoteDevContainer(state) => self
                    .render_create_dev_container(state, window, cx)
                    .into_any_element(),
                Mode::EditNickname(state) => self
                    .render_edit_nickname(state, window, cx)
                    .into_any_element(),
                #[cfg(target_os = "windows")]
                Mode::AddWslDistro(state) => self
                    .render_add_wsl_distro(state, window, cx)
                    .into_any_element(),
            })
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn ssh_config_entry(host: &'static str) -> RemoteEntry {
        RemoteEntry::SshConfig {
            host: SharedString::from(host),
        }
    }

    #[test]
    fn ssh_host_rows_expose_server_options_and_terminal_mode() {
        let entries = vec![RemoteEntry::Project {
            projects: Vec::new(),
            connection: Connection::Ssh(SshConnection {
                host: "example.com".into(),
                ..Default::default()
            }),
            index: ServerIndex::Ssh(SshServerIndex(0)),
        }];
        let mut delegate = RemoteServerPickerDelegate {
            remote_server_projects: WeakEntity::new_invalid(),
            state: DefaultState {
                filter_data: Arc::new(FilterData::build(&entries)),
                servers: entries,
                filtered_servers: None,
            },
            focused_server: None,
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
            has_open_project: false,
            is_local: true,
        };
        delegate.rebuild_matches();
        assert!(
            !delegate
                .matches
                .iter()
                .any(|entry| matches!(entry, RemoteMatch::ViewServerOptions { .. })),
            "per-server actions live in the server's own view"
        );

        delegate.focus_server(0);
        assert!(delegate.matches.windows(2).any(|entries| matches!(
            entries,
            [
                RemoteMatch::ViewServerOptions { server: 0 },
                RemoteMatch::RemoteTerminal { server: 0 }
            ]
        )));
    }

    #[test]
    fn test_filter_sync_repopulates_after_rebuild() {
        let entries = vec![ssh_config_entry("alpha"), ssh_config_entry("beta")];
        let mut state = DefaultState {
            filter_data: Arc::new(FilterData::build(&entries)),
            servers: entries,
            filtered_servers: None,
        };

        state.filter_sync("alp");
        let filtered = state.filtered_servers.as_ref().expect("should filter");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].server_index, 0);
        assert!(!filtered[0].host_positions.is_empty());

        // The filtered index resolves back into the original server list.
        match &state.servers[filtered[0].server_index] {
            RemoteEntry::SshConfig { host, .. } => assert_eq!(host.as_ref(), "alpha"),
            _ => panic!("expected SshConfig"),
        }

        state.filter_sync("");
        assert!(state.filtered_servers.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    #[gpui::test]
    async fn test_edit_local_ssh_config_creates_and_preserves_file(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, workspace, cx))
        });
        modal.update(cx, |modal, cx| {
            let picker = modal.default_picker.read(cx);
            assert!(
                picker
                    .delegate
                    .matches
                    .iter()
                    .any(|entry| matches!(entry, RemoteMatch::EditSshConfig))
            );
        });
        modal.update_in(cx, |modal, window, cx| {
            modal.edit_local_ssh_config(window, cx)
        });
        cx.run_until_parked();
        let path = user_ssh_config_file();
        assert_eq!(fs.load(&path).await.unwrap(), "");
        assert!(workspace.read_with(cx, |workspace, cx| workspace.active_item(cx).is_some()));

        let content = "Host example\n    HostName example.com\n";
        fs.atomic_write(path.clone(), content.to_owned())
            .await
            .unwrap();
        modal.update_in(cx, |modal, window, cx| {
            modal.edit_local_ssh_config(window, cx)
        });
        cx.run_until_parked();
        assert_eq!(fs.load(&path).await.unwrap(), content);
    }

    #[gpui::test]
    async fn test_ssh_config_changes_refresh_rendered_hosts(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();
        let config_path = user_ssh_config_file();
        fs.create_dir(config_path.parent().expect("SSH config has a parent"))
            .await
            .expect("create SSH config directory");
        fs.insert_file(&config_path, Vec::new()).await;

        let project = Project::test(fs.clone(), [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::test_new(project, window, cx));
            MultiWorkspace::new(workspace, window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new(false, fs.clone(), window, weak, cx)
            });
        });
        cx.run_until_parked();

        // Settle the initial picker refresh before discovery changes its item count.
        fs.insert_file(&config_path, b"Host alpha beta\n".to_vec())
            .await;
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-server-alpha").is_some());
        assert!(cx.debug_bounds("remote-server-beta").is_some());

        fs.insert_file(&config_path, b"Host beta\n".to_vec()).await;
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-server-alpha").is_none());
        assert!(cx.debug_bounds("remote-server-beta").is_some());

        fs.insert_file(&config_path, Vec::new()).await;
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-server-beta").is_none());
    }

    #[gpui::test]
    async fn test_ssh_config_changes_refresh_filtered_hosts(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();
        let config_path = user_ssh_config_file();
        fs.create_dir(config_path.parent().expect("SSH config has a parent"))
            .await
            .expect("create SSH config directory");
        fs.insert_file(&config_path, b"Host beta\n".to_vec()).await;

        let project = Project::test(fs.clone(), [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::test_new(project, window, cx));
            MultiWorkspace::new(workspace, window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let modal = workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new(false, fs.clone(), window, weak, cx)
            });
            workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("remote projects modal is open")
        });
        cx.run_until_parked();
        modal.update_in(cx, |modal, window, cx| {
            modal.default_picker.update(cx, |picker, cx| {
                picker.set_query("be", window, cx);
            });
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-server-beta").is_some());

        fs.insert_file(&config_path, b"Host alpha beta berry\n".to_vec())
            .await;
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-server-alpha").is_none());
        assert!(cx.debug_bounds("remote-server-beta").is_some());
        assert!(cx.debug_bounds("remote-server-berry").is_some());

        fs.insert_file(&config_path, b"Host alpha berry\n".to_vec())
            .await;
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-server-alpha").is_none());
        assert!(cx.debug_bounds("remote-server-beta").is_none());
        assert!(cx.debug_bounds("remote-server-berry").is_some());
        assert_eq!(
            modal.read_with(cx, |modal, cx| modal.default_picker.read(cx).query(cx)),
            "be"
        );
    }

    #[gpui::test]
    async fn test_create_host_from_ssh_config_returns_new_connection_index(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs: Arc<dyn Fs> = app_state.fs.clone();
        cx.update(|cx| {
            release_channel::init_test(
                semver::Version::new(1, 25, 0),
                release_channel::ReleaseChannel::Stable,
                cx,
            );
            cx.set_global(release_channel::CustomReleaseTag(
                "zed-cn-v1.25.0-r1".to_owned(),
            ));
        });

        cx.update(|cx| {
            update_settings_file(fs.clone(), cx, |settings, _| {
                settings.remote.ssh_connections = Some(vec![SshConnection {
                    host: "host-a.example".to_string(),
                    projects: vec![RemoteProject {
                        paths: vec!["/path/to/project-a".to_string()],
                    }],
                    ..Default::default()
                }]);
            });
        });
        cx.run_until_parked();

        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let weak = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, weak, cx))
        });

        let host_b = SharedString::from("host-b.example");
        let new_index = modal.update(cx, |modal, cx| {
            modal.create_host_from_ssh_config(&host_b, cx)
        });
        cx.run_until_parked();

        let connections = cx.update(|_, cx| {
            RemoteSettings::get_global(cx)
                .ssh_connections()
                .collect::<Vec<_>>()
        });

        assert_eq!(connections.len(), 2);
        assert_eq!(connections[0].host, "host-a.example");
        assert_eq!(connections[1].host, "host-b.example");
        assert_eq!(
            connections[new_index.0].host, "host-b.example",
            "returned index should point at the newly created host"
        );

        assert_eq!(connections[0].projects.len(), 1);
        assert!(connections[new_index.0].projects.is_empty());
        assert_eq!(
            connections[new_index.0].upload_binary_over_ssh, None,
            "an implicitly added SSH-config host keeps the default upload mode"
        );
    }

    #[gpui::test]
    async fn test_save_picker_server_reuses_a_saved_host_and_saves_an_ssh_config_host(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs: Arc<dyn Fs> = app_state.fs.clone();

        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let weak = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, weak, cx))
        });

        let saved_connection = Connection::Ssh(SshConnection {
            host: "host-a.example".to_string(),
            ..Default::default()
        });
        let (index, connection) = modal.update(cx, |modal, cx| {
            modal.save_picker_server(
                PickerServer::Saved {
                    index: ServerIndex::Ssh(SshServerIndex(0)),
                    connection: saved_connection.clone(),
                },
                cx,
            )
        });
        assert_eq!(index, ServerIndex::Ssh(SshServerIndex(0)));
        assert!(connection == saved_connection);

        let (index, connection) = modal.update(cx, |modal, cx| {
            modal.save_picker_server(
                PickerServer::UnsavedSshConfig {
                    host: SharedString::from("host-b.example"),
                },
                cx,
            )
        });
        cx.run_until_parked();

        assert_eq!(index, ServerIndex::Ssh(SshServerIndex(0)));
        assert!(
            matches!(connection, Connection::Ssh(ref connection) if connection.host == "host-b.example"),
            "an SSH-config host is saved and returned as a saved connection"
        );

        let connections = cx.update(|_, cx| {
            RemoteSettings::get_global(cx)
                .ssh_connections()
                .collect::<Vec<_>>()
        });
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].host, "host-b.example");
    }

    #[gpui::test]
    async fn test_add_server_form_validates_fields(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, workspace, cx))
        });

        modal.read_with(cx, |modal, cx| {
            assert!(
                modal
                    .default_picker
                    .read(cx)
                    .delegate
                    .matches
                    .iter()
                    .any(|entry| matches!(entry, RemoteMatch::AddServerForm)),
                "the picker offers the form entry alongside the one-line entry"
            );
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.mode = Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
            let Mode::CreateRemoteServerForm(state) = &modal.mode else {
                panic!("expected the form mode");
            };

            assert!(
                modal.read_form_draft(cx).is_err(),
                "an empty address must be rejected"
            );

            state.host_editor.update(cx, |editor, cx| {
                editor.set_text("-oProxyCommand=evil", window, cx)
            });
            assert!(
                modal.read_form_draft(cx).is_err(),
                "an address is never allowed to look like an ssh option"
            );

            state.host_editor.update(cx, |editor, cx| {
                editor.set_text("target.example", window, cx)
            });
            state.port_editor.update(cx, |editor, cx| {
                editor.set_text("not-a-port", window, cx)
            });
            assert!(modal.read_form_draft(cx).is_err());

            state.port_editor.update(cx, |editor, cx| {
                editor.set_text("2222", window, cx)
            });
            state.proxy_editor.update(cx, |editor, cx| {
                editor.set_text("ftp://127.0.0.1:21", window, cx)
            });
            assert!(modal.read_form_draft(cx).is_err());

            state.proxy_editor.update(cx, |editor, cx| {
                editor.set_text("socks5://127.0.0.1:1080", window, cx)
            });
            state.timeout_editor.update(cx, |editor, cx| {
                editor.set_text("30", window, cx)
            });
            let draft = modal.read_form_draft(cx).expect("a valid draft");
            assert_eq!(draft.host, "target.example");
            assert_eq!(draft.port, Some(2222));
            assert_eq!(draft.connection_timeout, Some(30));
            assert_eq!(draft.proxy.as_deref(), Some("socks5://127.0.0.1:1080"));
        });
    }

    #[gpui::test]
    async fn test_modal_width_follows_mode(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, workspace, cx))
        });

        modal.read_with(cx, |modal, _cx| {
            assert!(
                modal.modal_width() == picker::DEFAULT_MODAL_WIDTH,
                "the default picker keeps the standard picker width instead of leaving an empty band on the right"
            );
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.mode = Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
            assert!(
                modal.modal_width() == rems(40.),
                "the two-column form still gets the wider modal"
            );
        });
    }

    #[gpui::test]
    async fn test_add_server_form_persists_and_updates_one_entry(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, workspace, cx))
        });

        modal.update(cx, |modal, cx| {
            let options = SshConnectionOptions {
                host: "target.example".into(),
                username: Some("dev".into()),
                port: Some(2222),
                nickname: Some("测试主机".into()),
                auth_method: Some(SshAuthMethod::Key),
                ssh_key_id: Some("key-id".into()),
                jump_hosts: vec![JumpHost {
                    host: "jump.example".into(),
                    username: Some("ops".into()),
                    port: None,
                }],
                proxy: Some("socks5://127.0.0.1:1080".into()),
                connection_timeout: Some(30),
                ..Default::default()
            };

            let index = modal.persist_form_connection(&options, None, cx);
            assert_eq!(index, SshServerIndex(0));
            let updated = modal.persist_form_connection(&options, Some(index), cx);
            assert_eq!(updated, index);
        });
        cx.run_until_parked();

        cx.update(|_, cx| {
            let connections = RemoteSettings::get_global(cx)
                .ssh_connections()
                .collect::<Vec<_>>();
            assert_eq!(
                connections.len(),
                1,
                "retrying must update the saved entry instead of duplicating it"
            );
            let connection = &connections[0];
            assert_eq!(connection.nickname.as_deref(), Some("测试主机"));
            assert_eq!(connection.auth_method, Some(SshAuthMethod::Key));
            assert_eq!(connection.ssh_key_id.as_deref(), Some("key-id"));
            assert_eq!(connection.jump_hosts.len(), 1);
            assert_eq!(connection.jump_hosts[0].host, "jump.example");
            assert_eq!(connection.jump_hosts[0].username.as_deref(), Some("ops"));
            assert_eq!(
                connection.proxy.as_deref(),
                Some("socks5://127.0.0.1:1080")
            );
            assert_eq!(connection.connection_timeout, Some(30));
        });
    }

    #[gpui::test]
    async fn test_add_server_form_reports_the_host_error_inline(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, workspace, cx))
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.mode = Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
            modal.confirm_create_remote_server_form(window, cx);
            let Mode::CreateRemoteServerForm(state) = &modal.mode else {
                panic!("expected the form mode");
            };
            assert_eq!(
                state.host_error.as_deref(),
                Some(i18n::t!("a4339e03ef00d39d")),
                "an empty host must be flagged next to the field"
            );
        });

        modal.update_in(cx, |modal, window, cx| {
            let Mode::CreateRemoteServerForm(state) = &modal.mode else {
                panic!("expected the form mode");
            };
            state.host_editor.update(cx, |editor, cx| {
                editor.set_text("target.example", window, cx)
            });
            modal.confirm_create_remote_server_form(window, cx);
            let Mode::CreateRemoteServerForm(state) = &modal.mode else {
                panic!("expected the form mode");
            };
            assert!(
                state.host_error.is_none(),
                "a valid host must clear the inline error"
            );
        });
    }

    #[gpui::test]
    async fn test_add_server_form_previews_the_ssh_command(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, workspace, cx))
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.mode = Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
            let Mode::CreateRemoteServerForm(state) = &mut modal.mode else {
                panic!("expected the form mode");
            };

            assert_eq!(
                state.ssh_preview(cx),
                None,
                "an empty host has nothing to preview"
            );

            state.host_editor.update(cx, |editor, cx| {
                editor.set_text("target.example", window, cx)
            });
            state
                .username_editor
                .update(cx, |editor, cx| editor.set_text("dev", window, cx));
            state
                .port_editor
                .update(cx, |editor, cx| editor.set_text("2222", window, cx));
            assert_eq!(
                state.ssh_preview(cx).as_deref(),
                Some("ssh dev@target.example -p 2222")
            );

            state.jump_hosts = vec![JumpHost {
                host: "jump.example".into(),
                username: Some("ops".into()),
                port: None,
            }];
            state.proxy_editor.update(cx, |editor, cx| {
                editor.set_text("socks5://127.0.0.1:1080", window, cx)
            });
            assert_eq!(
                state.ssh_preview(cx).as_deref(),
                Some("ssh dev@target.example -p 2222 (socks5://127.0.0.1:1080) (ops@jump.example)")
            );

            state.proxy_disabled = true;
            assert_eq!(
                state.ssh_preview(cx).as_deref(),
                Some("ssh dev@target.example -p 2222 (ops@jump.example)"),
                "a disabled proxy is left out of the preview"
            );
        });
    }

    #[gpui::test]
    async fn test_add_server_form_renders_inputs_and_collapses_advanced_options(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();
        let project = Project::test(fs.clone(), [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::test_new(project, window, cx));
            MultiWorkspace::new(workspace, window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new(false, fs.clone(), window, weak, cx)
            });
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            let modal = workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("the remote server modal is open");
            modal.update(cx, |modal, cx| {
                modal.mode = Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
                cx.notify();
            });
        });
        cx.run_until_parked();

        for selector in [
            "form-input-nickname",
            "form-input-host",
            "form-input-port",
            "form-input-username",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} must be laid out as its own control"
            );
        }
        assert!(
            cx.debug_bounds("form-input-timeout").is_none(),
            "advanced fields stay collapsed until the section is opened"
        );
        assert!(
            cx.debug_bounds("form-ssh-preview").is_none(),
            "an empty host has nothing to preview"
        );

        // Fill the host so the derived command preview appears, then open the
        // advanced section.
        workspace.update_in(cx, |workspace, window, cx| {
            let modal = workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("the remote server modal is open");
            modal.update(cx, |modal, cx| {
                let Mode::CreateRemoteServerForm(state) = &modal.mode else {
                    panic!("expected the form mode");
                };
                state.host_editor.update(cx, |editor, cx| {
                    editor.set_text("target.example", window, cx)
                });
                cx.notify();
            });
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("form-ssh-preview").is_some(),
            "the ssh command preview follows the host field"
        );

        workspace.update_in(cx, |workspace, _window, cx| {
            let modal = workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("the remote server modal is open");
            modal.update(cx, |modal, cx| {
                let Mode::CreateRemoteServerForm(state) = &mut modal.mode else {
                    panic!("expected the form mode");
                };
                state.advanced_open = true;
                cx.notify();
            });
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("form-input-timeout").is_some(),
            "opening the section reveals the timeout field"
        );
        assert!(
            cx.debug_bounds("form-input-proxy").is_some(),
            "opening the section reveals the proxy field"
        );
    }

    #[gpui::test]
    async fn test_add_server_form_explains_the_default_key_for_key_auth(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();
        let project = Project::test(fs.clone(), [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::test_new(project, window, cx));
            MultiWorkspace::new(workspace, window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new(false, fs.clone(), window, weak, cx)
            });
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            let modal = workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("the remote server modal is open");
            modal.update(cx, |modal, cx| {
                modal.mode = Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
                modal.set_form_auth_method(SshAuthMethod::Key, cx);
            });
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("form-key-default-hint").is_some(),
            "key auth without a key file explains that the local default keys are used"
        );

        workspace.update_in(cx, |workspace, _window, cx| {
            let modal = workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("the remote server modal is open");
            modal.update(cx, |modal, cx| {
                let Mode::CreateRemoteServerForm(state) = &mut modal.mode else {
                    panic!("expected the form mode");
                };
                state.key_file = Some(PathBuf::from("/home/user/.ssh/id_ed25519"));
                cx.notify();
            });
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("form-key-default-hint").is_none(),
            "a chosen key file replaces the default-key hint"
        );

        workspace.update_in(cx, |workspace, _window, cx| {
            let modal = workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("the remote server modal is open");
            modal.update(cx, |modal, cx| {
                let Mode::CreateRemoteServerForm(state) = &mut modal.mode else {
                    panic!("expected the form mode");
                };
                state.key_file = None;
                state.key_id = Some("key-id".into());
                cx.notify();
            });
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("form-key-default-hint").is_none(),
            "an already imported key also keeps the default-key hint hidden"
        );

        workspace.update_in(cx, |workspace, _window, cx| {
            let modal = workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("the remote server modal is open");
            modal.update(cx, |modal, cx| {
                modal.set_form_auth_method(SshAuthMethod::Password, cx);
            });
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("form-key-default-hint").is_none(),
            "the default-key hint only belongs to key authentication"
        );
    }

    #[gpui::test]
    async fn test_add_server_form_summarizes_non_default_advanced_values(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, workspace, cx))
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.mode = Mode::CreateRemoteServerForm(CreateRemoteServerForm::new(window, cx));
            let Mode::CreateRemoteServerForm(state) = &mut modal.mode else {
                panic!("expected the form mode");
            };

            assert_eq!(
                state.advanced_summary(cx),
                None,
                "a collapsed section with defaults shows no badge"
            );

            state.jump_hosts = vec![JumpHost {
                host: "jump.example".into(),
                username: None,
                port: None,
            }];
            state.proxy_editor.update(cx, |editor, cx| {
                editor.set_text("socks5://127.0.0.1:1080", window, cx)
            });
            assert_eq!(
                state.advanced_summary(cx).as_deref(),
                Some(
                    format!(
                        "{} 1 · {}",
                        i18n::t!("1031cfcb802afd94"),
                        i18n::t!("48b970b568a7f8f9")
                    )
                    .as_str()
                ),
                "every non-default advanced value stays visible while collapsed"
            );
        });
    }
}

#[cfg(test)]
mod picker_view_tests {
    use super::*;

    fn project_entry(path: &str) -> ProjectEntry {
        ProjectEntry {
            project: RemoteProject {
                paths: vec![path.to_string()],
            },
        }
    }

    fn ssh_entry(index: usize, host: &str, projects: Vec<ProjectEntry>) -> RemoteEntry {
        RemoteEntry::Project {
            projects,
            connection: Connection::Ssh(SshConnection {
                host: host.into(),
                ..Default::default()
            }),
            index: ServerIndex::Ssh(SshServerIndex(index)),
        }
    }

    fn test_delegate(entries: Vec<RemoteEntry>) -> RemoteServerPickerDelegate {
        RemoteServerPickerDelegate {
            remote_server_projects: WeakEntity::new_invalid(),
            state: DefaultState {
                filter_data: Arc::new(FilterData::build(&entries)),
                servers: entries,
                filtered_servers: None,
            },
            focused_server: None,
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
            has_open_project: false,
            is_local: true,
        }
    }

    /// Applies a query the way a keystroke does: filter, then rebuild.
    fn search(delegate: &mut RemoteServerPickerDelegate, query: &str) {
        delegate.query = query.to_string();
        delegate.state.filter_sync(query);
        delegate.rebuild_matches();
    }

    #[test]
    fn record_remote_project_is_bounded_and_most_recent_first() {
        let mut projects = Vec::new();
        for index in 0..MAX_RECENT_PROJECTS_PER_SERVER + 2 {
            record_remote_project(
                &mut projects,
                RemoteProject {
                    paths: vec![format!("/p{index}")],
                },
            );
        }
        assert_eq!(projects.len(), MAX_RECENT_PROJECTS_PER_SERVER);
        assert_eq!(projects[0].paths, vec!["/p6".to_string()]);
        assert_eq!(
            projects[MAX_RECENT_PROJECTS_PER_SERVER - 1].paths,
            vec!["/p2".to_string()],
            "the least recently used entries are evicted first"
        );

        // Re-opening an existing location moves it to the front without
        // duplicating it.
        record_remote_project(
            &mut projects,
            RemoteProject {
                paths: vec!["/p4".to_string()],
            },
        );
        assert_eq!(projects.len(), MAX_RECENT_PROJECTS_PER_SERVER);
        assert_eq!(projects[0].paths, vec!["/p4".to_string()]);
        assert_eq!(
            projects
                .iter()
                .filter(|project| project.paths == vec!["/p4".to_string()])
                .count(),
            1
        );
    }

    #[test]
    fn default_view_shows_one_selectable_row_per_server() {
        let entries = vec![
            ssh_entry(
                0,
                "host-a.example",
                vec![project_entry("/a"), project_entry("/b")],
            ),
            RemoteEntry::SshConfig {
                host: SharedString::from("host-b.example"),
            },
        ];
        let mut delegate = test_delegate(entries);
        delegate.rebuild_matches();

        assert!(matches!(
            delegate.matches.first(),
            Some(RemoteMatch::AddServer)
        ));
        let server_rows: Vec<usize> = delegate
            .matches
            .iter()
            .filter_map(|entry| match entry {
                RemoteMatch::Server { server, .. } => Some(*server),
                _ => None,
            })
            .collect();
        assert_eq!(server_rows, vec![0, 1]);
        assert!(
            delegate.matches.iter().all(|entry| !matches!(
                entry,
                RemoteMatch::Project { .. }
                    | RemoteMatch::OpenFolder { .. }
                    | RemoteMatch::ViewServerOptions { .. }
            )),
            "the default view must not inline per-server entries: {:?}",
            delegate
                .matches
                .iter()
                .map(std::mem::discriminant)
                .collect::<Vec<_>>()
        );
        assert!(
            delegate
                .matches
                .iter()
                .all(|entry| entry.is_selectable(false)
                    || matches!(
                        entry,
                        RemoteMatch::Separator | RemoteMatch::FocusedServerHeader { .. }
                    )),
            "only separators are non-selectable in the default view"
        );
    }

    #[test]
    fn search_view_selects_servers_not_previewed_locations() {
        let entries = vec![
            ssh_entry(
                0,
                "host-a.example",
                vec![project_entry("/alpha"), project_entry("/beta")],
            ),
            ssh_entry(1, "host-b.example", vec![project_entry("/alpha")]),
        ];
        let mut delegate = test_delegate(entries);
        search(&mut delegate, "alpha");

        // A location can belong to several servers; each shows its own group,
        // but only the server rows themselves are selectable entries.
        let mut selectable_servers: Vec<usize> = delegate
            .matches
            .iter()
            .filter_map(|entry| match entry {
                RemoteMatch::Server { server, .. } if entry.is_selectable(true) => Some(*server),
                _ => None,
            })
            .collect();
        selectable_servers.sort_unstable();
        assert_eq!(selectable_servers, vec![0, 1]);
        assert_eq!(
            delegate
                .matches
                .iter()
                .filter(|entry| entry.is_selectable(true))
                .count(),
            selectable_servers.len(),
            "only the server rows are selectable while searching"
        );
        assert_eq!(
            delegate
                .matches
                .iter()
                .filter(|entry| matches!(entry, RemoteMatch::Project { .. }))
                .count(),
            2,
            "each matching server previews its matched location"
        );
        assert!(
            matches!(
                delegate.matches.get(delegate.selected_index),
                Some(RemoteMatch::Server { .. })
            ),
            "the initial selection lands on a server, not a previewed location"
        );
    }

    #[test]
    fn search_view_mirrors_the_server_view_content() {
        let entries = vec![ssh_entry(
            0,
            "host-a.example",
            vec![project_entry("/alpha")],
        )];
        let mut delegate = test_delegate(entries);
        search(&mut delegate, "alpha");

        // The group below a server lists the same entries as the server's own
        // view: Open Folder, its locations, then the per-server actions.
        for expected in [
            "open_folder",
            "project",
            "view_server_options",
            "remote_terminal",
        ] {
            let found = delegate
                .matches
                .iter()
                .any(|entry| match (expected, entry) {
                    ("open_folder", RemoteMatch::OpenFolder { .. }) => true,
                    ("project", RemoteMatch::Project { .. }) => true,
                    ("view_server_options", RemoteMatch::ViewServerOptions { .. }) => true,
                    ("remote_terminal", RemoteMatch::RemoteTerminal { .. }) => true,
                    _ => false,
                });
            assert!(found, "search view is missing {expected}");
        }
    }

    #[test]
    fn search_view_keeps_a_host_only_server_selectable() {
        let entries = vec![ssh_entry(
            0,
            "host-only.example",
            vec![project_entry("/alpha")],
        )];
        let mut delegate = test_delegate(entries);
        search(&mut delegate, "host-only");

        assert!(
            matches!(
                delegate.matches.get(delegate.selected_index),
                Some(RemoteMatch::Server { server: 0, host_positions }) if !host_positions.is_empty()
            ),
            "a host-only match keeps the server row selectable and highlighted"
        );
        // The server owns its locations, so they are still previewed below it,
        // just without any path highlight since only the host matched.
        assert!(
            delegate
                .matches
                .iter()
                .filter_map(|entry| match entry {
                    RemoteMatch::Project { positions, .. } => Some(positions),
                    _ => None,
                })
                .all(|positions| positions.is_empty()),
            "a host-only match previews locations without path highlights"
        );
    }

    #[test]
    fn drilling_into_a_searched_server_keeps_the_filter() {
        let entries = vec![ssh_entry(
            0,
            "host-a.example",
            vec![project_entry("/alpha"), project_entry("/beta")],
        )];
        let mut delegate = test_delegate(entries);
        search(&mut delegate, "beta");

        delegate.focus_server(0);

        assert!(matches!(
            delegate.matches.first(),
            Some(RemoteMatch::FocusedServerHeader { server: 0 })
        ));
        let projects: Vec<usize> = delegate
            .matches
            .iter()
            .filter_map(|entry| match entry {
                RemoteMatch::Project { project, .. } => Some(*project),
                _ => None,
            })
            .collect();
        assert_eq!(
            projects,
            vec![1],
            "the drilled-in view keeps the query and lists only matching locations"
        );
        assert!(
            matches!(
                delegate.matches.get(delegate.selected_index),
                Some(RemoteMatch::Project { project: 1, .. })
            ),
            "the matched location is preselected inside the server"
        );
    }

    #[test]
    fn focused_view_puts_open_folder_before_recent_projects() {
        let projects = (0..3)
            .map(|index| project_entry(&format!("/projects/p{index}")))
            .collect();
        let mut delegate = test_delegate(vec![ssh_entry(0, "host-a.example", projects)]);
        delegate.focus_server(0);

        assert!(matches!(
            delegate.matches.first(),
            Some(RemoteMatch::FocusedServerHeader { server: 0 })
        ));
        assert!(
            matches!(
                delegate.matches.get(1),
                Some(RemoteMatch::OpenFolder { server: 0 })
            ),
            "Open Folder is the first entry of a server's own view"
        );
        for (offset, expected_project) in (0..3).enumerate() {
            assert!(matches!(
                delegate.matches.get(2 + offset),
                Some(RemoteMatch::Project {
                    server: 0,
                    project,
                    ..
                }) if *project == expected_project
            ));
        }
        assert!(matches!(
            delegate.matches.get(5),
            Some(RemoteMatch::Separator)
        ));
        assert!(matches!(
            delegate.matches.get(6),
            Some(RemoteMatch::ViewServerOptions { server: 0 })
        ));
        assert!(matches!(
            delegate.matches.get(7),
            Some(RemoteMatch::RemoteTerminal { server: 0 })
        ));
        assert_eq!(
            delegate.selected_index, 1,
            "Open Folder is selected when entering a server's view"
        );
    }

    #[test]
    fn focused_view_for_ssh_config_host_offers_the_same_actions_as_a_saved_host() {
        let mut delegate = test_delegate(vec![RemoteEntry::SshConfig {
            host: SharedString::from("host-b.example"),
        }]);
        delegate.focus_server(0);

        assert!(matches!(
            delegate.matches.first(),
            Some(RemoteMatch::FocusedServerHeader { server: 0 })
        ));
        assert!(matches!(
            delegate.matches.get(1),
            Some(RemoteMatch::OpenFolder { server: 0 })
        ));
        assert!(matches!(
            delegate.matches.get(2),
            Some(RemoteMatch::Separator)
        ));
        assert!(matches!(
            delegate.matches.get(3),
            Some(RemoteMatch::ViewServerOptions { server: 0 })
        ));
        assert!(matches!(
            delegate.matches.get(4),
            Some(RemoteMatch::RemoteTerminal { server: 0 })
        ));
        assert_eq!(delegate.matches.len(), 5);
    }

    #[test]
    fn search_view_previews_ssh_config_host_actions_without_selecting_them() {
        let mut delegate = test_delegate(vec![RemoteEntry::SshConfig {
            host: SharedString::from("host-b.example"),
        }]);
        search(&mut delegate, "host-b");

        assert!(matches!(
            delegate.matches.first(),
            Some(RemoteMatch::Server { server: 0, .. })
        ));
        assert!(matches!(
            delegate.matches.get(1),
            Some(RemoteMatch::OpenFolder { server: 0 })
        ));
        assert!(matches!(
            delegate.matches.get(2),
            Some(RemoteMatch::Separator)
        ));
        assert!(matches!(
            delegate.matches.get(3),
            Some(RemoteMatch::ViewServerOptions { server: 0 })
        ));
        assert!(matches!(
            delegate.matches.get(4),
            Some(RemoteMatch::RemoteTerminal { server: 0 })
        ));
        assert_eq!(delegate.matches.len(), 5);
        assert!(
            delegate
                .matches
                .iter()
                .skip(1)
                .all(|entry| !entry.is_selectable(true)),
            "the actions stay read-only previews while searching"
        );
    }

    #[test]
    fn reload_re_resolves_focused_server_by_identity() {
        let entries = vec![
            ssh_entry(0, "host-a.example", Vec::new()),
            ssh_entry(1, "host-b.example", Vec::new()),
        ];
        let mut delegate = test_delegate(entries);
        delegate.focus_server(1);

        // Simulate a settings reload that reorders the server list: host-b
        // keeps its settings index (its identity) but moves to position 0.
        delegate.state.servers = vec![
            ssh_entry(1, "host-b.example", Vec::new()),
            ssh_entry(0, "host-a.example", Vec::new()),
        ];
        delegate.resolve_focus_after_reload();
        assert_eq!(
            delegate.focused_server.as_ref().map(|(index, _)| *index),
            Some(0)
        );

        // When the server disappears entirely, the focus is dropped.
        delegate.state.servers = vec![ssh_entry(0, "host-a.example", Vec::new())];
        delegate.resolve_focus_after_reload();
        assert!(delegate.focused_server.is_none());
    }
}

#[cfg(test)]
mod drill_in_tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    #[gpui::test]
    async fn test_server_drill_in_escape_back_and_display_cap(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.clone();
        cx.update(|cx| {
            update_settings_file(fs.clone(), cx, |settings, _| {
                settings.remote.ssh_connections = Some(vec![SshConnection {
                    host: "host-a.example".to_string(),
                    projects: (0..6)
                        .map(|index| RemoteProject {
                            paths: vec![format!("/projects/p{index}")],
                        })
                        .collect(),
                    ..Default::default()
                }]);
            });
        });
        cx.run_until_parked();

        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let weak = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, weak, cx))
        });

        let dismiss_count = Arc::new(AtomicUsize::new(0));
        let _dismiss_subscription = modal.update_in(cx, |_modal, _window, cx| {
            let entity = cx.entity();
            cx.subscribe(&entity, {
                let dismiss_count = dismiss_count.clone();
                move |_, _, _: &DismissEvent, _| {
                    dismiss_count.fetch_add(1, Ordering::SeqCst);
                }
            })
        });

        let picker = modal.read_with(cx, |modal, _| modal.default_picker.clone());

        // Confirming a server row opens that server's own view.
        picker.update_in(cx, |picker, window, cx| {
            let server_row = picker
                .delegate
                .matches
                .iter()
                .position(|entry| matches!(entry, RemoteMatch::Server { server: 0, .. }))
                .expect("a selectable row per server");
            picker.delegate.selected_index = server_row;
            picker.delegate.confirm(false, window, cx);
        });
        picker.update(cx, |picker, _| {
            assert!(picker.delegate.focused_server.is_some());
            assert!(matches!(
                picker.delegate.matches.get(1),
                Some(RemoteMatch::OpenFolder { server: 0 })
            ));
            let project_rows = picker
                .delegate
                .matches
                .iter()
                .filter(|entry| matches!(entry, RemoteMatch::Project { .. }))
                .count();
            assert_eq!(
                project_rows, MAX_RECENT_PROJECTS_PER_SERVER,
                "recent locations are capped for display"
            );
        });

        // Typing a query keeps filtering the focused server's own view instead
        // of leaving it, so the hierarchy stays intact.
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("host-a", window, cx);
        });
        cx.run_until_parked();
        picker.update(cx, |picker, _| {
            assert!(
                picker.delegate.focused_server.is_some(),
                "typing keeps filtering the focused server's own view"
            );
            assert!(picker.delegate.state.filtered_servers.is_some());
            assert!(
                picker
                    .delegate
                    .matches
                    .iter()
                    .any(|entry| matches!(entry, RemoteMatch::FocusedServerHeader { server: 0 })),
                "the server's own view is still shown"
            );
        });

        // Stepping back leaves the server's own view but keeps the query, so
        // the filtered server list is shown again.
        picker.update_in(cx, |picker, window, cx| {
            picker.cancel(&menu::Cancel, window, cx);
        });
        cx.run_until_parked();
        picker.update(cx, |picker, _| {
            assert!(picker.delegate.focused_server.is_none());
            assert!(
                picker
                    .delegate
                    .matches
                    .iter()
                    .any(|entry| matches!(entry, RemoteMatch::Server { server: 0, .. })),
                "the filtered server list is shown again"
            );
        });
        assert_eq!(dismiss_count.load(Ordering::SeqCst), 0);

        // Re-enter the server's view, then step back with Cancel instead of
        // dismissing the modal.
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("", window, cx);
        });
        cx.run_until_parked();
        picker.update_in(cx, |picker, window, cx| {
            let server_row = picker
                .delegate
                .matches
                .iter()
                .position(|entry| matches!(entry, RemoteMatch::Server { server: 0, .. }))
                .expect("a selectable row per server");
            picker.delegate.selected_index = server_row;
            picker.delegate.confirm(false, window, cx);
            assert!(picker.delegate.focused_server.is_some());
            picker.cancel(&menu::Cancel, window, cx);
        });
        cx.run_until_parked();
        picker.update(cx, |picker, _| {
            assert!(
                picker.delegate.focused_server.is_none(),
                "Cancel steps back to the server list"
            );
            assert!(
                picker
                    .delegate
                    .matches
                    .iter()
                    .any(|entry| matches!(entry, RemoteMatch::Server { server: 0, .. })),
                "the server list is shown again"
            );
        });
        assert_eq!(dismiss_count.load(Ordering::SeqCst), 0);

        // Cancel on the server list dismisses the modal.
        picker.update_in(cx, |picker, window, cx| {
            picker.cancel(&menu::Cancel, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(dismiss_count.load(Ordering::SeqCst), 1);
    }
}
