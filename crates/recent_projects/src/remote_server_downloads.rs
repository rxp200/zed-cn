use auto_update::remote_server_cache_path;
use auto_update::remote_server_downloads::{
    DownloadKey, PLATFORMS, RemoteServerDownloads, platform_supported,
};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    Window,
};
use release_channel::{AppVersion, CustomReleaseTag, ReleaseChannel};
use ui::{Checkbox, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

pub(crate) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.toggle_modal(window, cx, |_, cx| RemoteServerDownloadsModal::new(cx));
}

pub(crate) struct RemoteServerDownloadsModal {
    focus_handle: FocusHandle,
    custom: bool,
    selected: [bool; 6],
    statuses: [String; 6],
    store: Entity<RemoteServerDownloads>,
    task: Option<Task<()>>,
    _subscription: Subscription,
}

impl RemoteServerDownloadsModal {
    fn new(cx: &mut Context<Self>) -> Self {
        RemoteServerDownloads::ensure_global(cx);
        let store = RemoteServerDownloads::global(cx);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            custom: CustomReleaseTag::try_current(cx).is_some(),
            selected: [false; 6],
            statuses: std::array::from_fn(|_| String::new()),
            store: store.clone(),
            task: None,
            _subscription: cx.observe(&store, |this, store, cx| {
                if !store.read(cx).has_any_active() {
                    this.refresh(cx);
                }
                cx.notify();
            }),
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
            for (index, platform) in PLATFORMS.iter().enumerate() {
                let status = if !platform_supported(custom, channel, index, tag.is_some()) {
                    i18n::t!("b77ef0d9ca5014ab").to_owned()
                } else if !custom && channel == ReleaseChannel::Nightly {
                    i18n::t!("c95db392cea4388f").to_owned()
                } else {
                    let path = remote_server_cache_path(
                        channel,
                        &version,
                        if custom { tag.as_deref() } else { None },
                        platform.os,
                        platform.arch,
                    );
                    match smol::fs::metadata(path).await {
                        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => format!(
                            "{} · {:.1} MiB",
                            i18n::t!("f0b0738f75b8bcd1"),
                            metadata.len() as f64 / 1048576.
                        ),
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
        let store = self.store.clone();
        store.update(cx, |store, cx| {
            for (index, platform) in PLATFORMS.iter().enumerate() {
                if selected[index] {
                    store.start(
                        DownloadKey {
                            custom,
                            platform: *platform,
                        },
                        cx,
                    );
                }
            }
        });
    }

    fn cancel_download(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.cancel_all(cx));
    }

    fn delete(&mut self, index: usize, cx: &mut Context<Self>) {
        let tag = CustomReleaseTag::try_current(cx);
        let channel = ReleaseChannel::global(cx);
        if !self.custom && channel == ReleaseChannel::Nightly {
            return;
        }
        let Some(platform) = PLATFORMS.get(index) else {
            return;
        };
        let path = remote_server_cache_path(
            channel,
            &AppVersion::global(cx),
            if self.custom { tag.as_deref() } else { None },
            platform.os,
            platform.arch,
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

    fn live_status(&self, cx: &App, index: usize) -> Option<String> {
        let key = DownloadKey {
            custom: self.custom,
            platform: *PLATFORMS.get(index)?,
        };
        self.store
            .read(cx)
            .entries()
            .into_iter()
            .find(|entry| entry.key == key && entry.is_visible())
            .map(|entry| entry.status)
    }
}

impl EventEmitter<DismissEvent> for RemoteServerDownloadsModal {}
impl ModalView for RemoteServerDownloadsModal {}
impl Focusable for RemoteServerDownloadsModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Render for RemoteServerDownloadsModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let channel = ReleaseChannel::global(cx);
        let tag = CustomReleaseTag::try_current(cx);
        let version = if self.custom {
            tag.clone().unwrap_or_default()
        } else {
            AppVersion::global(cx).to_string()
        };
        let busy = self.store.read(cx).has_active(self.custom);
        let any_active = self.store.read(cx).has_any_active();
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
                        Button::new("official", i18n::t!("f4d01e6a6436dd7a")).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.custom = false;
                                this.refresh(cx);
                            }),
                        ),
                    )
                    .child(
                        Button::new("custom", "Zed CN")
                            .disabled(tag.is_none())
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
            .children(PLATFORMS.iter().enumerate().map(|(index, platform)| {
                let available = platform_supported(self.custom, channel, index, tag.is_some());
                let status = self
                    .live_status(cx, index)
                    .unwrap_or_else(|| self.statuses[index].clone());
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Checkbox::new(("platform", index), ToggleState::from(self.selected[index]))
                            .label(platform.label)
                            .disabled(!available)
                            .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                                this.selected[index] = state.selected();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new(status).size(LabelSize::Small)),
                    )
                    .child(
                        IconButton::new(("delete", index), IconName::Trash)
                            .disabled(
                                !available || (!self.custom && channel == ReleaseChannel::Nightly),
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
                                busy || !self.selected.iter().enumerate().any(
                                    |(index, selected)| {
                                        *selected
                                            && platform_supported(
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
                            .disabled(!any_active)
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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn init_dev(cx: &mut gpui::TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(|cx| {
            release_channel::init_test(semver::Version::new(1, 22, 0), ReleaseChannel::Dev, cx)
        });
    }

    #[gpui::test]
    fn closing_the_modal_does_not_cancel_a_running_download(cx: &mut gpui::TestAppContext) {
        init_dev(cx);
        cx.update(RemoteServerDownloads::ensure_global);
        let store = cx.update(|cx| RemoteServerDownloads::global(cx));
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let task = cx.spawn(move |_| async move {
            let _guard = util::defer(move || flag.store(true, Ordering::SeqCst));
            futures::future::pending::<()>().await;
        });
        store.update(cx, |store, _| {
            store.insert_test_download(
                DownloadKey {
                    custom: false,
                    platform: PLATFORMS[0],
                },
                task,
            );
        });
        assert!(store.read_with(cx, |store, _| store.has_any_active()));

        let modal = cx.new(RemoteServerDownloadsModal::new);
        drop(modal);
        cx.run_until_parked();

        assert!(
            !cancelled.load(Ordering::SeqCst),
            "the download task must keep running after the modal is dismissed"
        );
        assert!(store.read_with(cx, |store, _| store.has_any_active()));
    }

    #[gpui::test]
    fn cancelling_from_the_store_ends_the_download(cx: &mut gpui::TestAppContext) {
        init_dev(cx);
        cx.update(RemoteServerDownloads::ensure_global);
        let store = cx.update(|cx| RemoteServerDownloads::global(cx));
        store.update(cx, |store, _| {
            store.insert_test_download(
                DownloadKey {
                    custom: false,
                    platform: PLATFORMS[0],
                },
                Task::ready(()),
            );
        });
        store.update(cx, |store, cx| store.cancel_all(cx));
        let entry = store.read_with(cx, |store, _| store.entries().remove(0));
        assert!(entry.cancelled);
        assert!(!entry.is_active());
        assert!(!store.read_with(cx, |store, _| store.has_any_active()));
    }

    #[test]
    fn available_platforms_respect_distribution_and_build_identity() {
        for index in 0..PLATFORMS.len() {
            assert!(!platform_supported(
                true,
                ReleaseChannel::Stable,
                index,
                false
            ));
            assert_eq!(
                platform_supported(true, ReleaseChannel::Stable, index, true),
                index != 5
            );
            assert!(platform_supported(
                false,
                ReleaseChannel::Stable,
                index,
                false
            ));
            assert!(!platform_supported(
                false,
                ReleaseChannel::Dev,
                index,
                false
            ));
        }
    }
}
