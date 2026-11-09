use gpui::{
    App, Context, EventEmitter, IntoElement, PlatformDisplay, Size, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowKind, WindowOptions,
    linear_color_stop, linear_gradient, point,
};
use release_channel::ReleaseChannel;
use std::rc::Rc;
use ui::{Render, prelude::*};

/// Height of a single popup, used both for its window and for stacking slots.
pub const POPUP_NOTIFICATION_HEIGHT: Pixels = px(72.);

/// Vertical distance between two stacked popups.
pub const POPUP_NOTIFICATION_GAP: Pixels = px(8.);

/// A small always-on-top window used to alert the user about work that
/// finished while they were looking elsewhere.
pub struct PopupNotification {
    id: SharedString,
    title: SharedString,
    caption: Option<SharedString>,
    icon: IconName,
    icon_color: Color,
    project_name: Option<SharedString>,
    accept_label: SharedString,
}

impl PopupNotification {
    pub fn new(
        title: impl Into<SharedString>,
        caption: Option<SharedString>,
        icon: IconName,
        project_name: Option<impl Into<SharedString>>,
    ) -> Self {
        Self {
            id: "popup-notification".into(),
            title: title.into(),
            caption,
            icon,
            icon_color: Color::Muted,
            project_name: project_name.map(|name| name.into()),
            accept_label: i18n::t!("db8db0530432bd15").into(),
        }
    }

    /// Overrides the id of the root element of this popup.
    pub fn id(mut self, id: impl Into<SharedString>) -> Self {
        self.id = id.into();
        self
    }

    /// Overrides the color used to paint the leading icon.
    pub fn icon_color(mut self, icon_color: Color) -> Self {
        self.icon_color = icon_color;
        self
    }

    /// Overrides the label of the primary button that accepts the notification.
    pub fn accept_label(mut self, accept_label: impl Into<SharedString>) -> Self {
        self.accept_label = accept_label.into();
        self
    }

    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// Replaces the content an already-open popup shows.
    ///
    /// A stack that keeps its windows pinned to fixed screen slots reorders
    /// itself by swapping what each window renders, because GPUI cannot
    /// reposition an existing window.
    pub fn set_content(
        &mut self,
        title: SharedString,
        caption: Option<SharedString>,
        icon: IconName,
        icon_color: Color,
        project_name: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.title = title;
        self.caption = caption;
        self.icon = icon;
        self.icon_color = icon_color;
        self.project_name = project_name;
        cx.notify();
    }

    pub fn window_options(screen: Rc<dyn PlatformDisplay>, cx: &App) -> WindowOptions {
        Self::window_options_at_slot(screen, 0, cx)
    }

    /// The `slot`-th notification is placed below the preceding ones, so that
    /// several simultaneous notifications do not overlap.
    pub fn window_options_at_slot(
        screen: Rc<dyn PlatformDisplay>,
        slot: usize,
        cx: &App,
    ) -> WindowOptions {
        let size = Size {
            width: px(450.),
            height: POPUP_NOTIFICATION_HEIGHT,
        };

        let notification_margin_width = px(16.);
        let notification_margin_height = px(48.);
        let slot_offset = (POPUP_NOTIFICATION_HEIGHT + POPUP_NOTIFICATION_GAP) * slot as f32;

        let bounds = gpui::Bounds::<Pixels> {
            origin: screen.bounds().top_right()
                - point(
                    size.width + notification_margin_width,
                    -(notification_margin_height + slot_offset),
                ),
            size,
        };

        let app_id = ReleaseChannel::global(cx).app_id();

        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            display_id: Some(screen.id()),
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some(app_id.to_owned()),
            window_min_size: None,
            window_decorations: Some(WindowDecorations::Client),
            tabbing_identifier: None,
            ..Default::default()
        }
    }
}

pub enum PopupNotificationEvent {
    Accepted,
    Dismissed,
}

impl EventEmitter<PopupNotificationEvent> for PopupNotification {}

impl PopupNotification {
    pub fn accept(&mut self, cx: &mut Context<Self>) {
        cx.emit(PopupNotificationEvent::Accepted);
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(PopupNotificationEvent::Dismissed);
    }
}

impl Render for PopupNotification {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let line_height = window.line_height();

        let bg = cx.theme().colors().elevated_surface_background;
        let gradient_overflow = || {
            div()
                .h_full()
                .absolute()
                .w_8()
                .bottom_0()
                .right_0()
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(bg, 1.),
                    linear_color_stop(bg.opacity(0.2), 0.),
                ))
        };

        h_flex()
            .id(self.id.clone())
            .size_full()
            .p_3()
            .gap_4()
            .justify_between()
            .elevation_3(cx)
            .text_ui(cx)
            .font(ui_font)
            .border_color(cx.theme().colors().border)
            .rounded_xl()
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .flex_1()
                    .child(
                        h_flex().h(line_height).justify_center().child(
                            Icon::new(self.icon)
                                .color(self.icon_color)
                                .size(IconSize::Small),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .max_w(px(300.))
                            .child(
                                div()
                                    .relative()
                                    .text_size(px(14.))
                                    .text_color(cx.theme().colors().text)
                                    .truncate()
                                    .child(self.title.clone())
                                    .child(gradient_overflow()),
                            )
                            .child(
                                h_flex()
                                    .relative()
                                    .gap_1p5()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().colors().text_muted)
                                    .truncate()
                                    .when_some(
                                        self.project_name.clone(),
                                        |description, project_name| {
                                            let has_caption = self.caption.is_some();
                                            let project = div()
                                                .truncate()
                                                .when(has_caption, |this| this.max_w_16())
                                                .child(project_name);
                                            let mut row = h_flex().gap_1p5().child(project);
                                            if has_caption {
                                                row = row.child(
                                                    div().size(px(3.)).rounded_full().bg(cx
                                                        .theme()
                                                        .colors()
                                                        .text
                                                        .opacity(0.5)),
                                                );
                                            }
                                            description.child(row)
                                        },
                                    )
                                    .when_some(self.caption.clone(), |description, caption| {
                                        description.child(caption)
                                    })
                                    .child(gradient_overflow()),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Button::new("open", self.accept_label.clone())
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .full_width()
                            .on_click({
                                cx.listener(move |this, _event, _, cx| {
                                    this.accept(cx);
                                })
                            }),
                    )
                    .child(
                        Button::new("dismiss", i18n::t!("3fd47edce45b3603"))
                            .full_width()
                            .on_click({
                                cx.listener(move |this, _event, _, cx| {
                                    this.dismiss(cx);
                                })
                            }),
                    ),
            )
    }
}
