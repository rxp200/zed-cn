//! Importing git worktrees into the project that is already open.
//!
//! Zed's create-worktree flow used to open a new workspace for every linked
//! worktree. That makes the worktree's files and git status invisible from the
//! window the user is actually working in. This module adds the ability to
//! attach those roots to the current project instead, and to discover
//! worktrees that were created outside Zed while the project stays open.

use std::path::{Path, PathBuf};
use std::time::Duration;

use collections::HashSet;
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
/// outside Zed while the project is open.
///
/// The first pass only records the worktrees that already exist, so opening a
/// long-lived repository does not import every historical worktree. Later
/// passes import additions.
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

async fn discovery_loop(workspace: WeakEntity<Workspace>, cx: &mut AsyncApp) {
    let mut known_paths: HashSet<PathBuf> = HashSet::default();
    let mut first_pass = true;

    loop {
        let delay = if first_pass {
            INITIAL_DISCOVERY_DELAY
        } else {
            DISCOVERY_INTERVAL
        };
        cx.background_executor().timer(delay).await;

        let Some((project, repositories, project_roots)) = workspace
            .update(cx, |workspace, cx| {
                let project = workspace.project().clone();
                if project.read(cx).is_via_collab() {
                    return None;
                }
                let repositories = project
                    .read(cx)
                    .repositories(cx)
                    .values()
                    .cloned()
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

        let requests = cx.update(|cx| {
            repositories
                .iter()
                .map(|repository| repository.update(cx, |repository, _| repository.worktrees()))
                .collect::<Vec<_>>()
        });

        let mut listed_paths = HashSet::default();
        for request in requests {
            match request.await {
                Ok(Ok(worktrees)) => {
                    for worktree in worktrees {
                        if !worktree.is_bare {
                            listed_paths.insert(worktree.path);
                        }
                    }
                }
                Ok(Err(error)) => {
                    log::debug!("could not list git worktrees: {error:#}");
                }
                Err(_) => {}
            }
        }

        if listed_paths.is_empty() {
            continue;
        }

        let mut candidates =
            select_new_worktree_paths(&mut known_paths, &project_roots, listed_paths, first_pass);
        first_pass = false;

        if candidates.is_empty() {
            continue;
        }

        candidates.sort();
        candidates.truncate(MAX_AUTO_IMPORTS_PER_PASS);
        let propagate_trust = cx.update(|cx| should_propagate_trust(&project, cx));

        match import_worktrees(project, candidates, propagate_trust, cx).await {
            Ok(imported) if !imported.is_empty() => {
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

/// Chooses the paths to import from one `git worktree list` response.
///
/// The first pass only seeds `known_paths`, so opening an old repository does
/// not import every historical worktree. Later passes return additions only.
fn select_new_worktree_paths(
    known_paths: &mut HashSet<PathBuf>,
    project_roots: &HashSet<PathBuf>,
    listed_paths: HashSet<PathBuf>,
    first_pass: bool,
) -> Vec<PathBuf> {
    if first_pass {
        known_paths.extend(listed_paths);
        return Vec::new();
    }

    let candidates = listed_paths
        .iter()
        .filter(|path| !known_paths.contains(*path) && !project_roots.contains(*path))
        .cloned()
        .collect::<Vec<_>>();

    // Keep newly discovered paths out of `known_paths` until they are imported.
    // If the caller truncates this batch, the remaining candidates stay
    // eligible for the next pass instead of silently disappearing.
    known_paths.extend(
        listed_paths
            .iter()
            .filter(|path| !candidates.contains(*path))
            .cloned(),
    );
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(values: &[&str]) -> HashSet<PathBuf> {
        values.iter().map(PathBuf::from).collect()
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
}
