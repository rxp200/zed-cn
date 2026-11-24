//! Recursive listing of worktree files for the client's "download from
//! remote" action.
//!
//! The worktree snapshot intentionally leaves ignored directories unloaded, so
//! enumerating a snapshot can never download `node_modules`, `target` or any
//! other directory Zed does not index. This walker reads the filesystem
//! directly on the server and pages through it, which keeps the client's
//! memory bounded while still covering every regular file.
//!
//! Pages are stateless: a request resumes from the last path it returned.
//! Depth-first order with name-sorted children matches a component-wise path
//! comparison, so resuming only has to list the directories along the cursor
//! path instead of replaying the whole tree.

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::stream::StreamExt as _;
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::rel_path::RelPath;
use worktree::WorktreeSettings;

/// Number of paths returned when a client does not ask for a specific size.
pub const DEFAULT_PAGE_FILES: usize = 1024;
/// Hard bound on one response, regardless of what the client requested.
pub const MAX_PAGE_FILES: usize = 8192;
/// Also bound a response by the total size of the returned path strings.
const MAX_PAGE_PATH_BYTES: usize = 512 * 1024;
/// A symlink cycle that escapes inode tracking still cannot recurse forever.
const MAX_DIRECTORY_DEPTH: usize = 128;

/// One page of a worktree file listing.
pub struct FileListPage {
    pub paths: Vec<Arc<RelPath>>,
    /// Whether the walker ran out of work instead of hitting a page bound.
    pub complete: bool,
}

struct ListingDirectory {
    rel: Arc<RelPath>,
    abs: PathBuf,
    /// Inodes of this directory and all of its listing ancestors, used to stop
    /// symlink cycles.
    ancestor_inodes: Arc<Vec<u64>>,
    depth: usize,
}

enum ListingItem {
    File(Arc<RelPath>),
    Directory(ListingDirectory),
}

struct ChildEntry {
    name: String,
    rel: Arc<RelPath>,
    abs: PathBuf,
    inode: u64,
    is_dir: bool,
}

/// Walks the filesystem below one worktree directory.
pub struct WorktreeFileWalker {
    fs: Arc<dyn Fs>,
    settings: WorktreeSettings,
    root_canonical: PathBuf,
    items: Vec<ListingItem>,
}

impl WorktreeFileWalker {
    pub async fn new(
        fs: Arc<dyn Fs>,
        settings: WorktreeSettings,
        root_abs: &Path,
        root_rel: Arc<RelPath>,
    ) -> Result<Self> {
        let metadata = fs
            .metadata(root_abs)
            .await
            .with_context(|| format!("failed to inspect {}", root_abs.display()))?
            .with_context(|| format!("{} does not exist", root_abs.display()))?;
        anyhow::ensure!(metadata.is_dir, "{} is not a directory", root_abs.display());

        let root_canonical = fs
            .canonicalize(root_abs)
            .await
            .with_context(|| format!("failed to resolve {}", root_abs.display()))?;

        Ok(Self {
            fs,
            settings,
            root_canonical,
            items: vec![ListingItem::Directory(ListingDirectory {
                rel: root_rel,
                abs: root_abs.to_owned(),
                ancestor_inodes: Arc::new(vec![metadata.inode]),
                depth: 0,
            })],
        })
    }

    /// Returns the files that follow `cursor` in depth-first order.
    ///
    /// The caller pages until `complete` is true, passing the last returned
    /// path as the next cursor.
    pub async fn next_page(
        &mut self,
        cursor: Option<&RelPath>,
        max_files: usize,
    ) -> Result<FileListPage> {
        let max_files = max_files.clamp(1, MAX_PAGE_FILES);
        let mut paths = Vec::new();
        let mut path_bytes = 0;

        while let Some(item) = self.items.pop() {
            match item {
                ListingItem::File(path) => {
                    if let Some(cursor) = cursor
                        && path.components().cmp(cursor.components()) != Ordering::Greater
                    {
                        continue;
                    }
                    if paths.len() >= max_files || path_bytes >= MAX_PAGE_PATH_BYTES {
                        self.items.push(ListingItem::File(path));
                        return Ok(FileListPage {
                            paths,
                            complete: false,
                        });
                    }
                    path_bytes += path.as_unix_str().len();
                    paths.push(path);
                }
                ListingItem::Directory(directory) => {
                    if let Some(cursor) = cursor {
                        // Directories that contain the cursor have to be entered
                        // to reach it; every other directory entirely before the
                        // cursor can be skipped with all of its children.
                        let is_ancestor =
                            directory.rel.is_empty() || cursor.starts_with(&directory.rel);
                        if !is_ancestor
                            && directory.rel.components().cmp(cursor.components())
                                != Ordering::Greater
                        {
                            continue;
                        }
                    }

                    let mut children = self.read_sorted_children(&directory).await?;
                    children.reverse();
                    for child in children {
                        if !child.is_dir {
                            self.items.push(ListingItem::File(child.rel));
                            continue;
                        }

                        let mut ancestor_inodes = directory.ancestor_inodes.as_ref().clone();
                        ancestor_inodes.push(child.inode);
                        self.items.push(ListingItem::Directory(ListingDirectory {
                            rel: child.rel,
                            abs: child.abs,
                            ancestor_inodes: Arc::new(ancestor_inodes),
                            depth: directory.depth + 1,
                        }));
                    }
                }
            }
        }

        Ok(FileListPage {
            paths,
            complete: true,
        })
    }

    async fn read_sorted_children(&self, directory: &ListingDirectory) -> Result<Vec<ChildEntry>> {
        let mut entries = Vec::new();
        let mut stream = self
            .fs
            .read_dir(&directory.abs)
            .await
            .with_context(|| format!("failed to list {}", directory.abs.display()))?;
        while let Some(entry) = stream.next().await {
            let abs = match entry {
                Ok(abs) => abs,
                Err(error) => {
                    log::warn!(
                        "skipping unreadable entry in {:?}: {error:#}",
                        directory.abs
                    );
                    continue;
                }
            };
            let Some(name) = abs.file_name().and_then(|name| name.to_str()) else {
                log::debug!("skipping non-UTF-8 file name in {:?}", directory.abs);
                continue;
            };
            let Ok(name_rel) = RelPath::from_unix_str(name) else {
                log::debug!("skipping unrepresentable file name {name:?}");
                continue;
            };
            let rel: Arc<RelPath> = Arc::from(directory.rel.join(name_rel));
            if self.settings.is_path_excluded(&rel) {
                continue;
            }

            let metadata = match self.fs.metadata(&abs).await {
                Ok(Some(metadata)) => metadata,
                Ok(None) => continue,
                Err(error) => {
                    log::warn!("skipping uninspectable entry {abs:?}: {error:#}");
                    continue;
                }
            };
            if metadata.is_fifo {
                continue;
            }

            if metadata.is_dir {
                if name == ".git" {
                    continue;
                }
                if metadata.is_symlink {
                    let canonical = match self.fs.canonicalize(&abs).await {
                        Ok(canonical) => canonical,
                        Err(error) => {
                            log::debug!("skipping unresolvable symlink {abs:?}: {error:#}");
                            continue;
                        }
                    };
                    // Following a link out of the worktree would download an
                    // unbounded amount of unrelated content.
                    if !canonical.starts_with(&self.root_canonical) {
                        log::debug!("skipping symlink outside the worktree: {abs:?}");
                        continue;
                    }
                    if directory.ancestor_inodes.contains(&metadata.inode) {
                        log::debug!("skipping recursive symlink {abs:?}");
                        continue;
                    }
                }
                if directory.depth + 1 > MAX_DIRECTORY_DEPTH {
                    log::warn!("skipping directory nested too deeply: {abs:?}");
                    continue;
                }
            } else if name == ".git" {
                // A worktree can carry a `.git` file pointing at its real git
                // directory; it is not project content either.
                continue;
            }

            entries.push(ChildEntry {
                name: name.to_owned(),
                rel,
                abs,
                inode: metadata.inode,
                is_dir: metadata.is_dir,
            });
        }

        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}
