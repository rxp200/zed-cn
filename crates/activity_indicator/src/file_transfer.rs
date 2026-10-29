use auto_update::remote_server_downloads::{RemoteServerDownload, RemoteServerDownloads};
use gpui::{
    Anchor, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
};
use project::file_transfer::{FileTransfers, Transfer, TransferDirection};
use std::{path::Path, time::Duration};
use ui::{PopoverMenu, Tooltip, prelude::*};
use workspace::{StatusItemView, Workspace, item::ItemHandle};

const FINISHED_VISIBLE_DURATION: Duration = Duration::from_secs(5);

pub struct FileTransferIndicator {
    transfers: Entity<FileTransfers>,
    server_downloads: Entity<RemoteServerDownloads>,
    _subscription: Subscription,
    _server_subscription: Subscription,
    _hide_task: Option<Task<()>>,
}
impl FileTransferIndicator {
    pub fn new(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        RemoteServerDownloads::ensure_global(cx);
        let transfers = project::file_transfer::store(workspace.project(), cx);
        let server_downloads = RemoteServerDownloads::global(cx);
        cx.new(|cx| {
            let subscription = cx.observe(&transfers, |this: &mut Self, _, cx| {
                this.schedule_hide(cx);
                cx.notify();
            });
            let server_subscription = cx.observe(&server_downloads, |this: &mut Self, _, cx| {
                this.schedule_hide(cx);
                cx.notify();
            });
            Self {
                transfers,
                server_downloads,
                _subscription: subscription,
                _server_subscription: server_subscription,
                _hide_task: None,
            }
        })
    }

    /// Keeps the bar mounted long enough to re-render once a finished transfer
    /// or download falls out of its retention window.
    fn schedule_hide(&mut self, cx: &mut Context<Self>) {
        let file_delay = self
            .transfers
            .read(cx)
            .entries()
            .iter()
            .filter(|entry| !entry.error)
            .filter_map(|entry| entry.finished)
            .filter_map(|finished| FINISHED_VISIBLE_DURATION.checked_sub(finished.elapsed()))
            .max();
        let server_delay = self
            .server_downloads
            .read(cx)
            .entries()
            .iter()
            .filter(|entry| !entry.error)
            .filter_map(|entry| entry.finished)
            .filter_map(|finished| FINISHED_VISIBLE_DURATION.checked_sub(finished.elapsed()))
            .max();
        let delay = match (file_delay, server_delay) {
            (Some(file), Some(server)) => Some(file.max(server)),
            (file, server) => file.or(server),
        };
        if let Some(delay) = delay {
            self._hide_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                this.update(cx, |_, cx| cx.notify()).ok();
            }));
        }
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

fn server_summary(entry: &RemoteServerDownload) -> String {
    format!(
        "{} {} · {}",
        i18n::t!("68d0a2e10bc6df49"),
        entry.key.platform.label,
        entry.status
    )
}

fn is_file_transfer_visible(entry: &Transfer) -> bool {
    entry.error
        || entry.cancelled
        || entry
            .finished
            .is_none_or(|finished| finished.elapsed() < FINISHED_VISIBLE_DURATION)
}

/// Only an unfinished transfer can be stopped.
fn can_cancel(entry: &Transfer) -> bool {
    entry.finished.is_none()
}

/// The destination of a finished download exists on this machine, so it can be
/// revealed in the local file manager. Uploads write to the remote host and
/// copies stay inside the project, so neither gets the reveal button.
fn can_reveal_in_file_manager(entry: &Transfer) -> bool {
    entry.finished.is_some()
        && !entry.error
        && !entry.cancelled
        && entry.direction == TransferDirection::Download
        && !entry.destination.is_empty()
}
impl Render for FileTransferIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible_files: Vec<Transfer> = self
            .transfers
            .read(cx)
            .entries()
            .into_iter()
            .filter(is_file_transfer_visible)
            .collect();
        let visible_servers: Vec<RemoteServerDownload> = self
            .server_downloads
            .read(cx)
            .entries()
            .into_iter()
            .filter(RemoteServerDownload::is_visible)
            .collect();
        let transfers = self.transfers.clone();
        let server_downloads = self.server_downloads.clone();
        h_flex().when(
            !visible_files.is_empty() || !visible_servers.is_empty(),
            |element| {
                let active = visible_files
                    .iter()
                    .filter(|entry| entry.finished.is_none())
                    .count()
                    + visible_servers
                        .iter()
                        .filter(|entry| entry.is_active())
                        .count();
                let label = if visible_files.len() + visible_servers.len() == 1 {
                    if let Some(entry) = visible_files.first() {
                        summary(entry)
                    } else {
                        visible_servers
                            .first()
                            .map(server_summary)
                            .unwrap_or_default()
                    }
                } else {
                    let uploads = visible_files
                        .iter()
                        .filter(|entry| {
                            entry.finished.is_none() && entry.direction == TransferDirection::Upload
                        })
                        .count();
                    let downloads = visible_files
                        .iter()
                        .filter(|entry| {
                            entry.finished.is_none()
                                && entry.direction == TransferDirection::Download
                        })
                        .count()
                        + visible_servers
                            .iter()
                            .filter(|entry| entry.is_active())
                            .count();
                    let copies = active - uploads - downloads;
                    let failures = visible_files.iter().filter(|entry| entry.error).count()
                        + visible_servers.iter().filter(|entry| entry.error).count();
                    if active == 0 && failures == 0 {
                        i18n::t!("8a5e582c9cb452e8").into()
                    } else {
                        i18n::t!(
                            "69c9f552db697af6",
                            uploads = uploads,
                            downloads = downloads,
                            copies = copies,
                            failures = failures
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
                            Some(cx.new(|cx| {
                                TransferDetails::new(
                                    transfers.clone(),
                                    server_downloads.clone(),
                                    cx,
                                )
                            }))
                        }),
                )
            },
        )
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
    server_downloads: Entity<RemoteServerDownloads>,
    focus: FocusHandle,
    _subscription: Subscription,
    _server_subscription: Subscription,
}
impl TransferDetails {
    fn new(
        transfers: Entity<FileTransfers>,
        server_downloads: Entity<RemoteServerDownloads>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&transfers, |_, _, cx| cx.notify());
        let server_subscription = cx.observe(&server_downloads, |_, _, cx| cx.notify());
        Self {
            transfers,
            server_downloads,
            focus: cx.focus_handle(),
            _subscription: subscription,
            _server_subscription: server_subscription,
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
        let server_entries: Vec<RemoteServerDownload> = self
            .server_downloads
            .read(cx)
            .entries()
            .into_iter()
            .filter(RemoteServerDownload::is_visible)
            .collect();
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
                    .child(Label::new(i18n::t!("f3c32e652f6592d1")))
                    .child(
                        Button::new("clear-finished-transfers", i18n::t!("6b14dd8a6a1c12ce"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.transfers
                                    .update(cx, |transfers, cx| transfers.clear_finished(cx));
                                this.server_downloads
                                    .update(cx, |store, cx| store.clear_finished(cx));
                            })),
                    ),
            )
            .child(
                Label::new(i18n::t!("bf738c7556fc5ba0"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(entries.iter().map(|entry| {
                let id = entry.id;
                let destination = entry.destination.clone();
                let show_cancel = can_cancel(entry);
                let show_reveal = can_reveal_in_file_manager(entry);
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(
                                Label::new(summary(entry)).truncate().color(if entry.error {
                                    Color::Error
                                } else if entry.cancelled {
                                    Color::Muted
                                } else {
                                    Color::Default
                                }),
                            ))
                            .when(show_cancel, |element| {
                                element.child(
                                    Button::new(
                                        ("cancel-transfer", id),
                                        i18n::t!("8637ac9558ff1093"),
                                    )
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.transfers.update(cx, |transfers, cx| {
                                                transfers.cancel(id, cx)
                                            });
                                        },
                                    )),
                                )
                            })
                            .when(show_reveal, |element| {
                                let destination = Path::new(&destination).to_path_buf();
                                element.child(
                                    IconButton::new(("reveal-transfer", id), IconName::FolderOpen)
                                        .icon_size(IconSize::Small)
                                        .aria_label(ui::utils::reveal_in_file_manager_label(false))
                                        .tooltip(Tooltip::text(
                                            ui::utils::reveal_in_file_manager_label(false),
                                        ))
                                        .on_click(move |_, _, cx| {
                                            cx.reveal_path(&destination);
                                        }),
                                )
                            }),
                    )
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
                                    Label::new(i18n::t!(
                                        "16fb23c8514e94a3",
                                        percentage = percentage
                                    ))
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
                                Label::new(i18n::t!("c4bc5abcc47e115e", percentage = percentage))
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
                        Label::new(i18n::t_args!(
                            "5acabdee0bde1674",
                            format_bytes(entry.bytes),
                            entry
                                .total
                                .map(format_bytes)
                                .unwrap_or_else(|| i18n::t!("4d8c1c5b42830791").into()),
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
            .when(!server_entries.is_empty(), |element| {
                element.child(
                    Label::new(i18n::t!("0fd69732f0855eac"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(server_entries.into_iter().map(|entry| {
                let id = entry.id;
                let active = entry.is_active();
                v_flex()
                    .min_w_0()
                    .gap_1()
                    .child(Label::new(server_summary(&entry)).color(if entry.error {
                        Color::Error
                    } else {
                        Color::Default
                    }))
                    .when_some(entry.percentage(), |element, percentage| {
                        element.child(ui::ProgressBar::new(
                            ("server-download-progress", id),
                            percentage * 100.,
                            100.,
                            cx,
                        ))
                    })
                    .when(active, |element| {
                        element.child(
                            Button::new(
                                ("cancel-server-download", id),
                                i18n::t!("c0c05b34c0b1ed98"),
                            )
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.server_downloads
                                        .update(cx, |store, cx| store.cancel(id, cx));
                                },
                            )),
                        )
                    })
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn transfer(direction: TransferDirection) -> Transfer {
        Transfer {
            id: 1,
            direction,
            path: "dist/file.bin".into(),
            destination: "/tmp/file.bin".into(),
            bytes: 1024,
            total: Some(4096),
            completed_files: 0,
            total_entries: Some(1),
            status: i18n::t!("e0f3db545a0fe3d5").into(),
            finished: None,
            error: false,
            cancelled: false,
        }
    }

    #[test]
    fn only_unfinished_transfers_can_be_cancelled() {
        let mut entry = transfer(TransferDirection::Download);
        assert!(can_cancel(&entry));

        entry.finished = Some(Instant::now());
        assert!(!can_cancel(&entry));
    }

    #[test]
    fn finished_download_offers_reveal_in_file_manager() {
        let mut entry = transfer(TransferDirection::Download);
        assert!(
            !can_reveal_in_file_manager(&entry),
            "an in-flight download has no completed file to reveal"
        );

        entry.finished = Some(Instant::now());
        assert!(can_reveal_in_file_manager(&entry));
    }

    #[test]
    fn finished_upload_or_copy_has_no_reveal_button() {
        for direction in [TransferDirection::Upload, TransferDirection::Copy] {
            let mut entry = transfer(direction);
            entry.finished = Some(Instant::now());
            assert!(
                !can_reveal_in_file_manager(&entry),
                "uploads write to the remote host and copies stay inside the project"
            );
        }
    }

    #[test]
    fn failed_cancelled_or_destinationless_download_has_no_reveal_button() {
        let mut failed = transfer(TransferDirection::Download);
        failed.finished = Some(Instant::now());
        failed.error = true;
        assert!(!can_reveal_in_file_manager(&failed));

        let mut cancelled = transfer(TransferDirection::Download);
        cancelled.finished = Some(Instant::now());
        cancelled.cancelled = true;
        assert!(!can_reveal_in_file_manager(&cancelled));

        let mut without_destination = transfer(TransferDirection::Download);
        without_destination.finished = Some(Instant::now());
        without_destination.destination.clear();
        assert!(!can_reveal_in_file_manager(&without_destination));
    }

    #[test]
    fn cancelled_transfer_stays_visible_until_cleared() {
        let mut entry = transfer(TransferDirection::Download);
        assert!(is_file_transfer_visible(&entry));

        entry.finished = Some(Instant::now());
        entry.cancelled = true;
        assert!(is_file_transfer_visible(&entry));
        assert_eq!(entry.percentage(), None);
    }
}
