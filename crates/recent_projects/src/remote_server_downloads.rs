use auto_update::{AutoUpdater, remote_server_cache_path};
use gpui::{App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Task, Window};
use release_channel::{AppVersion, CustomReleaseTag, ReleaseChannel};
use ui::{Checkbox, prelude::*};
use workspace::{ModalView, Workspace};

const PLATFORMS: [(&str, &str, &str); 6] = [
    ("linux", "x86_64", "Linux x86_64"),
    ("linux", "aarch64", "Linux ARM64"),
    ("windows", "x86_64", "Windows x86_64"),
    ("windows", "aarch64", "Windows ARM64"),
    ("macos", "aarch64", "macOS Apple Silicon"),
    ("macos", "x86_64", "macOS Intel"),
];

pub(crate) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.toggle_modal(window, cx, |_, cx| RemoteServerDownloads::new(cx));
}

pub(crate) struct RemoteServerDownloads {
    focus_handle: FocusHandle,
    custom: bool,
    selected: [bool; 6],
    statuses: [String; 6],
    busy: bool,
    task: Option<Task<()>>,
}

fn supported(custom: bool, channel: ReleaseChannel, index: usize, has_tag: bool) -> bool {
    if custom {
        has_tag && index != 5
    } else {
        channel != ReleaseChannel::Dev
    }
}

impl RemoteServerDownloads {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            custom: CustomReleaseTag::try_current(cx).is_some(),
            selected: [false; 6],
            statuses: std::array::from_fn(|_| String::new()),
            busy: false,
            task: None,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let custom = self.custom;
        let tag = CustomReleaseTag::try_current(cx);
        let version = AppVersion::global(cx);
        let channel = ReleaseChannel::global(cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            for (index, (os, arch, _)) in PLATFORMS.iter().enumerate() {
                let status = if !supported(custom, channel, index, tag.is_some()) {
                    i18n::t!("b77ef0d9ca5014ab").to_owned()
                } else if !custom && channel == ReleaseChannel::Nightly {
                    i18n::t!("c95db392cea4388f").to_owned()
                } else {
                    let path = remote_server_cache_path(
                        channel,
                        &version,
                        if custom { tag.as_deref() } else { None },
                        os,
                        arch,
                    );
                    match smol::fs::metadata(path).await {
                        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {
                            format!(
                                "{} · {:.1} MiB",
                                i18n::t!("f0b0738f75b8bcd1"),
                                metadata.len() as f64 / 1048576.
                            )
                        }
                        Ok(_) => i18n::t!("c0a27d0e14c8d5f9").to_owned(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            i18n::t!("5569a4e0a4ce34fe").to_owned()
                        }
                        Err(error) => error.to_string(),
                    }
                };
                if this
                    .update(cx, |this, cx| {
                        this.statuses[index] = status;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn download(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected;
        let custom = self.custom;
        let tag = CustomReleaseTag::try_current(cx);
        let channel = ReleaseChannel::global(cx);
        let version = AppVersion::global(cx);
        self.busy = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            for (index, (os, arch, _)) in PLATFORMS.iter().enumerate() {
                if !selected[index] || !supported(custom, channel, index, tag.is_some()) {
                    continue;
                }
                let status_entity = this.clone();
                let status = move |status: &str, cx: &mut gpui::AsyncApp| {
                    status_entity
                        .update(cx, |this, cx| {
                            this.statuses[index] = status.to_owned();
                            cx.notify();
                        })
                        .log_err();
                };
                let progress_entity = this.clone();
                let progress = move |progress: Option<f32>, cx: &mut gpui::AsyncApp| {
                    if let Some(progress) = progress {
                        progress_entity
                            .update(cx, |this, cx| {
                                this.statuses[index] = format!(
                                    "{} {:.0}%",
                                    i18n::t!("37b345555d7ea6c1"),
                                    progress * 100.
                                );
                                cx.notify();
                            })
                            .log_err();
                    }
                };
                let result = if custom {
                    match tag.as_ref() {
                        Some(tag) => {
                            AutoUpdater::download_custom_remote_server_release(
                                tag.clone(),
                                os,
                                arch,
                                status,
                                progress,
                                cx,
                            )
                            .await
                        }
                        None => Err(anyhow::anyhow!(i18n::t!("b77ef0d9ca5014ab"))),
                    }
                } else {
                    AutoUpdater::download_remote_server_release(
                        channel,
                        if channel == ReleaseChannel::Nightly {
                            None
                        } else {
                            Some(version.clone())
                        },
                        os,
                        arch,
                        status,
                        progress,
                        cx,
                    )
                    .await
                };
                let status = match result {
                    Ok(path) => match smol::fs::metadata(path).await {
                        Ok(metadata) => format!(
                            "{} · {:.1} MiB",
                            i18n::t!("f0b0738f75b8bcd1"),
                            metadata.len() as f64 / 1048576.
                        ),
                        Err(error) => error.to_string(),
                    },
                    Err(error) => format!("{error:#}"),
                };
                if this
                    .update(cx, |this, cx| {
                        this.statuses[index] = status;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.busy = false;
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn cancel_download(&mut self, cx: &mut Context<Self>) {
        self.task = None;
        self.busy = false;
        self.refresh(cx);
    }

    fn delete(&mut self, index: usize, cx: &mut Context<Self>) {
        let tag = CustomReleaseTag::try_current(cx);
        let channel = ReleaseChannel::global(cx);
        if !self.custom && channel == ReleaseChannel::Nightly {
            return;
        }
        let Some((os, arch, _)) = PLATFORMS.get(index) else {
            return;
        };
        let path = remote_server_cache_path(
            channel,
            &AppVersion::global(cx),
            if self.custom { tag.as_deref() } else { None },
            os,
            arch,
        );
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = smol::fs::remove_file(path).await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.refresh(cx),
                Err(error) => {
                    this.statuses[index] = error.to_string();
                    cx.notify();
                }
            })
            .log_err();
        }));
    }
}

use util::ResultExt;
impl EventEmitter<DismissEvent> for RemoteServerDownloads {}
impl ModalView for RemoteServerDownloads {}
impl Focusable for RemoteServerDownloads {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Render for RemoteServerDownloads {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let channel = ReleaseChannel::global(cx);
        let tag = CustomReleaseTag::try_current(cx);
        let version = if self.custom {
            tag.clone().unwrap_or_default()
        } else {
            AppVersion::global(cx).to_string()
        };
        v_flex()
            .elevation_3(cx)
            .w(rems(38.))
            .p_4()
            .gap_3()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(Label::new(i18n::t!("eeaf60325cae1c33")))
            .child(
                Label::new(i18n::t!("f6512b144ea090ef"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("official", i18n::t!("f4d01e6a6436dd7a"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.custom = false;
                                this.refresh(cx);
                            })),
                    )
                    .child(
                        Button::new("custom", "Zed CN")
                            .disabled(self.busy || tag.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.custom = true;
                                this.refresh(cx);
                            })),
                    ),
            )
            .child(Label::new(format!(
                "{} · {version}",
                if self.custom {
                    "Zed CN"
                } else {
                    channel.display_name()
                }
            )))
            .children(PLATFORMS.iter().enumerate().map(|(index, (_, _, label))| {
                let available = supported(self.custom, channel, index, tag.is_some());
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Checkbox::new(("platform", index), ToggleState::from(self.selected[index]))
                            .label(*label)
                            .disabled(self.busy || !available)
                            .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                                this.selected[index] = state.selected();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new(self.statuses[index].clone()).size(LabelSize::Small)),
                    )
                    .child(
                        IconButton::new(("delete", index), IconName::Trash)
                            .disabled(
                                self.busy
                                    || !available
                                    || (!self.custom && channel == ReleaseChannel::Nightly),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.delete(index, cx))),
                    )
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("download", i18n::t!("bf99ac442d15f953"))
                            .disabled(
                                self.busy
                                    || !self.selected.iter().enumerate().any(
                                        |(index, selected)| {
                                            *selected
                                                && supported(
                                                    self.custom,
                                                    channel,
                                                    index,
                                                    tag.is_some(),
                                                )
                                        },
                                    ),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.download(cx))),
                    )
                    .child(
                        Button::new("cancel-download", i18n::t!("c0c05b34c0b1ed98"))
                            .disabled(!self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_download(cx);
                            })),
                    )
                    .child(
                        Button::new("close", i18n::t!("7d9eb7acb13e2462"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn cancellation_preserves_selection_and_refreshes_without_downloading(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        cx.update(|cx| {
            release_channel::init_test(semver::Version::new(1, 22, 0), ReleaseChannel::Dev, cx)
        });
        let downloads = cx.new(RemoteServerDownloads::new);
        downloads.update(cx, |this, cx| {
            this.selected[0] = true;
            this.busy = true;
            this.task = Some(cx.spawn(async move |_, _| futures::future::pending::<()>().await));
            this.cancel_download(cx);
            assert!(!this.busy);
            assert!(this.selected[0]);
        });
        cx.run_until_parked();
        downloads.read_with(cx, |this, _| {
            assert!(
                this.statuses
                    .iter()
                    .all(|status| status == i18n::t!("b77ef0d9ca5014ab"))
            );
        });
    }

    #[test]
    fn available_platforms_respect_distribution_and_build_identity() {
        for index in 0..PLATFORMS.len() {
            assert!(!supported(true, ReleaseChannel::Stable, index, false));
            assert_eq!(
                supported(true, ReleaseChannel::Stable, index, true),
                index != 5
            );
            assert!(supported(false, ReleaseChannel::Stable, index, false));
            assert!(!supported(false, ReleaseChannel::Dev, index, false));
        }
    }
}
