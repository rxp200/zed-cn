//! Importing git worktrees into the project that is already open.
//!
//! Zed's create-worktree flow used to open a new workspace for every linked
//! worktree. That makes the worktree's files and git status invisible from the
//! window the user is actually working in. This module adds the ability to
//! attach those roots to the current project instead, and to discover
//! worktrees that were created outside Zed while the project stays open.

use std::path::{Path, PathBuf};
use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{App, AsyncApp, Context, Entity, TaskExt, WeakEntity, Window};
use project::project_settings::ProjectSettings;
use project::trusted_worktrees::{PathTrust, TrustedWorktrees};
use project::{Project, Worktree};
use settings::Settings;
use workspace::Workspace;

/// A single discovery pass will import at most this many worktrees, so a
/// repository with many historical worktrees cannot multiply file scans and
/// language servers without bound.
const MAX_AUTO_IMPORTS_PER_PASS: usize = 8;
const INITIAL_DISCOVERY_DELAY: Duration = Duration::from_secs(3);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(15);
/// Consecutive failed passes back off up to this delay.
///
/// Without a cap a repository whose listing keeps failing is re-requested every
/// `DISCOVERY_INTERVAL` for as long as the project stays open, which turns one
/// broken repository into a permanent RPC loop.
const MAX_DISCOVERY_BACKOFF: Duration = Duration::from_secs(300);

/// Finds the project root whose absolute path is exactly `path`.
///
/// `Project::find_worktree` returns the containing root for a path inside an
/// already-open checkout, which would misidentify a worktree nested inside the
/// main worktree as an already-open root.
pub(crate) fn find_project_root(
    project: &Project,
    path: &Path,
    cx: &App,
) -> Option<Entity<Worktree>> {
    project
        .worktrees(cx)
        .find(|worktree| worktree.read(cx).abs_path().as_ref() == path)
}

/// Returns whether newly imported worktrees should inherit trust from the
/// project that is already open. Must be evaluated before the new roots are
/// added, because the imported roots themselves count as restricted until the
/// trust store is updated.
pub fn should_propagate_trust(project: &Entity<Project>, cx: &App) -> bool {
    if ProjectSettings::get_global(cx).session.trust_all_worktrees {
        return false;
    }
    let worktree_store = project.read(cx).worktree_store();
    !TrustedWorktrees::has_restricted_worktrees(&worktree_store, cx)
}

/// Adds `paths` to `project` as visible worktree roots.
///
/// Paths that are already open are skipped, so this is safe to call from both
/// the create-worktree flow and the discovery loop. Returns the paths that were
/// actually added.
pub async fn import_worktrees(
    project: Entity<Project>,
    paths: Vec<PathBuf>,
    propagate_trust: bool,
    cx: &mut AsyncApp,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut imported = Vec::new();
    for path in paths {
        let already_open = cx.update(|cx| find_project_root(project.read(cx), &path, cx).is_some());
        if already_open {
            continue;
        }

        let task = project.update(cx, |project, cx| {
            project.find_or_create_worktree(&path, true, cx)
        });
        task.await?;
        imported.push(path);
    }

    if imported.is_empty() {
        return Ok(imported);
    }

    // Wait for the new roots to be scanned into the git store so the worktree
    // appears in the Git panel's switcher instead of only in the file tree.
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    // Make the imported worktree the git panel's target.
    let first_imported = imported[0].clone();
    project.update(cx, |project, cx| {
        let Some(worktree) = find_project_root(project, &first_imported, cx) else {
            return;
        };
        let worktree_id = worktree.read(cx).id();
        project.git_store().update(cx, |git_store, cx| {
            git_store.set_active_repo_for_worktree(worktree_id, cx);
        });
    });

    if propagate_trust {
        trust_imported_worktrees(&project, &imported, cx);
    }

    Ok(imported)
}

fn trust_imported_worktrees(project: &Entity<Project>, paths: &[PathBuf], cx: &mut AsyncApp) {
    project.update(cx, |project, cx| {
        let worktree_store = project.worktree_store();
        let paths_to_trust: HashSet<PathTrust> = paths
            .iter()
            .filter_map(|path| {
                let worktree = find_project_root(project, path, cx)?;
                Some(PathTrust::Worktree(worktree.read(cx).id()))
            })
            .collect();
        if paths_to_trust.is_empty() {
            return;
        }
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.trust(&worktree_store, paths_to_trust, cx);
            });
        }
    });
}

/// Imports one existing worktree on an explicit user action.
pub fn handle_import_worktree(
    workspace: &mut Workspace,
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let propagate_trust = should_propagate_trust(&project, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let imported = import_worktrees(project, vec![path], propagate_trust, cx).await?;
        if !imported.is_empty() {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.show_worktree_trust_security_modal(false, window, cx);
            })?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Starts a bounded background loop that imports linked worktrees created
/// outside Zed while the project is open, and drops the roots of worktrees that
/// have since been removed.
///
/// The first pass only records the worktrees that already exist, so opening a
/// long-lived repository does not import every historical worktree. Later
/// passes import additions and reconcile removals.
pub fn start_worktree_discovery(workspace: &mut Workspace, cx: &mut App) {
    let workspace_handle = workspace.weak_handle();
    let project = workspace.project().clone();

    {
        let project_ref = project.read(cx);
        if project_ref.fs().is_fake() || project_ref.is_via_collab() || project_ref.is_read_only(cx)
        {
            return;
        }
    }

    cx.spawn(async move |cx| {
        discovery_loop(workspace_handle, cx).await;
    })
    .detach();
}

/// Removes the open project root that stands for the worktree at `path`.
///
/// Only callers that know the checkout itself is gone should use this; the
/// project's own "remove from project" action also uses `Project::remove_worktree`,
/// so the root's buffers, language servers and Git repository are released the
/// same way.
pub fn remove_worktree_root(project: &Entity<Project>, path: &Path, cx: &mut App) -> bool {
    let Some(worktree) = find_project_root(project.read(cx), path, cx) else {
        return false;
    };
    let worktree_id = worktree.read(cx).id();
    project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
    true
}

/// Delay before the next discovery pass.
///
/// A pass that could not list every live repository backs off exponentially, so
/// a listing that keeps failing stops being a 15-second retry. A pass that lists
/// everything goes straight back to the normal interval.
fn discovery_delay(failure_streak: u32) -> Duration {
    let shift = failure_streak.saturating_sub(1).min(5);
    DISCOVERY_INTERVAL
        .saturating_mul(1u32 << shift)
        .min(MAX_DISCOVERY_BACKOFF)
}

async fn discovery_loop(workspace: WeakEntity<Workspace>, cx: &mut AsyncApp) {
    let mut known_paths: HashSet<PathBuf> = HashSet::default();
    let mut tracked_paths: HashSet<PathBuf> = HashSet::default();
    let mut first_pass = true;
    let mut failure_streak = 0u32;
    let mut reported_stale_repositories = 0usize;

    loop {
        let delay = if first_pass && failure_streak == 0 {
            INITIAL_DISCOVERY_DELAY
        } else {
            discovery_delay(failure_streak)
        };
        cx.background_executor().timer(delay).await;

        let Some((project, repositories, mut project_roots)) = workspace
            .update(cx, |workspace, cx| {
                let project = workspace.project().clone();
                if project.read(cx).is_via_collab() {
                    return None;
                }
                let repositories = project
                    .read(cx)
                    .repositories(cx)
                    .values()
                    .map(|repository| {
                        let repo = repository.read(cx);
                        let root = repo.work_directory_abs_path.to_path_buf();
                        (
                            repository.clone(),
                            root,
                            repo.linked_worktree_path().is_some(),
                        )
                    })
                    .collect::<Vec<_>>();
                let project_roots = project
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect::<HashSet<_>>();
                Some((project, repositories, project_roots))
            })
            .ok()
            .flatten()
        else {
            break;
        };

        // A stale checkout makes `worktrees()` fail every time, which would
        // also block reconciliation of other linked roots.
        let mut checkout_status = HashMap::default();
        let mut live_repositories = Vec::with_capacity(repositories.len());
        let mut stale_repositories = 0;
        let mut stale_linked_roots = Vec::new();
        let mut listing_failed = false;
        for (repository, root, is_linked) in repositories {
            match check_checkout(&project, &root, &mut checkout_status, cx).await {
                CheckoutStatus::Present => live_repositories.push(repository),
                CheckoutStatus::Missing => {
                    stale_repositories += 1;
                    if is_linked {
                        stale_linked_roots.push(root);
                    }
                }
                CheckoutStatus::Unknown => listing_failed = true,
            }
        }

        if !stale_linked_roots.is_empty() {
            cx.update(|cx| {
                for root in &stale_linked_roots {
                    if remove_worktree_root(&project, root, cx) {
                        project_roots.remove(root);
                    }
                    tracked_paths.remove(root);
                }
            });
        }
        if stale_repositories > 0 && stale_repositories != reported_stale_repositories {
            log::info!(
                "ignoring {stale_repositories} git repository handle(s) whose checkout no longer exists"
            );
        }
        reported_stale_repositories = stale_repositories;

        let requests = cx.update(|cx| {
            live_repositories
                .iter()
                .map(|repository| repository.update(cx, |repository, _| repository.worktrees()))
                .collect::<Vec<_>>()
        });

        let mut linked_paths = HashSet::default();
        for request in requests {
            match request.await {
                Ok(Ok(worktrees)) => {
                    for worktree in worktrees {
                        if !worktree.is_bare && !worktree.is_main {
                            linked_paths.insert(worktree.path);
                        }
                    }
                }
                Ok(Err(error)) => {
                    log::debug!("could not list git worktrees: {error:#}");
                    listing_failed = true;
                }
                Err(_) => {
                    listing_failed = true;
                }
            }
        }

        // A partial listing cannot tell us that a previously known path is gone.
        if listing_failed {
            failure_streak = failure_streak.saturating_add(1);
            continue;
        }

        let mut missing_paths = HashSet::default();
        let mut metadata_failed = false;
        for path in linked_paths.union(&tracked_paths) {
            match check_checkout(&project, path, &mut checkout_status, cx).await {
                CheckoutStatus::Missing => {
                    missing_paths.insert(path.clone());
                }
                CheckoutStatus::Unknown => metadata_failed = true,
                CheckoutStatus::Present => {}
            }
        }
        if metadata_failed {
            failure_streak = failure_streak.saturating_add(1);
        } else {
            failure_streak = 0;
        }

        // Remember which open roots stand for linked worktrees, so a root can
        // be dropped once its checkout is gone even though Git has already
        // stopped listing it.
        tracked_paths.extend(
            linked_paths
                .iter()
                .filter(|path| project_roots.contains(*path))
                .cloned(),
        );
        tracked_paths.retain(|path| project_roots.contains(path));

        if !first_pass {
            let departed_paths = select_departed_worktree_roots(&tracked_paths, &missing_paths);
            if !departed_paths.is_empty() {
                let mut removed = 0;
                cx.update(|cx| {
                    for path in &departed_paths {
                        if remove_worktree_root(&project, path, cx) {
                            removed += 1;
                        }
                        tracked_paths.remove(path);
                    }
                });
                if removed > 0 {
                    log::info!(
                        "removed {removed} departed git worktree root(s) from the current project"
                    );
                }
            }
        }

        let linked_paths = linked_paths
            .into_iter()
            .filter(|path| !missing_paths.contains(path))
            .collect();
        let mut candidates =
            select_new_worktree_paths(&mut known_paths, &project_roots, linked_paths, first_pass);
        first_pass = false;
        candidates.retain(|path| checkout_status.get(path) == Some(&CheckoutStatus::Present));

        if candidates.is_empty() {
            continue;
        }

        candidates.sort();
        candidates.truncate(MAX_AUTO_IMPORTS_PER_PASS);
        let propagate_trust = cx.update(|cx| should_propagate_trust(&project, cx));

        match import_worktrees(project, candidates, propagate_trust, cx).await {
            Ok(imported) if !imported.is_empty() => {
                tracked_paths.extend(imported.iter().cloned());
                log::info!(
                    "imported {} git worktree(s) into the current project",
                    imported.len()
                );
            }
            Ok(_) => {}
            Err(error) => {
                log::warn!("failed to import discovered git worktrees: {error:#}");
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckoutStatus {
    Present,
    Missing,
    Unknown,
}

async fn check_checkout(
    project: &Entity<Project>,
    path: &Path,
    checkout_status: &mut HashMap<PathBuf, CheckoutStatus>,
    cx: &AsyncApp,
) -> CheckoutStatus {
    if let Some(status) = checkout_status.get(path) {
        return *status;
    }
    let status = match project
        .read_with(cx, |project, cx| {
            project.path_exists_on_project_host(path, cx)
        })
        .await
    {
        Ok(true) => CheckoutStatus::Present,
        Ok(false) => CheckoutStatus::Missing,
        Err(error) => {
            log::debug!(
                "could not check git worktree '{}': {error:#}",
                path.display()
            );
            CheckoutStatus::Unknown
        }
    };
    checkout_status.insert(path.to_owned(), status);
    status
}

/// Chooses the paths to import from one `git worktree list` response.
///
/// The first pass only seeds `known_paths`, so opening an old repository does
/// not import every historical worktree. Later passes return additions only.
fn select_new_worktree_paths(
    known_paths: &mut HashSet<PathBuf>,
    project_roots: &HashSet<PathBuf>,
    available_paths: HashSet<PathBuf>,
    first_pass: bool,
) -> Vec<PathBuf> {
    // Drop paths no longer available so reconciling a deleted checkout does
    // not prevent importing a new checkout created at the same path.
    known_paths.retain(|path| available_paths.contains(path));

    if first_pass {
        known_paths.extend(available_paths);
        return Vec::new();
    }

    let candidates = available_paths
        .iter()
        .filter(|path| !known_paths.contains(*path) && !project_roots.contains(*path))
        .cloned()
        .collect::<Vec<_>>();

    // Keep newly discovered paths out of `known_paths` until they are imported.
    // If the caller truncates this batch, the remaining candidates stay
    // eligible for the next pass instead of silently disappearing.
    known_paths.extend(
        available_paths
            .iter()
            .filter(|path| !candidates.contains(*path))
            .cloned(),
    );
    candidates
}

/// Returns the tracked roots whose checkout no longer exists on disk.
///
/// Only roots that were open in the project while Git listed them as linked
/// worktrees are tracked, so a folder the user opened or added themselves is
/// never removed, and a worktree that Git stops listing while its directory
/// still exists is left in place.
fn select_departed_worktree_roots(
    tracked_paths: &HashSet<PathBuf>,
    missing_paths: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    tracked_paths.intersection(missing_paths).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs as _};
    use gpui::TestAppContext;
    use project::project_settings::ProjectSettings;
    use project::{Project, WorktreeSettings};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            ProjectSettings::register(cx);
            WorktreeSettings::register(cx);
        });
    }

    fn paths(values: &[&str]) -> HashSet<PathBuf> {
        values.iter().map(PathBuf::from).collect()
    }

    fn is_open_root(project: &Entity<Project>, path: &Path, cx: &mut TestAppContext) -> bool {
        project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == path)
        })
    }

    #[test]
    fn worktree_recreated_at_the_same_path_is_imported_again() {
        let mut known = paths(&["/main", "/old"]);
        let roots = paths(&["/main"]);
        // `git worktree remove` makes the path disappear from the listing.
        assert!(select_new_worktree_paths(&mut known, &roots, paths(&["/main"]), false).is_empty());
        // Re-creating a worktree at the same path must be treated as a new one.
        let candidates =
            select_new_worktree_paths(&mut known, &roots, paths(&["/main", "/old"]), false);
        assert_eq!(candidates, vec![PathBuf::from("/old")]);
    }

    #[gpui::test]
    async fn removing_a_git_worktree_drops_its_imported_root(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    ".git": {},
                    "file.txt": "buffer_text",
                },
                "worktrees": {},
            }),
        )
        .await;
        fs.set_head_for_repo(
            path!("/root/project/.git").as_ref(),
            &[("file.txt", "buffer_text".to_string())],
            "deadbeef",
        );

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project.repositories(cx).values().next().unwrap().clone()
        });
        let worktree_path = PathBuf::from(path!("/root/worktrees/wt"));

        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.create_worktree(
                    git::repository::CreateWorktreeTarget::NewBranch {
                        branch_name: "wt".to_string(),
                        base_sha: Some("deadbeef".to_string()),
                    },
                    worktree_path.clone(),
                )
            })
        })
        .await
        .unwrap()
        .unwrap();
        cx.run_until_parked();

        import_worktrees(
            project.clone(),
            vec![worktree_path.clone()],
            false,
            &mut cx.to_async(),
        )
        .await
        .unwrap();
        cx.run_until_parked();
        assert!(
            is_open_root(&project, &worktree_path, cx),
            "importing a worktree should add its root"
        );

        cx.update(|cx| {
            repository.update(cx, |repository, _| {
                repository.remove_worktree(worktree_path.clone(), false)
            })
        })
        .await
        .unwrap()
        .unwrap();
        cx.run_until_parked();

        // Reconciling the removal is what one discovery pass does: the tracked
        // root's checkout is gone, so the root must go away too.
        let mut tracked_paths = HashSet::default();
        tracked_paths.insert(worktree_path.clone());
        let status = check_checkout(
            &project,
            &worktree_path,
            &mut HashMap::default(),
            &cx.to_async(),
        )
        .await;
        assert_eq!(status, CheckoutStatus::Missing);
        let missing_paths = HashSet::from_iter([worktree_path.clone()]);
        let departed_paths = select_departed_worktree_roots(&tracked_paths, &missing_paths);
        assert_eq!(departed_paths, vec![worktree_path.clone()]);

        cx.update(|cx| {
            for path in &departed_paths {
                assert!(remove_worktree_root(&project, path, cx));
            }
        });
        cx.run_until_parked();

        assert!(
            !is_open_root(&project, &worktree_path, cx),
            "a removed git worktree must not stay in the project's roots"
        );
    }

    #[gpui::test]
    async fn a_worktree_without_its_directory_is_no_longer_on_disk(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "wt": {} })).await;
        let project = Project::test(fs, [], cx).await;
        let mut checkout_status = HashMap::default();
        for (path, exists) in [
            (path!("/root/wt"), CheckoutStatus::Present),
            (path!("/root/gone"), CheckoutStatus::Missing),
        ] {
            assert_eq!(
                check_checkout(
                    &project,
                    path.as_ref(),
                    &mut checkout_status,
                    &cx.to_async()
                )
                .await,
                exists,
            );
        }
    }

    #[gpui::test]
    async fn remote_discovery_only_removes_confirmed_missing_linked_roots(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        use gpui::AppContext as _;
        use proto::{REMOTE_SERVER_PROJECT_ID, TypedEnvelope};
        use remote::RemoteClient;
        use workspace::MultiWorkspace;

        const MAIN: &str = path!("/remote/project");
        const LINKED: &str = path!("/remote/linked");
        const COMMON_DIR: &str = path!("/remote/project/.git");
        const LINKED_GIT_DIR: &str = path!("/remote/project/.git/worktrees/linked");

        init_test(cx);
        cx.update(|cx| {
            release_channel::init("0.0.0".parse().unwrap(), cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let client_fs = FakeFs::new(cx.executor());
        let local_project = Project::test(client_fs.clone(), [], cx).await;
        let remote_fs = FakeFs::new(server_cx.executor());
        remote_fs
            .insert_tree(
                Path::new(path!("/remote")),
                json!({ "project": {}, "linked": {} }),
            )
            .await;

        let (options, server, guard) = RemoteClient::fake_server(cx, server_cx);
        let metadata_allowed = server_cx.new(|_| true);
        server.add_request_handler(metadata_allowed.downgrade(), {
            let remote_fs = remote_fs.clone();
            move |allowed, request: TypedEnvelope<proto::GetPathMetadata>, cx| {
                let remote_fs = remote_fs.clone();
                async move {
                    anyhow::ensure!(
                        cx.read_entity(&allowed, |allowed, _| *allowed),
                        "metadata unavailable"
                    );
                    let path = request.payload.path;
                    let metadata = remote_fs.metadata(Path::new(&path)).await?;
                    Ok(proto::GetPathMetadataResponse {
                        exists: metadata.is_some(),
                        is_dir: metadata.is_some_and(|metadata| metadata.is_dir),
                        path,
                    })
                }
            }
        });
        server.add_request_handler(
            metadata_allowed.downgrade(),
            |_, _: TypedEnvelope<proto::Ping>, _| async { Ok(proto::Ack {}) },
        );
        server.add_request_handler(
            metadata_allowed.downgrade(),
            |_, _: TypedEnvelope<proto::RemoveWorktree>, _| async { Ok(proto::Ack {}) },
        );
        server.add_request_handler(
            metadata_allowed.downgrade(),
            |_, _: TypedEnvelope<proto::GitGetWorktrees>, _| async {
                Ok(proto::GitWorktreesResponse {
                    worktrees: vec![
                        proto::Worktree {
                            path: MAIN.into(),
                            is_main: true,
                            ..Default::default()
                        },
                        proto::Worktree {
                            path: LINKED.into(),
                            ..Default::default()
                        },
                    ],
                })
            },
        );
        drop(guard);
        let remote = RemoteClient::connect_mock(options, cx).await;
        let project = cx.update(|cx| {
            let local_project = local_project.read(cx);
            let client = local_project.client();
            let node = local_project.node_runtime().unwrap().clone();
            let user_store = local_project.user_store();
            let languages = local_project.languages().clone();
            Project::init(&client, cx);
            Project::remote(
                remote,
                client,
                node,
                user_store,
                languages,
                client_fs.clone(),
                false,
                cx,
            )
        });

        server
            .send(proto::UpdateProject {
                project_id: REMOTE_SERVER_PROJECT_ID,
                worktrees: [(1, MAIN, "project", false), (2, LINKED, "linked", true)]
                    .into_iter()
                    .map(
                        |(id, abs_path, root_name, is_linked)| proto::WorktreeMetadata {
                            id,
                            abs_path: abs_path.into(),
                            root_name: root_name.into(),
                            visible: true,
                            root_repo_common_dir: Some(COMMON_DIR.into()),
                            root_repo_is_linked_worktree: is_linked,
                        },
                    )
                    .collect(),
            })
            .unwrap();
        let announce_repository = |id, path: &str, git_dir: &str| {
            server
                .send(proto::UpdateRepository {
                    project_id: REMOTE_SERVER_PROJECT_ID,
                    id,
                    abs_path: path.into(),
                    repository_dir_abs_path: Some(git_dir.into()),
                    common_dir_abs_path: Some(COMMON_DIR.into()),
                    is_last_update: true,
                    ..Default::default()
                })
                .unwrap();
        };
        announce_repository(1, MAIN, COMMON_DIR);
        cx.run_until_parked();
        assert_eq!(
            project.read_with(cx, |project, cx| project.repositories(cx).len()),
            1
        );
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.workspace().downgrade()
            })
            .unwrap();
        let discovery = cx.spawn(|mut cx| async move { discovery_loop(workspace, &mut cx).await });
        cx.run_until_parked();

        assert!(client_fs.metadata(Path::new(MAIN)).await.unwrap().is_none());
        assert!(is_open_root(&project, Path::new(MAIN), cx));
        assert!(is_open_root(&project, Path::new(LINKED), cx));
        cx.executor().advance_clock(INITIAL_DISCOVERY_DELAY);
        cx.run_until_parked();
        assert!(
            is_open_root(&project, Path::new(MAIN), cx),
            "the main checkout exists on the remote host"
        );
        assert!(
            is_open_root(&project, Path::new(LINKED), cx),
            "the linked checkout exists on the remote host"
        );

        cx.executor().advance_clock(DISCOVERY_INTERVAL);
        cx.run_until_parked();
        assert!(
            is_open_root(&project, Path::new(LINKED), cx),
            "a linked root without its own repository handle must also be checked remotely"
        );
        announce_repository(2, LINKED, LINKED_GIT_DIR);
        remote_fs
            .remove_dir(Path::new(LINKED), Default::default())
            .await
            .unwrap();
        metadata_allowed.update(server_cx, |allowed, _| *allowed = false);
        cx.executor().advance_clock(DISCOVERY_INTERVAL);
        cx.run_until_parked();
        assert!(is_open_root(&project, Path::new(MAIN), cx));
        assert!(
            is_open_root(&project, Path::new(LINKED), cx),
            "metadata errors must not remove roots"
        );
        remote_fs
            .remove_dir(Path::new(MAIN), Default::default())
            .await
            .unwrap();
        metadata_allowed.update(server_cx, |allowed, _| *allowed = true);
        cx.executor().advance_clock(DISCOVERY_INTERVAL);
        cx.run_until_parked();
        assert!(
            is_open_root(&project, Path::new(MAIN), cx),
            "Git discovery must never detach the main checkout"
        );
        assert!(
            !is_open_root(&project, Path::new(LINKED), cx),
            "only the missing linked worktree is detached"
        );
        drop(discovery);
    }

    #[test]
    fn only_tracked_roots_can_depart() {
        let mut tracked_paths = HashSet::default();
        tracked_paths.insert(PathBuf::from("/imported"));
        let missing_paths = paths(&["/main", "/manual", "/imported"]);

        assert_eq!(
            select_departed_worktree_roots(&tracked_paths, &missing_paths),
            vec![PathBuf::from("/imported")]
        );
    }

    #[test]
    fn a_tracked_root_that_still_exists_is_kept() {
        let mut tracked_paths = HashSet::default();
        tracked_paths.insert(PathBuf::from("/wt"));
        let missing_paths = paths(&["/other"]);

        assert!(select_departed_worktree_roots(&tracked_paths, &missing_paths).is_empty());
    }

    #[test]
    fn first_pass_only_records_existing_worktrees() {
        let mut known = HashSet::default();
        let roots = paths(&["/main"]);
        let candidates =
            select_new_worktree_paths(&mut known, &roots, paths(&["/main", "/old"]), true);
        assert!(
            candidates.is_empty(),
            "historical worktrees stay unimported"
        );
        assert_eq!(known, paths(&["/main", "/old"]));
    }

    #[test]
    fn later_pass_imports_only_new_worktrees() {
        let mut known = paths(&["/main", "/old"]);
        let roots = paths(&["/main"]);
        let candidates =
            select_new_worktree_paths(&mut known, &roots, paths(&["/main", "/old", "/new"]), false);
        assert_eq!(candidates, vec![PathBuf::from("/new")]);
    }

    #[test]
    fn worktree_already_open_is_not_imported_again() {
        let mut known = HashSet::default();
        let roots = paths(&["/main", "/already-open"]);
        let candidates = select_new_worktree_paths(
            &mut known,
            &roots,
            paths(&["/main", "/already-open"]),
            false,
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn discovery_delay_backs_off_and_caps() {
        // A healthy pass keeps the normal cadence.
        assert_eq!(discovery_delay(0), DISCOVERY_INTERVAL);
        assert_eq!(discovery_delay(1), DISCOVERY_INTERVAL);
        // Consecutive failures double the wait.
        assert_eq!(discovery_delay(2), std::time::Duration::from_secs(30));
        assert_eq!(discovery_delay(3), std::time::Duration::from_secs(60));
        assert_eq!(discovery_delay(4), std::time::Duration::from_secs(120));
        assert_eq!(discovery_delay(5), std::time::Duration::from_secs(240));
        // ... and stop at the cap instead of growing without bound.
        assert_eq!(discovery_delay(6), MAX_DISCOVERY_BACKOFF);
        assert_eq!(discovery_delay(u32::MAX), MAX_DISCOVERY_BACKOFF);
    }
}
