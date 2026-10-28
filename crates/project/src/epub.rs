use std::{io::Read as _, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::BackgroundExecutor;
use rpc::proto;

pub const EPUB_ENTRY_LIMIT: u64 = 16 * 1024 * 1024;
pub const EPUB_CHUNK_SIZE: usize = 256 * 1024;
pub const EPUB_FILE_LIMIT: u64 = 512 * 1024 * 1024;

/// Office documents are ZIP containers as well, but the parts worth reading are
/// much larger than an EPUB chapter, so they use bigger chunks and a tighter
/// whole-file budget. They are still read entry by entry, never as whole files.
pub const OFFICE_ENTRY_LIMIT: u64 = 64 * 1024 * 1024;
pub const OFFICE_CHUNK_SIZE: usize = 1024 * 1024;
pub const OFFICE_FILE_LIMIT: u64 = 256 * 1024 * 1024;

/// Which kind of ZIP container an entry request addresses. Only the limits and
/// a few diagnostic messages differ; the reading path is shared.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Epub,
    Office,
}

impl ArchiveKind {
    fn changed_message(self) -> &'static str {
        match self {
            Self::Epub => i18n::t!("289cab9d66de19ce"),
            Self::Office => i18n::t!("d757df97219fc135"),
        }
    }

    fn oversize_message(self) -> &'static str {
        match self {
            Self::Epub => i18n::t!("f5223a233593a5e1"),
            Self::Office => i18n::t!("11275775e3eaa2cc"),
        }
    }
}

struct ArchiveSpec {
    kind: ArchiveKind,
    extensions: &'static [&'static str],
    chunk_size: usize,
    entry_limit: u64,
    file_limit: u64,
    not_found: &'static str,
    not_a_file: &'static str,
    unsupported_file: &'static str,
    invalid_entry_path: &'static str,
    too_many_entries: &'static str,
    truncated_entry: &'static str,
    removed_while_reading: &'static str,
}

const EPUB_SPEC: ArchiveSpec = ArchiveSpec {
    kind: ArchiveKind::Epub,
    extensions: &["epub"],
    chunk_size: EPUB_CHUNK_SIZE,
    entry_limit: EPUB_ENTRY_LIMIT,
    file_limit: EPUB_FILE_LIMIT,
    not_found: "EPUB not found",
    not_a_file: "not an EPUB file",
    unsupported_file: "not a supported EPUB file",
    invalid_entry_path: "invalid EPUB entry path",
    too_many_entries: "too many EPUB entries",
    truncated_entry: "truncated EPUB entry",
    removed_while_reading: "EPUB removed while reading",
};

const OFFICE_SPEC: ArchiveSpec = ArchiveSpec {
    kind: ArchiveKind::Office,
    extensions: &["docx", "docm", "dotx", "dotm"],
    chunk_size: OFFICE_CHUNK_SIZE,
    entry_limit: OFFICE_ENTRY_LIMIT,
    file_limit: OFFICE_FILE_LIMIT,
    not_found: "office document not found",
    not_a_file: "not an office document",
    unsupported_file: "not a supported office document",
    invalid_entry_path: "invalid office document entry path",
    too_many_entries: "too many office document entries",
    truncated_entry: "truncated office document entry",
    removed_while_reading: "office document removed while reading",
};

pub async fn read_entry_chunk(
    fs: Arc<dyn Fs>,
    abs_path: PathBuf,
    request: proto::ReadEpubEntry,
    executor: BackgroundExecutor,
) -> Result<proto::ReadEpubEntryResponse> {
    read_archive_entry_chunk(fs, abs_path, request, executor, &EPUB_SPEC).await
}

/// Reads one chunk of a single entry of a Word/OOXML container.
///
/// The request reuses the EPUB entry message because the wire shape is
/// identical: the server extracts the requested range of one archive entry and
/// the client parses it. Only the accepted extensions and the size limits are
/// office-specific.
pub async fn read_office_entry_chunk(
    fs: Arc<dyn Fs>,
    abs_path: PathBuf,
    request: proto::ReadEpubEntry,
    executor: BackgroundExecutor,
) -> Result<proto::ReadEpubEntryResponse> {
    read_archive_entry_chunk(fs, abs_path, request, executor, &OFFICE_SPEC).await
}

async fn read_archive_entry_chunk(
    fs: Arc<dyn Fs>,
    abs_path: PathBuf,
    request: proto::ReadEpubEntry,
    executor: BackgroundExecutor,
    spec: &'static ArchiveSpec,
) -> Result<proto::ReadEpubEntryResponse> {
    anyhow::ensure!(
        abs_path.extension().is_some_and(|extension| spec
            .extensions
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))),
        "{}",
        spec.not_a_file
    );
    let metadata = fs.metadata(&abs_path).await?.context(spec.not_found)?;
    anyhow::ensure!(
        !metadata.is_dir && !metadata.is_fifo && metadata.len <= spec.file_limit,
        "{}",
        spec.unsupported_file
    );
    let mtime: proto::Timestamp = metadata.mtime.into();
    if request.expected_mtime.is_some() {
        anyhow::ensure!(
            request.expected_size == metadata.len && request.expected_mtime == Some(mtime),
            spec.kind.changed_message()
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
            "{}",
            spec.invalid_entry_path
        );
        let handle = fs.open_sync(&abs_path).await?;
        let entry_path = request.entry_path.clone();
        let offset = request.offset;
        executor
            .spawn(async move {
                let mut archive = zip::ZipArchive::new(handle)?;
                anyhow::ensure!(archive.len() <= 100_000, "{}", spec.too_many_entries);
                let mut entry = archive.by_name(&entry_path)?;
                let entry_size = entry.size();
                anyhow::ensure!(
                    !entry.is_dir() && entry_size <= spec.entry_limit && offset <= entry_size,
                    spec.kind.oversize_message()
                );
                let skipped =
                    std::io::copy(&mut entry.by_ref().take(offset), &mut std::io::sink())?;
                anyhow::ensure!(skipped == offset, "{}", spec.truncated_entry);
                let length = (entry_size - offset).min(spec.chunk_size as u64) as usize;
                let mut content = vec![0; length];
                entry.read_exact(&mut content)?;
                Ok::<_, anyhow::Error>((content, entry_size))
            })
            .await?
    };
    let after = fs
        .metadata(&abs_path)
        .await?
        .context(spec.removed_while_reading)?;
    anyhow::ensure!(
        after.len == metadata.len && after.mtime == metadata.mtime,
        spec.kind.changed_message()
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

    fn office_fixture(size: usize) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("word/document.xml", zip::write::FileOptions::default())
            .expect("entry");
        archive.write_all(&vec![b'x'; size]).expect("write");
        archive.finish().expect("finish").into_inner()
    }

    #[gpui::test]
    async fn office_entries_use_their_own_chunk_size_and_extensions(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_file("/report.docx", office_fixture(OFFICE_CHUNK_SIZE + 7))
            .await;
        let request = proto::ReadEpubEntry {
            path: "report.docx".into(),
            entry_path: "word/document.xml".into(),
            ..Default::default()
        };
        let first = read_office_entry_chunk(
            fs.clone(),
            "/report.docx".into(),
            request.clone(),
            cx.background_executor.clone(),
        )
        .await
        .expect("first office chunk");
        assert_eq!(first.content.len(), OFFICE_CHUNK_SIZE);
        assert_eq!(first.entry_size, OFFICE_CHUNK_SIZE as u64 + 7);
        let last = read_office_entry_chunk(
            fs.clone(),
            "/report.docx".into(),
            proto::ReadEpubEntry {
                offset: OFFICE_CHUNK_SIZE as u64,
                expected_size: first.total_size,
                expected_mtime: first.file.as_ref().and_then(|file| file.mtime),
                ..request.clone()
            },
            cx.background_executor.clone(),
        )
        .await
        .expect("last office chunk");
        assert_eq!(last.content.len(), 7);

        // Each reader accepts only its own container extensions, so an EPUB
        // path can never reach the office limits and vice versa.
        assert!(
            read_office_entry_chunk(
                fs.clone(),
                "/report.docx".into(),
                proto::ReadEpubEntry {
                    path: "report.docx".into(),
                    entry_path: "../word/document.xml".into(),
                    ..Default::default()
                },
                cx.background_executor.clone(),
            )
            .await
            .is_err()
        );
        fs.insert_file("/book.epub", fixture(3)).await;
        assert!(
            read_office_entry_chunk(
                fs.clone(),
                "/book.epub".into(),
                proto::ReadEpubEntry {
                    path: "book.epub".into(),
                    entry_path: "chapter.xhtml".into(),
                    ..Default::default()
                },
                cx.background_executor.clone(),
            )
            .await
            .is_err()
        );
        assert!(
            read_entry_chunk(
                fs,
                "/report.docx".into(),
                request,
                cx.background_executor.clone()
            )
            .await
            .is_err()
        );
    }
}
