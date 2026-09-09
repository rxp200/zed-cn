use std::ops::Range;

use gpui::{IntoElement, ParentElement, Role, Styled};
use ui::{Divider, DividerColor, HighlightedLabel, prelude::*};

#[derive(IntoElement)]
pub struct SettingsSectionHeader {
    icon: Option<IconName>,
    label: SharedString,
    highlight_ranges: Vec<Range<usize>>,
    no_padding: bool,
}

impl SettingsSectionHeader {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            highlight_ranges: Vec::new(),
            icon: None,
            no_padding: false,
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
}

impl RenderOnce for SettingsSectionHeader {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let label_text = self.label.clone();
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
