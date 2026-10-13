use crate::{AutoUpdater, remote_server_cache_path};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, Task, WeakEntity};
use release_channel::{AppVersion, CustomReleaseTag, ReleaseChannel};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use util::ResultExt as _;

/// How long a finished download stays visible before it is pruned.
pub const FINISHED_RETENTION: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
    pub label: &'static str,
}

pub const PLATFORMS: [Platform; 6] = [
    Platform {
        os: "linux",
        arch: "x86_64",
        label: "Linux x86_64",
    },
    Platform {
        os: "linux",
        arch: "aarch64",
        label: "Linux ARM64",
    },
    Platform {
        os: "windows",
        arch: "x86_64",
        label: "Windows x86_64",
    },
    Platform {
        os: "windows",
        arch: "aarch64",
        label: "Windows ARM64",
    },
    Platform {
        os: "macos",
        arch: "aarch64",
        label: "macOS Apple Silicon",
    },
    Platform {
        os: "macos",
        arch: "x86_64",
        label: "macOS Intel",
    },
];

/// Identifies one downloadable remote server archive.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownloadKey {
    pub custom: bool,
    pub platform: Platform,
}

/// Whether the running build can fetch the given platform from the given source.
pub fn platform_supported(
    custom: bool,
    channel: ReleaseChannel,
    index: usize,
    has_tag: bool,
) -> bool {
    if custom {
        has_tag && index != 5
    } else {
        channel != ReleaseChannel::Dev
    }
}

#[derive(Clone)]
pub struct RemoteServerDownload {
    pub id: usize,
    pub key: DownloadKey,
    pub path: PathBuf,
    pub progress: Option<f32>,
    pub status: String,
    pub finished: Option<Instant>,
    pub error: bool,
    pub cancelled: bool,
}

impl RemoteServerDownload {
    pub fn percentage(&self) -> Option<f32> {
        if self.finished.is_some() && !self.error && !self.cancelled {
            return Some(1.0);
        }
        self.progress.map(|progress| progress.clamp(0.0, 0.99))
    }

    pub fn is_active(&self) -> bool {
        self.finished.is_none()
    }

    pub fn is_visible(&self) -> bool {
        self.finished
            .is_none_or(|finished| finished.elapsed() < FINISHED_RETENTION)
    }
}

struct GlobalRemoteServerDownloads(Entity<RemoteServerDownloads>);

impl Global for GlobalRemoteServerDownloads {}

/// Application-wide store for remote server pre-downloads.
///
/// Downloads are owned here rather than by any view so that closing the
/// predownload dialog keeps them running. A single permit keeps the downloads
/// sequential, matching the previous modal behavior.
pub struct RemoteServerDownloads {
    entries: Vec<RemoteServerDownload>,
    tasks: Vec<(usize, Task<()>)>,
    semaphore: Arc<async_lock::Semaphore>,
    next_id: usize,
}

impl RemoteServerDownloads {
    pub fn init_global(cx: &mut App) {
        let store = cx.new(Self::new);
        cx.set_global(GlobalRemoteServerDownloads(store));
    }

    pub fn ensure_global(cx: &mut App) {
        if cx.try_global::<GlobalRemoteServerDownloads>().is_none() {
            Self::init_global(cx);
        }
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalRemoteServerDownloads>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalRemoteServerDownloads>()
            .map(|global| global.0.clone())
    }

    fn new(_: &mut Context<Self>) -> Self {
        Self {
            entries: Vec::new(),
            tasks: Vec::new(),
            semaphore: Arc::new(async_lock::Semaphore::new(1)),
            next_id: 0,
        }
    }

    pub fn entries(&self) -> Vec<RemoteServerDownload> {
        self.entries.clone()
    }

    /// Registers a download whose task is supplied by the caller. Only used by
    /// tests to model an in-flight download without network access.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_test_download(&mut self, key: DownloadKey, task: Task<()>) -> usize {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push(RemoteServerDownload {
            id,
            key,
            path: PathBuf::new(),
            progress: None,
            status: String::new(),
            finished: None,
            error: false,
            cancelled: false,
        });
        self.tasks.push((id, task));
        id
    }

    pub fn active_entries(&self) -> Vec<RemoteServerDownload> {
        self.entries
            .iter()
            .filter(|entry| entry.is_active())
            .cloned()
            .collect()
    }

    pub fn has_active(&self, custom: bool) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.is_active() && entry.key.custom == custom)
    }

    pub fn has_any_active(&self) -> bool {
        self.entries.iter().any(|entry| entry.is_active())
    }

    /// Queues a download for `key`, returning its entry id.
    ///
    /// Returns `None` when the platform is unsupported by this build or an
    /// identical download is already running.
    pub fn start(&mut self, key: DownloadKey, cx: &mut Context<Self>) -> Option<usize> {
        self.prune_entries();
        self.prune_tasks();
        if self
            .entries
            .iter()
            .any(|entry| entry.is_active() && entry.key == key)
        {
            return None;
        }
        let channel = ReleaseChannel::global(cx);
        let tag = CustomReleaseTag::try_current(cx);
        if key.custom && tag.is_none() {
            return None;
        }
        let index = PLATFORMS
            .iter()
            .position(|platform| *platform == key.platform)?;
        if !platform_supported(key.custom, channel, index, tag.is_some()) {
            return None;
        }

        let version = AppVersion::global(cx);
        let path = remote_server_cache_path(
            channel,
            &version,
            if key.custom { tag.as_deref() } else { None },
            key.platform.os,
            key.platform.arch,
        );
        let official_version = (channel != ReleaseChannel::Nightly).then(|| version.clone());
        let custom_tag = if key.custom { tag } else { None };
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push(RemoteServerDownload {
            id,
            key,
            path,
            progress: None,
            status: i18n::t!("5f2ed701d82cd70e").to_owned(),
            finished: None,
            error: false,
            cancelled: false,
        });

        let semaphore = self.semaphore.clone();
        let task = cx.spawn(async move |this, cx| {
            let _permit = semaphore.acquire_arc().await;
            let status_entity: WeakEntity<Self> = this.clone();
            let set_status = move |status: &str, cx: &mut AsyncApp| {
                status_entity
                    .update(cx, |store, cx| store.set_status(id, status, cx))
                    .log_err();
            };
            let progress_entity: WeakEntity<Self> = this.clone();
            let set_progress = move |progress: Option<f32>, cx: &mut AsyncApp| {
                if let Some(progress) = progress {
                    progress_entity
                        .update(cx, |store, cx| store.set_progress(id, progress, cx))
                        .log_err();
                }
            };
            let result = if key.custom {
                match custom_tag {
                    Some(tag) => {
                        AutoUpdater::download_custom_remote_server_release(
                            tag,
                            key.platform.os,
                            key.platform.arch,
                            set_status,
                            set_progress,
                            cx,
                        )
                        .await
                    }
                    None => Err(anyhow::anyhow!(i18n::t!("b77ef0d9ca5014ab"))),
                }
            } else {
                AutoUpdater::download_remote_server_release(
                    channel,
                    official_version,
                    key.platform.os,
                    key.platform.arch,
                    set_status,
                    set_progress,
                    cx,
                )
                .await
            };
            let (status, error) = match result {
                Ok(path) => match smol::fs::metadata(&path).await {
                    Ok(metadata) => (
                        format!(
                            "{} · {:.1} MiB",
                            i18n::t!("f0b0738f75b8bcd1"),
                            metadata.len() as f64 / 1048576.
                        ),
                        false,
                    ),
                    Err(error) => (error.to_string(), true),
                },
                Err(error) => (format!("{error:#}"), true),
            };
            this.update(cx, |store, cx| store.finish(id, status, error, cx))
                .log_err();
        });
        self.tasks.push((id, task));
        cx.notify();
        Some(id)
    }

    /// Cancels a single download. Dropping the task aborts the transfer and
    /// removes any partially written temporary file.
    pub fn cancel(&mut self, id: usize, cx: &mut Context<Self>) {
        self.tasks.retain(|(task_id, _)| *task_id != id);
        self.mark_cancelled(id, cx);
    }

    pub fn cancel_all(&mut self, cx: &mut Context<Self>) {
        self.tasks.clear();
        let ids: Vec<usize> = self
            .entries
            .iter()
            .filter(|entry| entry.is_active())
            .map(|entry| entry.id)
            .collect();
        for id in ids {
            self.mark_cancelled(id, cx);
        }
    }

    pub fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.entries.retain(RemoteServerDownload::is_active);
        self.prune_tasks();
        cx.notify();
    }

    fn mark_cancelled(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id)
            && entry.is_active()
        {
            entry.finished = Some(Instant::now());
            entry.cancelled = true;
            entry.error = false;
            entry.status = i18n::t!("a37778f17c5f3ee5").to_owned();
        }
        cx.notify();
    }

    fn set_status(&mut self, id: usize, status: &str, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.status = status.to_owned();
        }
        cx.notify();
    }

    fn set_progress(&mut self, id: usize, progress: f32, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.progress = Some(progress);
            entry.status = format!("{} {:.0}%", i18n::t!("37b345555d7ea6c1"), progress * 100.);
        }
        cx.notify();
    }

    fn finish(&mut self, id: usize, status: String, error: bool, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.status = status;
            entry.error = error;
            entry.finished = Some(Instant::now());
            if !error {
                entry.progress = Some(1.0);
            }
        }
        cx.notify();
    }

    /// Drops completed task handles. Only finished tasks are dropped, so this
    /// never cancels a running download.
    fn prune_tasks(&mut self) {
        self.tasks.retain(|(_, task)| !task.is_ready());
    }

    /// Forgets finished entries once their retention window has passed. Errors
    /// stay until the user clears them.
    fn prune_entries(&mut self) {
        self.entries.retain(|entry| {
            entry.is_active()
                || entry.error
                || entry
                    .finished
                    .is_some_and(|finished| finished.elapsed() < FINISHED_RETENTION)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(index: usize) -> DownloadKey {
        DownloadKey {
            custom: false,
            platform: PLATFORMS[index],
        }
    }

    #[gpui::test]
    fn pruning_drops_expired_finished_downloads_but_keeps_errors(cx: &mut gpui::TestAppContext) {
        let store = cx.new(RemoteServerDownloads::new);
        store.update(cx, |store, cx| {
            store.insert_test_download(key(0), Task::ready(()));
            let error_id = store.insert_test_download(key(1), Task::ready(()));
            let active_id =
                store.insert_test_download(key(2), cx.background_spawn(futures::future::pending()));
            let expired = Instant::now() - FINISHED_RETENTION - Duration::from_secs(1);
            for entry in &mut store.entries {
                match entry.id {
                    id if id == error_id => {
                        entry.error = true;
                        entry.finished = Some(expired);
                    }
                    id if id != active_id => entry.finished = Some(expired),
                    _ => {}
                }
            }
            store.prune_entries();
        });
        let entries = store.read_with(cx, |store, _| store.entries());
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.error));
        assert!(entries.iter().any(|entry| entry.is_active()));
    }
}
