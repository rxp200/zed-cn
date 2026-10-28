use crate::component_prelude::*;
use crate::prelude::*;
use crate::{Checkbox, ListBulletItem, ToggleState};
use gpui::Action;
use gpui::FocusHandle;
use gpui::IntoElement;
use gpui::Stateful;
use smallvec::{SmallVec, smallvec};
use theme::ActiveTheme;

type ActionHandler = Box<dyn FnOnce(Stateful<Div>) -> Stateful<Div>>;

#[derive(IntoElement, RegisterComponent)]
pub struct AlertModal {
    id: ElementId,
    header: Option<AnyElement>,
    children: SmallVec<[AnyElement; 2]>,
    footer: Option<AnyElement>,
    title: Option<SharedString>,
    primary_action: Option<SharedString>,
    dismiss_label: Option<SharedString>,
    width: Option<DefiniteLength>,
    key_context: Option<String>,
    action_handlers: Vec<ActionHandler>,
    focus_handle: Option<FocusHandle>,
}

impl AlertModal {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            header: None,
            children: smallvec![],
            footer: None,
            title: None,
            primary_action: None,
            dismiss_label: None,
            width: None,
            key_context: None,
            action_handlers: Vec::new(),
            focus_handle: None,
        }
    }

    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn header(mut self, header: impl IntoElement) -> Self {
        self.header = Some(header.into_any_element());
        self
    }

    pub fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }

    pub fn primary_action(mut self, primary_action: impl Into<SharedString>) -> Self {
        self.primary_action = Some(primary_action.into());
        self
    }

    pub fn dismiss_label(mut self, dismiss_label: impl Into<SharedString>) -> Self {
        self.dismiss_label = Some(dismiss_label.into());
        self
    }

    pub fn width(mut self, width: impl Into<DefiniteLength>) -> Self {
        self.width = Some(width.into());
        self
    }

    pub fn key_context(mut self, key_context: impl Into<String>) -> Self {
        self.key_context = Some(key_context.into());
        self
    }

    pub fn on_action<A: Action>(
        mut self,
        listener: impl Fn(&A, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.action_handlers
            .push(Box::new(move |div| div.on_action(listener)));
        self
    }

    pub fn track_focus(mut self, focus_handle: &gpui::FocusHandle) -> Self {
        self.focus_handle = Some(focus_handle.clone());
        self
    }
}

impl RenderOnce for AlertModal {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let width = self.width.unwrap_or_else(|| px(440.).into());
        let has_default_footer = self.primary_action.is_some() || self.dismiss_label.is_some();

        let mut modal = v_flex()
            .when_some(self.key_context, |this, key_context| {
                this.key_context(key_context.as_str())
            })
            .when_some(self.focus_handle, |this, focus_handle| {
                this.track_focus(&focus_handle)
            })
            .id(self.id)
            .elevation_3(cx)
            .w(width)
            .bg(cx.theme().colors().elevated_surface_background)
            .overflow_hidden();

        for handler in self.action_handlers {
            modal = handler(modal);
        }

        if let Some(header) = self.header {
            modal = modal.child(header);
        } else if let Some(title) = self.title {
            modal = modal.child(
                v_flex()
                    .pt_3()
                    .pr_3()
                    .pl_3()
                    .pb_1()
                    .child(Headline::new(title).size(HeadlineSize::Small)),
            );
        }

        if !self.children.is_empty() {
            modal = modal.child(
                v_flex()
                    .p_3()
                    .text_ui(cx)
                    .text_color(Color::Muted.color(cx))
                    .gap_1()
                    .children(self.children),
            );
        }

        if let Some(footer) = self.footer {
            modal = modal.child(footer);
        } else if has_default_footer {
            let primary_action = self.primary_action.unwrap_or_else(|| "OK".into());
            let dismiss_label = self
                .dismiss_label
                .unwrap_or_else(|| i18n::t!("2cd0f3be8738a86c").into());

            modal = modal.child(
                h_flex()
                    .p_3()
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .child(Button::new(dismiss_label.clone(), dismiss_label).color(Color::Muted))
                    .child(Button::new(primary_action.clone(), primary_action)),
            );
        }

        modal
    }
}

impl ParentElement for AlertModal {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl Component for AlertModal {
    fn scope() -> ComponentScope {
        ComponentScope::Notification
    }

    fn status() -> ComponentStatus {
        ComponentStatus::WorkInProgress
    }

    fn description() -> &'static str {
        i18n::t!("581b9f2b19decf6f")
    }

    fn preview(_window: &mut Window, cx: &mut App) -> AnyElement {
        v_flex()
                .gap_6()
                .p_4()
                .children(vec![
                    example_group(vec![single_example(
                        i18n::t!("ec51167263cf2f72"),
                        AlertModal::new("simple-modal")
                            .title(i18n::t!("bb74a4300d908eb9"))
                            .child(
                                i18n::t!("82b45da3777bb82f")
                            )
                            .primary_action(i18n::t!("3446876b684782f8"))
                            .dismiss_label(i18n::t!("2cd0f3be8738a86c"))
                            .into_any_element(),
                    )]),
                    example_group(vec![single_example(
                        i18n::t!("99ea19febe21c40c"),
                        AlertModal::new("custom-header-modal")
                            .header(
                                v_flex()
                                    .p_3()
                                    .bg(cx.theme().colors().background)
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(Icon::new(IconName::Warning).color(Color::Warning))
                                            .child(Headline::new(i18n::t!("c38886c07f73630d")).size(HeadlineSize::Small))
                                    )
                                    .child(
                                        h_flex()
                                            .pl(IconSize::default().rems() + rems(0.5))
                                            .child(Label::new("~/projects/my-project").color(Color::Muted))
                                    )
                            )
                            .child(
                                "Untrusted workspaces are opened in Restricted Mode to protect your system.
Review .zed/settings.json for any extensions or commands configured by this project.",
                            )
                            .child(
                                v_flex()
                                    .mt_1()
                                    .child(Label::new(i18n::t!("8a9bf1e30f256317")).color(Color::Muted))
                                    .child(ListBulletItem::new(i18n::t!("1026d9417051b164")))
                                    .child(ListBulletItem::new(i18n::t!("f8baa675efb1604a")))
                                    .child(ListBulletItem::new(i18n::t!("99a8ea274b0e5c8a")))
                            )
                            .footer(
                                h_flex()
                                    .p_3()
                                    .justify_between()
                                    .child(
                                        Checkbox::new("trust-parent", ToggleState::Unselected)
                                            .label(i18n::t!("f03cae31f21311fc"))
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(Button::new("restricted", i18n::t!("26cdeb4b4b278314")).color(Color::Muted))
                                            .child(Button::new("trust", i18n::t!("1ac3a881cb626809")).style(ButtonStyle::Filled))
                                    )
                            )
                            .width(rems(40.))
                            .into_any_element(),
                    )]),
                ])
                .into_any_element()
    }
}
