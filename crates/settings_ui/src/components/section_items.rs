use std::ops::Range;

use gpui::{App, IntoElement, ParentElement, Role, Styled, div};
use ui::{Divider, DividerColor, HighlightedLabel, prelude::*};

/// 子页面通用的卡片容器，与主设置页小节卡片视觉一致。
pub(crate) fn settings_card(cx: &App) -> Div {
    div()
        .w_full()
        .bg(cx.theme().colors().surface_background)
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .rounded_md()
}

/// 卡片内的行：除首行外以上边框作为分隔线。
pub(crate) fn settings_card_row(first: bool, cx: &App) -> Div {
    div().w_full().px_4().py_2().when(!first, |this| {
        this.border_t_1()
            .border_color(cx.theme().colors().border_variant)
    })
}

#[derive(IntoElement)]
pub struct SettingsSectionHeader {
    icon: Option<IconName>,
    label: SharedString,
    highlight_ranges: Vec<Range<usize>>,
    no_padding: bool,
    carded: bool,
    card_bottom: bool,
}

impl SettingsSectionHeader {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            highlight_ranges: Vec::new(),
            icon: None,
            no_padding: false,
            carded: false,
            card_bottom: false,
        }
    }

    pub fn highlight_ranges(mut self, highlight_ranges: Vec<Range<usize>>) -> Self {
        self.highlight_ranges = highlight_ranges;
        self
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn no_padding(mut self, no_padding: bool) -> Self {
        self.no_padding = no_padding;
        self
    }

    /// 以卡片头部样式渲染：标题嵌入卡片顶边，`card_bottom` 表示该小节
    /// 没有可见条目，需要同时封闭卡片底边。
    pub fn carded(mut self, carded: bool, card_bottom: bool) -> Self {
        self.carded = carded;
        self.card_bottom = card_bottom;
        self
    }
}

impl RenderOnce for SettingsSectionHeader {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let label_text = self.label.clone();

        if self.carded {
            let label = HighlightedLabel::from_ranges(self.label, self.highlight_ranges)
                .size(LabelSize::Default)
                .color(Color::Default);

            return v_flex()
                .id(label_text.clone())
                .role(Role::Heading)
                .aria_level(2)
                .aria_label(label_text)
                .w_full()
                .bg(cx.theme().colors().surface_background)
                .border_l_1()
                .border_r_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .rounded_t_md()
                .when(self.card_bottom, |this| this.rounded_b_md().border_b_1())
                .px_4()
                .py_2p5()
                .map(|this| {
                    if let Some(icon) = self.icon {
                        this.child(
                            h_flex()
                                .gap_1p5()
                                .child(Icon::new(icon).color(Color::Muted))
                                .child(label),
                        )
                    } else {
                        this.child(label)
                    }
                });
        }

        let label = HighlightedLabel::from_ranges(self.label, self.highlight_ranges)
            .size(LabelSize::Small)
            .color(Color::Muted)
            .buffer_font(cx);

        v_flex()
            .id(label_text.clone())
            .role(Role::Heading)
            .aria_level(2)
            .aria_label(label_text)
            .w_full()
            .when(!self.no_padding, |this| this.px_8())
            .gap_1p5()
            .map(|this| {
                if let Some(icon) = self.icon {
                    this.child(
                        h_flex()
                            .gap_1p5()
                            .child(Icon::new(icon).color(Color::Muted))
                            .child(label),
                    )
                } else {
                    this.child(label)
                }
            })
            .child(Divider::horizontal().color(DividerColor::BorderFaded))
    }
}
