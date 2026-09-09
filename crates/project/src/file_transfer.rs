use gpui::{App, AppContext as _, Context, Entity, Task};
use parking_lot::Mutex;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
    Copy,
}
impl TransferDirection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Upload => "上传",
            Self::Download => "下载",
            Self::Copy => "复制",
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
}
impl Transfer {
    pub fn percentage(&self) -> Option<u64> {
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
            wake,
            _task: task,
            next_id: 0,
        }
    }
    pub fn entries(&self) -> Vec<Transfer> {
        self.entries.lock().clone()
    }
    pub fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.entries.lock().retain(|entry| entry.finished.is_none());
        cx.notify();
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
        self.entries.lock().retain(|entry| {
            entry.error
                || entry
                    .finished
                    .is_none_or(|finished| finished.elapsed() < Duration::from_secs(5))
        });
        self.entries.lock().push(Transfer {
            id,
            direction,
            path,
            destination,
            bytes: 0,
            total: None,
            completed_files: 0,
            total_entries: None,
            status: "正在准备".into(),
            finished: None,
            error: false,
        });
        cx.notify();
        TransferHandle {
            id,
            entries: self.entries.clone(),
            wake: self.wake.clone(),
        }
    }
}

#[derive(Clone)]
pub struct TransferHandle {
    id: usize,
    entries: Arc<Mutex<Vec<Transfer>>>,
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
    pub fn progress(&self, progress: worktree::FileTransferProgress) {
        self.update(|entry| match progress {
            worktree::FileTransferProgress::Started(path) => {
                entry.path = path;
                entry.bytes = 0;
                entry.total = None;
                entry.status = "正在传输".into();
            }
            worktree::FileTransferProgress::Bytes(bytes, total) => {
                entry.bytes = bytes;
                entry.total = Some(total);
                entry.status = if bytes >= total {
                    match entry.direction {
                        TransferDirection::Upload => "正在等待远端确认",
                        TransferDirection::Download => "正在写入本地文件",
                        TransferDirection::Copy => "正在等待复制完成",
                    }
                } else {
                    "正在传输"
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
        self.update(|entry| {
            entry.finished = Some(Instant::now());
            entry.error = result.is_err();
            entry.status = match result {
                Ok(()) => "已完成".into(),
                Err(error) => format!("失败：{error:#}"),
            };
            if result.is_ok() {
                entry.completed_files = entry.total_entries.unwrap_or(entry.completed_files.max(1));
                entry.bytes = entry.total.unwrap_or(entry.bytes);
            }
        });
    }
    pub fn track<T: Send + 'static>(
        &self,
        task: Task<anyhow::Result<T>>,
        cx: &App,
    ) -> Task<anyhow::Result<T>> {
        let handle = self.clone();
        let cancelled = handle.clone();
        let guard = util::defer(move || cancelled.finish(&Err(anyhow::anyhow!("传输已中断"))));
        cx.background_spawn(async move {
            let result = task.await;
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
                Task::<anyhow::Result<()>>::ready(Err(anyhow::anyhow!("磁盘写入失败"))),
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
