use gpui::{App, AppContext as _, Context, Entity, Task};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

/// How long a finished transfer stays in the list before it is pruned.
const FINISHED_RETENTION: Duration = Duration::from_secs(5);

/// An entry that is already cancelled is not a failure, so callers that await
/// the tracked task can tell a user cancellation from a real error.
#[derive(Debug)]
pub struct TransferCancelled;
impl fmt::Display for TransferCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(i18n::t!("166b9cd3f5c5a86b"))
    }
}
impl std::error::Error for TransferCancelled {}

fn cancelled_error() -> anyhow::Error {
    anyhow::Error::new(TransferCancelled)
}

/// Keeps failures and user-cancelled transfers until the user clears them, and
/// successful ones for a short window.
fn is_retained(entry: &Transfer) -> bool {
    entry.error
        || entry.cancelled
        || entry
            .finished
            .is_none_or(|finished| finished.elapsed() < FINISHED_RETENTION)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
    Copy,
}
impl TransferDirection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Upload => i18n::t!("9e07e3c0532d4976"),
            Self::Download => i18n::t!("4673a23061656125"),
            Self::Copy => i18n::t!("63d90d977348ab1f"),
        }
    }
}

#[derive(Clone)]
pub struct Transfer {
    pub id: usize,
    pub direction: TransferDirection,
    pub path: String,
    pub destination: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub completed_files: usize,
    pub total_entries: Option<usize>,
    pub status: String,
    pub finished: Option<Instant>,
    pub error: bool,
    pub cancelled: bool,
}
impl Transfer {
    pub fn percentage(&self) -> Option<u64> {
        if self.cancelled {
            return None;
        }
        if self.finished.is_some() && !self.error {
            return Some(100);
        }
        self.total
            .filter(|total| *total > 0)
            .map(|total| (self.bytes as u128 * 100 / total as u128).min(99) as u64)
    }
}

pub struct FileTransfers {
    entries: Arc<Mutex<Vec<Transfer>>>,
    /// Cancellation signals for in-flight transfers, keyed by transfer id.
    cancellations: Arc<Mutex<HashMap<usize, async_channel::Sender<()>>>>,
    wake: async_channel::Sender<()>,
    _task: Task<()>,
    next_id: usize,
}
impl FileTransfers {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (wake, receiver) = async_channel::bounded(1);
        let task = cx.spawn(async move |this, cx| {
            while receiver.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        });
        Self {
            entries: Default::default(),
            cancellations: Default::default(),
            wake,
            _task: task,
            next_id: 0,
        }
    }
    pub fn entries(&self) -> Vec<Transfer> {
        self.entries.lock().clone()
    }
    pub fn clear_finished(&mut self, cx: &mut Context<Self>) {
        let mut entries = self.entries.lock();
        entries.retain(|entry| entry.finished.is_none());
        self.cancellations
            .lock()
            .retain(|id, _| entries.iter().any(|entry| entry.id == *id));
        drop(entries);
        cx.notify();
    }
    /// Stops an in-flight transfer. The tracked task notices the signal, drops
    /// its work (so partial downloads clean up through their own guards) and
    /// resolves with [`TransferCancelled`]. Returns `false` when the transfer
    /// already finished, was already cancelled, or is unknown.
    pub fn cancel(&mut self, id: usize, cx: &mut Context<Self>) -> bool {
        let sender = self.cancellations.lock().get(&id).cloned();
        let Some(sender) = sender else {
            return false;
        };
        match sender.try_send(()) {
            Ok(()) | Err(async_channel::TrySendError::Full(())) => {}
            Err(async_channel::TrySendError::Closed(())) => return false,
        }
        cx.notify();
        true
    }
    pub fn start(
        &mut self,
        direction: TransferDirection,
        path: String,
        destination: String,
        cx: &mut Context<Self>,
    ) -> TransferHandle {
        self.next_id += 1;
        let id = self.next_id;
        let mut entries = self.entries.lock();
        entries.retain(is_retained);
        self.cancellations
            .lock()
            .retain(|id, _| entries.iter().any(|entry| entry.id == *id));
        entries.push(Transfer {
            id,
            direction,
            path,
            destination,
            bytes: 0,
            total: None,
            completed_files: 0,
            total_entries: None,
            status: i18n::t!("5f2ed701d82cd70e").into(),
            finished: None,
            error: false,
            cancelled: false,
        });
        drop(entries);

        let (cancel_sender, cancel_receiver) = async_channel::bounded(1);
        self.cancellations.lock().insert(id, cancel_sender);
        cx.notify();
        TransferHandle {
            id,
            entries: self.entries.clone(),
            cancellations: self.cancellations.clone(),
            cancel_receiver: Arc::new(Mutex::new(Some(cancel_receiver))),
            wake: self.wake.clone(),
        }
    }
}

#[derive(Clone)]
pub struct TransferHandle {
    id: usize,
    entries: Arc<Mutex<Vec<Transfer>>>,
    cancellations: Arc<Mutex<HashMap<usize, async_channel::Sender<()>>>>,
    cancel_receiver: Arc<Mutex<Option<async_channel::Receiver<()>>>>,
    wake: async_channel::Sender<()>,
}
impl TransferHandle {
    fn update(&self, update: impl FnOnce(&mut Transfer)) {
        if let Some(entry) = self
            .entries
            .lock()
            .iter_mut()
            .find(|entry| entry.id == self.id)
        {
            update(entry);
        }
        match self.wake.try_send(()) {
            Ok(())
            | Err(async_channel::TrySendError::Full(()))
            | Err(async_channel::TrySendError::Closed(())) => {}
        }
    }
    /// Ends the entry and stops tracking its cancellation channel, so a later
    /// `cancel` cannot resolve an already-finished transfer.
    fn complete(&self, update: impl FnOnce(&mut Transfer)) {
        self.update(update);
        self.cancellations.lock().remove(&self.id);
    }
    pub fn progress(&self, progress: worktree::FileTransferProgress) {
        self.update(|entry| match progress {
            worktree::FileTransferProgress::Started(path) => {
                entry.path = path;
                entry.bytes = 0;
                entry.total = None;
                entry.status = i18n::t!("e0f3db545a0fe3d5").into();
            }
            worktree::FileTransferProgress::Bytes(bytes, total) => {
                entry.bytes = bytes;
                entry.total = Some(total);
                entry.status = if bytes >= total {
                    match entry.direction {
                        TransferDirection::Upload => i18n::t!("1482851dc28f43eb"),
                        TransferDirection::Download => i18n::t!("2b15097c138fbffe"),
                        TransferDirection::Copy => i18n::t!("5f3135e58a1e6010"),
                    }
                } else {
                    i18n::t!("e0f3db545a0fe3d5")
                }
                .into();
            }
            worktree::FileTransferProgress::Finished => {
                entry.completed_files += 1;
            }
            worktree::FileTransferProgress::TotalEntries(total) => {
                entry.total_entries = Some(total)
            }
        });
    }
    pub fn finish(&self, result: &anyhow::Result<()>) {
        if self
            .entries
            .lock()
            .iter()
            .any(|entry| entry.id == self.id && entry.cancelled)
        {
            return;
        }
        self.complete(|entry| {
            entry.finished = Some(Instant::now());
            entry.error = result.is_err();
            entry.status = match result {
                Ok(()) => i18n::t!("f28461bb49c85647").to_string(),
                Err(error) => i18n::t!("8f5d896ae85cd3aa", error = error),
            };
            if result.is_ok() {
                entry.completed_files = entry.total_entries.unwrap_or(entry.completed_files.max(1));
                entry.bytes = entry.total.unwrap_or(entry.bytes);
            }
        });
    }
    /// Ends the entry as user-cancelled (not a failure). Kept visible until the
    /// user clears finished transfers.
    fn finish_cancelled(&self) {
        self.complete(|entry| {
            entry.finished = Some(Instant::now());
            entry.cancelled = true;
            entry.error = false;
            entry.status = i18n::t!("71e03cd890f413e3").into();
        });
    }
    pub fn track<T: Send + 'static>(
        &self,
        task: Task<anyhow::Result<T>>,
        cx: &App,
    ) -> Task<anyhow::Result<T>> {
        let handle = self.clone();
        let cancelled = handle.clone();
        let guard = util::defer(move || {
            cancelled.finish(&Err(anyhow::anyhow!(i18n::t!("265b461c5c3d102a"))))
        });
        let cancel_receiver = self.cancel_receiver.lock().take();
        cx.background_spawn(async move {
            let result = match cancel_receiver {
                Some(cancel_receiver) => {
                    let cancel = wait_for_cancel(cancel_receiver);
                    let task = Box::pin(task);
                    futures::pin_mut!(cancel);
                    match futures::future::select(cancel, task).await {
                        futures::future::Either::Left(((), task)) => {
                            // Dropping the task aborts the transfer, and its own
                            // guards remove partially written files.
                            drop(task);
                            handle.finish_cancelled();
                            guard.abort();
                            return Err(cancelled_error());
                        }
                        futures::future::Either::Right((result, _cancel)) => result,
                    }
                }
                None => task.await,
            };
            handle.finish(
                &result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| anyhow::anyhow!("{error:#}")),
            );
            guard.abort();
            result
        })
    }
}

/// Resolves only when the transfer is cancelled on purpose. A closed channel
/// means the store stopped tracking this transfer (it finished or was cleared),
/// which must never look like a cancellation.
async fn wait_for_cancel(receiver: async_channel::Receiver<()>) {
    if receiver.recv().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn test_download_empty_file_completion(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("nested/empty.bin");
        let store = cx.new(FileTransfers::new);
        let progress = store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Download,
                "empty.bin".into(),
                destination.display().to_string(),
                cx,
            )
        });
        let (completion, receiver) = futures::channel::oneshot::channel();
        let file = crate::DownloadingFile {
            destination_path: destination.clone(),
            chunks: Vec::new(),
            total_size: 0,
            file_id: Some(1),
            progress,
            completion,
        };
        file.write().await;
        receiver.await.expect("completion").expect("write");
        assert_eq!(
            std::fs::read(&destination).expect("empty file"),
            Vec::<u8>::new()
        );
    }

    #[gpui::test]
    async fn test_download_completion_reports_write_failure(cx: &mut TestAppContext) {
        let store = cx.new(FileTransfers::new);
        let progress = store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Download,
                "broken.bin".into(),
                "/unused".into(),
                cx,
            )
        });
        let (completion, receiver) = futures::channel::oneshot::channel();
        let file = crate::DownloadingFile {
            destination_path: "/unused".into(),
            chunks: vec![1],
            total_size: 2,
            file_id: Some(1),
            progress,
            completion,
        };
        file.write().await;
        let error = receiver
            .await
            .expect("completion must be delivered")
            .expect_err("size mismatch");
        assert!(error.to_string().contains("大小不匹配"));
    }

    #[gpui::test]
    async fn test_cancel_stops_task_and_marks_entry_cancelled(cx: &mut TestAppContext) {
        let store = cx.new(FileTransfers::new);
        let handle = store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Download,
                "huge.bin".into(),
                "/tmp/huge.bin".into(),
                cx,
            )
        });
        handle.progress(worktree::FileTransferProgress::Bytes(10, 100));
        let task = cx.update(|cx| {
            handle.track(
                cx.background_spawn(async {
                    futures::future::pending::<anyhow::Result<()>>().await
                }),
                cx,
            )
        });

        assert!(store.update(cx, |store, cx| store.cancel(handle.id, cx)));
        let error = task.await.expect_err("cancel must end the task");
        assert!(
            error.downcast_ref::<TransferCancelled>().is_some(),
            "cancel must be distinguishable from a real failure: {error:#}"
        );

        let entries = store.read_with(cx, |store, _| store.entries());
        let entry = &entries[0];
        assert!(entry.cancelled);
        assert!(!entry.error);
        assert!(entry.finished.is_some());
        assert_eq!(entry.status, i18n::t!("71e03cd890f413e3"));
        assert_eq!(entry.percentage(), None);

        // A cancelled transfer is no longer tracked, so it cannot be cancelled twice.
        assert!(!store.update(cx, |store, cx| store.cancel(handle.id, cx)));

        // Cancelled entries stay visible like failures do, and clearing removes them.
        store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Upload,
                "next.bin".into(),
                "/project".into(),
                cx,
            )
        });
        assert!(store.read_with(cx, |store, _| {
            store.entries().iter().any(|entry| entry.cancelled)
        }));
        store.update(cx, |store, cx| store.clear_finished(cx));
        assert!(!store.read_with(cx, |store, _| {
            store.entries().iter().any(|entry| entry.cancelled)
        }));
    }

    #[gpui::test]
    async fn test_completed_transfer_ignores_cancel(cx: &mut TestAppContext) {
        let store = cx.new(FileTransfers::new);
        let handle = store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Download,
                "done.bin".into(),
                "/tmp/done.bin".into(),
                cx,
            )
        });
        let task = cx.update(|cx| handle.track(Task::ready(Ok(())), cx));
        task.await.expect("transfer");

        let entry = store.read_with(cx, |store, _| store.entries()[0].clone());
        assert!(entry.finished.is_some());
        assert!(!entry.cancelled);
        assert_eq!(entry.percentage(), Some(100));
        assert!(!store.update(cx, |store, cx| store.cancel(handle.id, cx)));
        assert!(!store.read_with(cx, |store, _| store.entries()[0].cancelled));
    }

    #[gpui::test]
    async fn test_transfer_progress_waits_for_confirmation(cx: &mut TestAppContext) {
        let store = cx.new(FileTransfers::new);
        let handle = store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Upload,
                "test.bin".into(),
                "/project".into(),
                cx,
            )
        });
        handle.progress(worktree::FileTransferProgress::Bytes(50, 100));
        assert_eq!(
            store.read_with(cx, |store, _| store.entries()[0].percentage()),
            Some(50)
        );
        handle.progress(worktree::FileTransferProgress::Bytes(100, 100));
        assert_eq!(
            store.read_with(cx, |store, _| store.entries()[0].percentage()),
            Some(99)
        );
        let task = cx.update(|cx| handle.track(Task::ready(Ok(())), cx));
        task.await.expect("transfer");
        assert_eq!(
            store.read_with(cx, |store, _| store.entries()[0].percentage()),
            Some(100)
        );
        let failed = store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Download,
                "failed.bin".into(),
                "/missing".into(),
                cx,
            )
        });
        let task = cx.update(|cx| {
            failed.track(
                Task::<anyhow::Result<()>>::ready(Err(anyhow::anyhow!(i18n::t!(
                    "6b6cc7dd50c6ad6a"
                )))),
                cx,
            )
        });
        assert!(task.await.is_err());
        assert!(store.read_with(cx, |store, _| {
            store
                .entries()
                .iter()
                .any(|entry| entry.error && entry.status.contains("磁盘写入失败"))
        }));
        store.update(cx, |store, cx| store.clear_finished(cx));
        assert!(store.read_with(cx, |store, _| store.entries().is_empty()));
        let cancelled = store.update(cx, |store, cx| {
            store.start(
                TransferDirection::Upload,
                "cancelled.bin".into(),
                "/project".into(),
                cx,
            )
        });
        let task = cx.update(|cx| {
            cancelled.track(
                cx.background_spawn(async {
                    futures::future::pending::<anyhow::Result<()>>().await
                }),
                cx,
            )
        });
        drop(task);
        cx.run_until_parked();
        assert!(store.read_with(cx, |store, _| {
            store.entries().iter().any(|entry| entry.error)
        }));
    }
}

pub fn store(project: &Entity<crate::Project>, cx: &mut App) -> Entity<FileTransfers> {
    project.update(cx, |project, cx| {
        project
            .file_transfers
            .get_or_insert_with(|| cx.new(FileTransfers::new))
            .clone()
    })
}
