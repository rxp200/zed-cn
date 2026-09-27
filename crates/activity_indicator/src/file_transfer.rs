use gpui::{
    Anchor, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
};
use project::file_transfer::{FileTransfers, Transfer, TransferDirection};
use std::time::Duration;
use ui::{PopoverMenu, Tooltip, prelude::*};
use workspace::{StatusItemView, Workspace, item::ItemHandle};

pub struct FileTransferIndicator {
    transfers: Entity<FileTransfers>,
    _subscription: Subscription,
    _hide_task: Option<Task<()>>,
}
impl FileTransferIndicator {
    pub fn new(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        let transfers = project::file_transfer::store(workspace.project(), cx);
        cx.new(|cx| {
            let subscription = cx.observe(&transfers, |this: &mut Self, transfers, cx| {
                cx.notify();
                if let Some(delay) = transfers
                    .read(cx)
                    .entries()
                    .iter()
                    .filter(|entry| !entry.error)
                    .filter_map(|entry| entry.finished)
                    .filter_map(|finished| Duration::from_secs(5).checked_sub(finished.elapsed()))
                    .max()
                {
                    this._hide_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(delay).await;
                        this.update(cx, |_, cx| cx.notify()).ok();
                    }));
                }
            });
            Self {
                transfers,
                _subscription: subscription,
                _hide_task: None,
            }
        })
    }
}
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024. * 1024. * 1024.))
    } else if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024. * 1024.))
    }
}

fn summary(entry: &Transfer) -> String {
    let name = entry.path.rsplit(['/', '\\']).next().unwrap_or(&entry.path);
    let percentage = entry
        .percentage()
        .map(|value| format!(" {value}%"))
        .unwrap_or_default();
    format!(
        "{} {}{} · {}",
        entry.direction.label(),
        name,
        percentage,
        entry.status
    )
}
impl Render for FileTransferIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.transfers.read(cx).entries();
        let visible: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.error
                    || entry
                        .finished
                        .is_none_or(|finished| finished.elapsed() < Duration::from_secs(5))
            })
            .collect();
        let transfers = self.transfers.clone();
        h_flex().when(!visible.is_empty(), |element| {
            let active = visible
                .iter()
                .filter(|entry| entry.finished.is_none())
                .count();
            let label = if visible.len() == 1 {
                visible
                    .first()
                    .map(|entry| summary(entry))
                    .unwrap_or_default()
            } else {
                let uploads = visible
                    .iter()
                    .filter(|entry| {
                        entry.finished.is_none() && entry.direction == TransferDirection::Upload
                    })
                    .count();
                let downloads = visible
                    .iter()
                    .filter(|entry| {
                        entry.finished.is_none() && entry.direction == TransferDirection::Download
                    })
                    .count();
                let copies = active - uploads - downloads;
                let failures = visible.iter().filter(|entry| entry.error).count();
                if active == 0 && failures == 0 {
                    "文件传输已完成".into()
                } else {
                    format!(
                        "↑ 上传 {uploads} · ↓ 下载 {downloads} · 复制 {copies} · 失败 {failures}"
                    )
                }
            };
            element.child(
                PopoverMenu::new("file-transfers")
                    .anchor(Anchor::BottomLeft)
                    .trigger(
                        Button::new(
                            "file-transfers-trigger",
                            util::truncate_and_trailoff(&label, 55),
                        )
                        .label_size(LabelSize::Small)
                        .loading(active > 0)
                        .tooltip(Tooltip::text(label)),
                    )
                    .menu(move |_, cx| {
                        Some(cx.new(|cx| TransferDetails::new(transfers.clone(), cx)))
                    }),
            )
        })
    }
}
impl StatusItemView for FileTransferIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}
struct TransferDetails {
    transfers: Entity<FileTransfers>,
    focus: FocusHandle,
    _subscription: Subscription,
}
impl TransferDetails {
    fn new(transfers: Entity<FileTransfers>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&transfers, |_, _, cx| cx.notify());
        Self {
            transfers,
            focus: cx.focus_handle(),
            _subscription: subscription,
        }
    }
}
impl EventEmitter<DismissEvent> for TransferDetails {}
impl Focusable for TransferDetails {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for TransferDetails {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.transfers.read(cx).entries();
        v_flex()
            .id("transfer-details")
            .track_focus(&self.focus)
            .w(px(400.))
            .max_h(px(420.))
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("文件传输"))
                    .child(
                        Button::new("clear-finished-transfers", "清除已结束").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.transfers
                                    .update(cx, |transfers, cx| transfers.clear_finished(cx))
                            }),
                        ),
                    ),
            )
            .child(
                Label::new("上传统计本机已发送字节，含消息开销；远端确认后才完成")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(entries.iter().map(|entry| {
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .child(Label::new(summary(entry)).color(if entry.error {
                        Color::Error
                    } else {
                        Color::Default
                    }))
                    .child(
                        Label::new(format!("{} → {}", entry.path, entry.destination))
                            .size(LabelSize::Small)
                            .truncate()
                            .color(Color::Muted),
                    )
                    .when_some(
                        entry.total_entries.filter(|total| *total > 1),
                        |element, total| {
                            let percentage = entry.completed_files as f32 / total as f32 * 100.;
                            element
                                .child(
                                    Label::new(format!("整体进度（按项目数）：{percentage:.0}%"))
                                        .size(LabelSize::Small),
                                )
                                .child(ui::ProgressBar::new(
                                    ("transfer-batch-progress", entry.id),
                                    percentage,
                                    100.,
                                    cx,
                                ))
                        },
                    )
                    .when_some(entry.percentage(), |element, percentage| {
                        element
                            .child(
                                Label::new(format!("当前文件：{percentage}%"))
                                    .size(LabelSize::Small),
                            )
                            .child(ui::ProgressBar::new(
                                ("transfer-progress", entry.id),
                                percentage as f32,
                                100.,
                                cx,
                            ))
                    })
                    .child(
                        Label::new(format!(
                            "当前文件 {} / {} · 已完成 {} / {} 项",
                            format_bytes(entry.bytes),
                            entry
                                .total
                                .map(format_bytes)
                                .unwrap_or_else(|| "未知".into()),
                            entry.completed_files,
                            entry
                                .total_entries
                                .map(|total| total.to_string())
                                .unwrap_or_else(|| "1".into())
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
            }))
    }
}
