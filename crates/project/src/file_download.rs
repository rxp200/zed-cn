//! Client-driven, chunked download of files from a remote project.
//!
//! The older `DownloadFileByPath` path asks the server to push the whole file
//! as one burst of 1 MiB notifications on the shared RPC connection. That
//! queues the entire file ahead of terminal output on the server and spawns one
//! foreground task per chunk on the client, so downloading a large file stalled
//! every other remote terminal. This path instead pulls one bounded chunk per
//! request, writes it straight to the destination file, and paces between
//! requests while the user is interacting. It is only used when the server
//! advertises `download_chunks_v1` and the connection source allows custom
//! requests; official and older servers keep the legacy push path.

use anyhow::{Context as _, Result};
use gpui::BackgroundExecutor;
use rpc::{AnyProtoClient, network_pacing, network_pacing::TransferPacer, proto};
use smol::io::AsyncWriteExt as _;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use crate::file_transfer::TransferHandle;
use util::rel_path::RelPath;
use worktree::{FileTransferProgress, WorktreeId};

/// Largest file accepted through the chunked download path. The server enforces
/// the same bound so a client cannot ask it to stream an unbounded file.
pub const MAX_DOWNLOAD_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Largest chunk a single request may return. The client asks for much smaller
/// chunks; this only bounds what one response can allocate.
pub const MAX_DOWNLOAD_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Downloads `path` from the remote worktree into `destination_path`.
///
/// The bytes are staged in a sibling temporary file and renamed into place only
/// after the whole file arrived, so a failed or cancelled download never
/// truncates an existing destination file.
pub(crate) async fn download_in_chunks(
    client: &AnyProtoClient,
    worktree_id: WorktreeId,
    path: &Arc<RelPath>,
    destination_path: &Path,
    progress: &TransferHandle,
    executor: &BackgroundExecutor,
) -> Result<()> {
    let staging_path = staging_path(destination_path);
    let mut staging = StagingFile {
        path: staging_path.clone(),
        published: false,
    };
    pull_file_to_staging(
        client,
        worktree_id,
        path,
        &staging_path,
        progress,
        executor,
    )
    .await?;
    publish_staged_file(&staging_path, destination_path).await?;
    staging.published = true;
    Ok(())
}

/// Removes the partial staging file when a download fails or its task is
/// dropped, so a cancelled download cannot leave a hidden file behind.
struct StagingFile {
    path: PathBuf,
    published: bool,
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!(
                "failed to remove partial download {:?}: {error}",
                self.path
            ),
        }
    }
}

async fn pull_file_to_staging(
    client: &AnyProtoClient,
    worktree_id: WorktreeId,
    path: &Arc<RelPath>,
    staging_path: &Path,
    progress: &TransferHandle,
    executor: &BackgroundExecutor,
) -> Result<()> {
    if let Some(parent) = staging_path.parent() {
        smol::fs::create_dir_all(parent)
            .await
            .with_context(|| i18n::t_args!("82f83796064558c7", parent.display()))?;
    }
    let path_str = path.as_unix_str().to_owned();
    let mut file = smol::fs::File::create(staging_path)
        .await
        .with_context(|| i18n::t_args!("ca501ea4078e56a6", staging_path.display()))?;

    let invalid_data = i18n::t!("0973a57b0db888f7");
    let mut pacer = TransferPacer::new();
    let mut offset = 0u64;
    let mut expected_size = 0u64;
    let mut expected_mtime = None;

    loop {
        let plan = pacer.plan_next_chunk(network_pacing::interactive_activity_age());
        let started = Instant::now();
        let response = client
            .request(proto::DownloadFileChunk {
                worktree_id: worktree_id.to_proto(),
                path: path_str.clone(),
                offset,
                expected_size,
                expected_mtime,
                max_bytes: plan.chunk_size.min(u32::MAX as usize) as u32,
            })
            .await?;
        pacer.observe_round_trip(started.elapsed());

        let proto_file = response.file.as_ref().context(invalid_data)?;
        anyhow::ensure!(
            proto_file.worktree_id == worktree_id.to_proto() && proto_file.path == path_str,
            invalid_data
        );
        let content = response.content;
        anyhow::ensure!(content.len() <= plan.chunk_size, invalid_data);

        if offset == 0 {
            anyhow::ensure!(
                response.total_size <= MAX_DOWNLOAD_FILE_BYTES,
                i18n::t!("6b0acd8bbf4d83b5")
            );
            expected_size = response.total_size;
            expected_mtime = proto_file.mtime;
            progress.progress(FileTransferProgress::Bytes(0, expected_size));
        } else {
            anyhow::ensure!(
                response.total_size == expected_size && proto_file.mtime == expected_mtime,
                i18n::t!("1d731895289f313b")
            );
        }

        if content.is_empty() {
            // An empty chunk is only valid once the server has sent every byte,
            // which is also how a zero-byte file completes.
            anyhow::ensure!(offset == expected_size, invalid_data);
            break;
        }
        anyhow::ensure!(offset + content.len() as u64 <= expected_size, invalid_data);

        file.write_all(&content)
            .await
            .with_context(|| i18n::t_args!("ca501ea4078e56a6", staging_path.display()))?;
        offset += content.len() as u64;
        progress.progress(FileTransferProgress::Bytes(offset, expected_size));

        if offset == expected_size {
            break;
        }
        if !plan.delay.is_zero() {
            executor.timer(plan.delay).await;
        }
    }

    file.flush()
        .await
        .with_context(|| i18n::t_args!("ca501ea4078e56a6", staging_path.display()))?;
    // Close explicitly so the destination rename cannot race an open handle on
    // platforms that refuse to rename a file that is still open.
    file.close()
        .await
        .with_context(|| i18n::t_args!("ca501ea4078e56a6", staging_path.display()))?;
    Ok(())
}

/// A hidden sibling of the destination, so the final rename stays on the same
/// filesystem and cannot lose the previous file when the download fails.
fn staging_path(destination: &Path) -> PathBuf {
    let file_name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    destination.with_file_name(format!(
        ".{file_name}.zed-download-{:016x}",
        rand::random::<u64>()
    ))
}

async fn publish_staged_file(staging_path: &Path, destination_path: &Path) -> Result<()> {
    if let Some(parent) = destination_path.parent() {
        smol::fs::create_dir_all(parent)
            .await
            .with_context(|| i18n::t_args!("82f83796064558c7", parent.display()))?;
    }
    smol::fs::rename(staging_path, destination_path)
        .await
        .with_context(|| i18n::t_args!("ca501ea4078e56a6", destination_path.display()))
}
