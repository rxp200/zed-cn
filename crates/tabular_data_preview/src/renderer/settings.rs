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
            Tooltip::text("表格设置"),
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
                        menu.header("文本对齐"),
                        "顶部",
                        Some("选择单元格内的垂直文本对齐方式"),
                        matches!(settings.vertical_alignment, VerticalAlignment::Top),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Top,
                    );
                    let menu = toggle_entry(
                        menu,
                        "居中",
                        None,
                        matches!(settings.vertical_alignment, VerticalAlignment::Center),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Center,
                    );

                    let menu = menu.separator().header("筛选排序");
                    let menu = toggle_entry(
                        menu,
                        "A-Z，再按数量",
                        Some("选择筛选菜单中筛选值的排序方式"),
                        settings.filter_sort_order == FilterSortOrder::AlphaThenCount,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::AlphaThenCount,
                    );
                    let menu = toggle_entry(
                        menu,
                        "按数量，再按 A-Z",
                        None,
                        settings.filter_sort_order == FilterSortOrder::CountThenAlpha,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::CountThenAlpha,
                    );

                    let menu = toggle_entry(
                        menu.separator(),
                        "显示多行单元格",
                        Some(
                            "启用时，行高将增长以显示所有内容。\
                             禁用时，仅显示第一行——悬停在单元格上可查看其余内容。",
                        ),
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

    let menu = menu.separator().header("仅开发用：渲染模式");
    let menu = toggle_entry(
        menu,
        "可变高度",
        Some(
            "仅开发用的调试部分。\n\
             表格数据预览功能公开发布后将移除。",
        ),
        settings.rendering_with == RowRenderMechanism::VariableList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::VariableList,
    );
    let menu = toggle_entry(
        menu,
        "统一高度",
        None,
        settings.rendering_with == RowRenderMechanism::UniformList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::UniformList,
    );

    let menu = toggle_entry(
        menu.separator(),
        "显示性能指标",
        None,
        settings.show_perf_metrics_overlay,
        view_entity,
        |settings| settings.show_perf_metrics_overlay = !settings.show_perf_metrics_overlay,
    );
    toggle_entry(
        menu,
        "显示单元格位置",
        None,
        settings.show_debug_info,
        view_entity,
        |settings| settings.show_debug_info = !settings.show_debug_info,
    )
}
