use gpui::{Anchor, Entity};
use ui::{
    ContextMenu, ContextMenuEntry, DocumentationSide, IconButton, IconName, IconPosition, IconSize,
    Label, PopoverMenu, Tooltip, prelude::*,
};

use crate::{
    TabularDataPreviewPane,
    settings::{FilterSortOrder, TabularDataPreviewSettings, VerticalAlignment},
};

///// Settings related /////

/// Adds a toggleable entry that applies `set` to the pane's settings when clicked. `description`,
/// when given, shows as a documentation aside explaining what the entry does.
fn toggle_entry(
    menu: ContextMenu,
    label: &'static str,
    description: Option<&'static str>,
    selected: bool,
    view_entity: &Entity<TabularDataPreviewPane>,
    set: impl Fn(&mut TabularDataPreviewSettings) + 'static,
) -> ContextMenu {
    let view_entity = view_entity.clone();
    let entry = ContextMenuEntry::new(label)
        .toggleable(IconPosition::Start, selected)
        .handler(move |_, cx| {
            view_entity.update(cx, |this, cx| {
                set(&mut this.settings);
                cx.notify();
            });
        });
    let entry = if let Some(description) = description {
        entry.documentation_aside(DocumentationSide::Right, move |_| {
            Label::new(description).into_any_element()
        })
    } else {
        entry
    };
    menu.item(entry)
}

pub(crate) fn settings_popover_menu(
    view_entity: Entity<TabularDataPreviewPane>,
) -> PopoverMenu<ContextMenu> {
    PopoverMenu::new("table-settings-menu")
        .trigger_with_tooltip(
            IconButton::new("table-settings-trigger", IconName::Filter)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Compact),
            Tooltip::text(i18n::t!("974ec1c56f232101")),
        )
        .anchor(Anchor::TopRight)
        .menu(move |window, cx| {
            let view_entity = view_entity.clone();
            Some(ContextMenu::build_persistent(
                window,
                cx,
                move |menu, _window, cx| {
                    let settings = view_entity.read(cx).settings.clone();

                    let menu = toggle_entry(
                        menu.header(i18n::t!("2527727872202c15")),
                        i18n::t!("6f2a4a02e60e067c"),
                        Some(i18n::t!("d3e29943e485320f")),
                        matches!(settings.vertical_alignment, VerticalAlignment::Top),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Top,
                    );
                    let menu = toggle_entry(
                        menu,
                        i18n::t!("4cc3885c182e419c"),
                        None,
                        matches!(settings.vertical_alignment, VerticalAlignment::Center),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Center,
                    );

                    let menu = menu.separator().header(i18n::t!("52f903208e38294c"));
                    let menu = toggle_entry(
                        menu,
                        i18n::t!("bd5e9509fe14020e"),
                        Some(i18n::t!("f61d4e621adf7e0a")),
                        settings.filter_sort_order == FilterSortOrder::AlphaThenCount,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::AlphaThenCount,
                    );
                    let menu = toggle_entry(
                        menu,
                        i18n::t!("0d53a24706341a87"),
                        None,
                        settings.filter_sort_order == FilterSortOrder::CountThenAlpha,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::CountThenAlpha,
                    );

                    let menu = toggle_entry(
                        menu.separator(),
                        i18n::t!("e4ca990d8bb868ff"),
                        Some(i18n::t!("da034868dba9940e")),
                        settings.multiline_cells_enabled,
                        &view_entity,
                        |settings| {
                            settings.multiline_cells_enabled = !settings.multiline_cells_enabled
                        },
                    );

                    #[cfg(feature = "dev-tools")]
                    let menu = append_dev_only_entries(menu, &view_entity, &settings);

                    menu
                },
            ))
        })
}

#[cfg(feature = "dev-tools")]
fn append_dev_only_entries(
    menu: ContextMenu,
    view_entity: &Entity<TabularDataPreviewPane>,
    settings: &TabularDataPreviewSettings,
) -> ContextMenu {
    use crate::settings::RowRenderMechanism;

    let menu = menu.separator().header(i18n::t!("0451d2aa393f2240"));
    let menu = toggle_entry(
        menu,
        i18n::t!("5c317977bd20201a"),
        Some(i18n::t!("1b40c8e317dca335")),
        settings.rendering_with == RowRenderMechanism::VariableList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::VariableList,
    );
    let menu = toggle_entry(
        menu,
        i18n::t!("baff0125a227a78d"),
        None,
        settings.rendering_with == RowRenderMechanism::UniformList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::UniformList,
    );

    let menu = toggle_entry(
        menu.separator(),
        i18n::t!("328ef20b05838596"),
        None,
        settings.show_perf_metrics_overlay,
        view_entity,
        |settings| settings.show_perf_metrics_overlay = !settings.show_perf_metrics_overlay,
    );
    toggle_entry(
        menu,
        i18n::t!("8c101ef157cdc3e3"),
        None,
        settings.show_debug_info,
        view_entity,
        |settings| settings.show_debug_info = !settings.show_debug_info,
    )
}
