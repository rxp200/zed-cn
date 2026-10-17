//! Table Cell Rendering

use gpui::{AnyElement, ClickEvent, ClipboardItem, ElementId, MouseButton, StatefulInteractiveElement};
use ui::{Color, Divider, Label, LabelSize, SharedString, Tooltip, div, prelude::*};

use crate::{TableView, settings::VerticalAlignment, types::{DataCellId, DisplayCellId}};

/// Adds a right-click-to-copy handler and a tooltip showing `text` plus `hints`
/// (e.g. "Right click to copy content") to a `Stateful` element.
pub(crate) fn with_copy_on_right_click<E: StatefulInteractiveElement>(
    element: E,
    text: SharedString,
    hints: Vec<SharedString>,
) -> E {
    element
        .on_mouse_down(MouseButton::Right, {
            let text_to_copy = text.clone();
            move |_event, _window, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(ClipboardItem::new_string(text_to_copy.to_string()));
            }
        })
        .tooltip(Tooltip::element(move |_window, cx| {
            let mut tooltip = v_flex()
                .gap_1()
                .child(div().font_buffer(cx).child(text.clone()))
                .child(Divider::horizontal());
            for hint in &hints {
                tooltip = tooltip.child(
                    Label::new(hint.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            tooltip.into_any_element()
        }))
}

impl TableView {
    /// Create selectable table cell with mouse event handlers.
    ///
    /// Double-clicking an editable cell opens the inline editor over it.
    pub fn create_selectable_cell(
        display_cell_id: DisplayCellId,
        data_cell_id: DataCellId,
        cell_content: SharedString,
        vertical_alignment: VerticalAlignment,
        editable: bool,
        cx: &Context<TableView>,
    ) -> AnyElement {
        let mut hints = vec![i18n::t!("0129ff93ed2d15ef").into()];
        if editable {
            hints.push(i18n::t!("306767b45381e7e4").into());
        }

        create_table_cell(
            display_cell_id,
            cell_content,
            vertical_alignment,
            hints,
            cx,
        )
        .when(editable, |cell| {
            cell.on_click(cx.listener(
                move |this, event: &ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        cx.stop_propagation();
                        this.begin_cell_edit(data_cell_id, window, cx);
                    }
                },
            ))
        })
        .into_any_element()
    }
}

/// Create styled table cell div element.
fn create_table_cell(
    display_cell_id: DisplayCellId,
    cell_content: SharedString,
    vertical_alignment: VerticalAlignment,
    hints: Vec<SharedString>,
    cx: &Context<'_, TableView>,
) -> gpui::Stateful<Div> {
    let cell = div()
        .id(ElementId::NamedInteger(
            format!("table-display-cell-{}", *display_cell_id.row).into(),
            *display_cell_id.col as u64,
        ))
        .flex()
        .h_full()
        .px_1()
        .border_color(cx.theme().colors().border_variant)
        .map(|div| match vertical_alignment {
            VerticalAlignment::Top => div.items_start(),
            VerticalAlignment::Center => div.items_center(),
        })
        .font_buffer(cx);
    with_copy_on_right_click(cell, cell_content.clone(), hints).child(div().child(cell_content))
}
