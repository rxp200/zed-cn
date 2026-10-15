use std::{io::Read as _, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::BackgroundExecutor;
use rpc::proto;

pub const EPUB_ENTRY_LIMIT: u64 = 16 * 1024 * 1024;
pub const EPUB_CHUNK_SIZE: usize = 256 * 1024;

pub async fn read_entry_chunk(
    fs: Arc<dyn Fs>,
    abs_path: PathBuf,
    request: proto::ReadEpubEntry,
    executor: BackgroundExecutor,
) -> Result<proto::ReadEpubEntryResponse> {
    anyhow::ensure!(
        abs_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("epub")),
        "not an EPUB file"
    );
    let metadata = fs.metadata(&abs_path).await?.context("EPUB not found")?;
    anyhow::ensure!(
        !metadata.is_dir && !metadata.is_fifo && metadata.len <= 512 * 1024 * 1024,
        "not a supported EPUB file"
    );
    let mtime: proto::Timestamp = metadata.mtime.into();
    if request.expected_mtime.is_some() {
        anyhow::ensure!(
            request.expected_size == metadata.len && request.expected_mtime == Some(mtime),
            i18n::t!("289cab9d66de19ce")
        );
    }
    let (content, entry_size) = if request.entry_path.is_empty() {
        anyhow::ensure!(request.offset == 0, "invalid metadata offset");
        (Vec::new(), 0)
    } else {
        anyhow::ensure!(
            request.entry_path.len() <= 4096
                && !request.entry_path.starts_with('/')
                && !request.entry_path.contains('\\')
                && !request.entry_path.split('/').any(|part| part == ".."),
            "invalid EPUB entry path"
        );
        let handle = fs.open_sync(&abs_path).await?;
        let entry_path = request.entry_path.clone();
        let offset = request.offset;
        executor
            .spawn(async move {
                let mut archive = zip::ZipArchive::new(handle)?;
                anyhow::ensure!(archive.len() <= 100_000, "too many EPUB entries");
                let mut entry = archive.by_name(&entry_path)?;
                let entry_size = entry.size();
                anyhow::ensure!(
                    !entry.is_dir() && entry_size <= EPUB_ENTRY_LIMIT && offset <= entry_size,
                    i18n::t!("f5223a233593a5e1")
                );
                let skipped =
                    std::io::copy(&mut entry.by_ref().take(offset), &mut std::io::sink())?;
                anyhow::ensure!(skipped == offset, "truncated EPUB entry");
                let length = (entry_size - offset).min(EPUB_CHUNK_SIZE as u64) as usize;
                let mut content = vec![0; length];
                entry.read_exact(&mut content)?;
                Ok::<_, anyhow::Error>((content, entry_size))
            })
            .await?
    };
    let after = fs
        .metadata(&abs_path)
        .await?
        .context("EPUB removed while reading")?;
    anyhow::ensure!(
        after.len == metadata.len && after.mtime == metadata.mtime,
        i18n::t!("289cab9d66de19ce")
    );
    Ok(proto::ReadEpubEntryResponse {
        file: Some(proto::File {
            worktree_id: request.worktree_id,
            path: request.path,
            mtime: Some(mtime),
            ..Default::default()
        }),
        content,
        total_size: metadata.len,
        entry_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::io::{Cursor, Write as _};

    fn fixture(size: usize) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("chapter.xhtml", zip::write::FileOptions::default())
            .expect("entry");
        archive.write_all(&vec![b'x'; size]).expect("write");
        archive.finish().expect("finish").into_inner()
    }

    #[gpui::test]
    async fn entry_reads_are_bounded_and_validate_snapshots(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_file("/book.epub", fixture(EPUB_CHUNK_SIZE + 13))
            .await;
        let request = proto::ReadEpubEntry {
            path: "book.epub".into(),
            entry_path: "chapter.xhtml".into(),
            ..Default::default()
        };
        let first = read_entry_chunk(
            fs.clone(),
            "/book.epub".into(),
            request.clone(),
            cx.background_executor.clone(),
        )
        .await
        .expect("first chunk");
        assert_eq!(first.content.len(), EPUB_CHUNK_SIZE);
        assert_eq!(first.entry_size, EPUB_CHUNK_SIZE as u64 + 13);
        let next = proto::ReadEpubEntry {
            offset: EPUB_CHUNK_SIZE as u64,
            expected_size: first.total_size,
            expected_mtime: first.file.as_ref().and_then(|file| file.mtime),
            ..request.clone()
        };
        let last = read_entry_chunk(
            fs.clone(),
            "/book.epub".into(),
            next.clone(),
            cx.background_executor.clone(),
        )
        .await
        .expect("last chunk");
        assert_eq!(last.content.len(), 13);
        fs.insert_file("/book.epub", fixture(3)).await;
        assert!(
            read_entry_chunk(
                fs.clone(),
                "/book.epub".into(),
                next,
                cx.background_executor.clone()
            )
            .await
            .is_err()
        );
        for entry_path in ["../chapter.xhtml", "/chapter.xhtml", "missing"] {
            assert!(
                read_entry_chunk(
                    fs.clone(),
                    "/book.epub".into(),
                    proto::ReadEpubEntry {
                        entry_path: entry_path.into(),
                        ..request.clone()
                    },
                    cx.background_executor.clone()
                )
                .await
                .is_err()
            );
        }
    }

    #[gpui::test]
    async fn oversized_entries_are_rejected_before_allocation(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_file("/book.epub", fixture(EPUB_ENTRY_LIMIT as usize + 1))
            .await;
        let result = read_entry_chunk(
            fs,
            "/book.epub".into(),
            proto::ReadEpubEntry {
                entry_path: "chapter.xhtml".into(),
                ..Default::default()
            },
            cx.background_executor.clone(),
        )
        .await;
        assert!(result.is_err());
    }
}
