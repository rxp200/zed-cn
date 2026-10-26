use std::sync::{Arc, atomic};

use futures::channel::oneshot;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{App, AppContext, Entity, TestAppContext, VisualTestContext};
use picker::{Picker, PickerDelegate};
use project::Project;
use serde_json::json;
use util::path;
use workspace::{AppState, MultiWorkspace};

use crate::{CandidateInfo, CreateDirectoryHandler, DirectoryState, OpenPathDelegate};

#[gpui::test]
async fn test_open_path_prompt(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                ".a1": ".A1",
                ".b1": ".B1",
                "a1": "A1",
                "a2": "A2",
                "a3": "A3",
                "dir1": {},
                "dir2": {
                    "c": "C",
                    "d1": "D1",
                    "d2": "D2",
                    "d3": "D3",
                    "dir3": {},
                    "dir4": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    insert_query(path!("sadjaoislkdjasldj"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), Vec::<String>::new());

    let query = path!("/root");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["root"]);

    #[cfg(not(windows))]
    let expected_separator = "./";
    #[cfg(windows)]
    let expected_separator = ".\\";

    // If the query ends with a slash, the picker should show the contents of the directory and not show any of the hidden entries.
    let query = path!("/root/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![expected_separator, "dir1", "dir2", "a1", "a2", "a3"]
    );

    // Show candidates for the query "a".
    let query = path!("/root/a");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["a1", "a2", "a3"]
    );

    // Show candidates for the query "d".
    let query = path!("/root/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);

    let query = path!("/root/dir2");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir2"]);

    let query = path!("/root/dir2/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![expected_separator, "dir3", "dir4", "c", "d1", "d2", "d3"]
    );

    // Show candidates for the query "d".
    let query = path!("/root/dir2/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["dir3", "dir4", "d1", "d2", "d3"]
    );

    let query = path!("/root/dir2/di");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir3", "dir4"]);

    // Don't show candidates for the query ".".
    let query = path!("/root/.");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), Vec::<String>::new());

    // Don't show any candidates for the query ".a".
    let query = path!("/root/.a");
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), Vec::<String>::new());

    // Show candidates for the query "./".
    // Should show current directory and contents.
    let query = path!("/root/./");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![expected_separator, "dir1", "dir2", "a1", "a2", "a3"]
    );

    // Show candidates for the query "../". Show parent contents.
    let query = path!("/root/dir1/../");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![expected_separator, "dir1", "dir2", "a1", "a2", "a3"]
    );
}

#[gpui::test]
async fn test_open_path_prompt_completion(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a": "A",
                "dir1": {},
                "dir2": {
                    "c": "C",
                    "d": "D",
                    "dir3": {},
                    "dir4": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    // Confirm completion for the query "/root", since it's a directory, it should add a trailing slash.
    let query = path!("/root");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/")
    );

    // Confirm completion for the query "/root/", selecting the first candidate "dir1", since it's a directory, it should add a trailing slash.
    let query = path!("/root/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx),
        None,
        "First entry is `./` and when we confirm completion, it is tabbed below"
    );
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir1/"),
        "Second entry is the first directory"
    );

    // Confirm completion for the query "/root/", selecting the third candidate "a", since it's a file, it should not add a trailing slash.
    let query = path!("/root/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 3, &picker, cx).unwrap(),
        path!("/root/a")
    );

    let query = path!("/root/a");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/a")
    );

    let query = path!("/root/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir2/")
    );

    let query = path!("/root/dir2");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/dir2/")
    );

    let query = path!("/root/dir2/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir2/dir3/")
    );

    let query = path!("/root/dir2/");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 3, &picker, cx).unwrap(),
        path!("/root/dir2/c")
    );

    let query = path!("/root/dir2/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        path!("/root/dir2/dir3/")
    );

    let query = path!("/root/dir2/d");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir2/dir4/")
    );

    let query = path!("/root/dir2/di");
    insert_query(query, &picker, cx).await;
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        path!("/root/dir2/dir4/")
    );
}

#[gpui::test]
async fn test_enter_completes_open_path_before_confirming(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/"),
            json!({
                "code": {},
                "config": {}
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/").as_ref()], cx).await;
    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    insert_query(path!("/cod"), &picker, cx).await;
    assert_eq!(
        confirm_update_query(&picker, cx).as_deref(),
        Some(path!("/code/"))
    );

    insert_query(path!("/code/"), &picker, cx).await;
    assert_eq!(confirm_update_query(&picker, cx), None);
}

#[gpui::test]
#[cfg_attr(not(target_os = "windows"), ignore)]
async fn test_open_path_prompt_on_windows(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a": "A",
                "dir1": {},
                "dir2": {}
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    // Support both forward and backward slashes.
    let query = "C:/root/";
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![".\\", "dir1", "dir2", "a"]
    );
    assert_eq!(
        confirm_completion(query, 0, &picker, cx),
        None,
        "First entry is `.\\` and when we confirm completion, it is tabbed below"
    );
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        "C:/root/dir1\\",
        "Second entry is the first directory"
    );

    let query = "C:\\root/";
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![".\\", "dir1", "dir2", "a"]
    );
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        "C:\\root/dir1\\"
    );

    let query = "C:\\root\\";
    insert_query(query, &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![".\\", "dir1", "dir2", "a"]
    );
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        "C:\\root\\dir1\\"
    );

    // Confirm completion for the query "C:/root/d", selecting the second candidate "dir2", since it's a directory, it should add a trailing slash.
    let query = "C:/root/d";
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);
    assert_eq!(
        confirm_completion(query, 1, &picker, cx).unwrap(),
        "C:/root/dir2\\"
    );

    let query = "C:\\root/d";
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        "C:\\root/dir1\\"
    );

    let query = "C:\\root\\d";
    insert_query(query, &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1", "dir2"]);
    assert_eq!(
        confirm_completion(query, 0, &picker, cx).unwrap(),
        "C:\\root\\dir1\\"
    );
}

#[gpui::test]
async fn test_new_path_prompt(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a1": "A1",
                "a2": "A2",
                "a3": "A3",
                "dir1": {},
                "dir2": {
                    "c": "C",
                    "d1": "D1",
                    "d2": "D2",
                    "d3": "D3",
                    "dir3": {},
                    "dir4": {}
                }
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, true, false, cx);

    insert_query(path!("/root"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["root"]);

    insert_query(path!("/root/d"), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["d", "dir1", "dir2"]
    );

    insert_query(path!("/root/dir1"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1"]);

    insert_query(path!("/root/dir12"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir12"]);

    insert_query(path!("/root/dir1"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["dir1"]);
}

#[gpui::test]
async fn test_new_path_prompt_creates_missing_directory(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "dir1": {} }))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let create_fs = app_state.fs.clone();
    let create_directory: Arc<CreateDirectoryHandler> = Arc::new(move |path: String, cx: &App| {
        let fs = create_fs.clone();
        cx.spawn(async move |_| {
            let path = std::path::PathBuf::from(path);
            fs.create_dir(&path).await?;
            Ok(fs.canonicalize(&path).await?.to_string_lossy().into_owned())
        })
    });
    let (picker, mut confirmed, cx) = build_new_path_prompt(project, create_directory, cx);

    insert_query(path!("/root/new-project"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["new-project"]);

    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.confirm(false, window, cx)
    });
    cx.run_until_parked();

    let created = std::path::PathBuf::from(path!("/root/new-project"));
    assert!(
        app_state.fs.is_dir(&created).await,
        "confirming a typed, nonexistent name creates that directory"
    );
    assert_eq!(
        picker.read_with(cx, |picker, _| picker.delegate.current_query.clone()),
        path!("/root/new-project/").to_string(),
        "the first Enter descends into the created directory"
    );
    assert!(
        matches!(confirmed.try_recv(), Ok(None)),
        "creating a directory does not open it before the user is done"
    );
    assert!(
        picker.read_with(cx, |picker, _| picker.delegate.create_error.is_none()),
        "a successful creation reports no error"
    );

    // The created directory is now the listed parent and the `./` row is
    // selected, so a second Enter opens it.
    assert_eq!(
        picker.read_with(cx, |picker, _| picker.delegate.selected_index()),
        0
    );
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.confirm(false, window, cx)
    });
    cx.run_until_parked();
    let confirmed_path = confirmed
        .try_recv()
        .expect("confirmation is delivered")
        .expect("confirmation is not cancelled")
        .expect("a path is confirmed");
    assert_eq!(confirmed_path.len(), 1);
    assert_eq!(
        app_state
            .fs
            .canonicalize(&confirmed_path[0])
            .await
            .expect("the confirmed path exists"),
        app_state
            .fs
            .canonicalize(&created)
            .await
            .expect("the created path exists"),
        "the second Enter opens the directory created by the first"
    );
}

#[gpui::test]
async fn test_new_path_prompt_creates_each_segment_of_a_deep_path(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({}))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let create_fs = app_state.fs.clone();
    let create_directory: Arc<CreateDirectoryHandler> = Arc::new(move |path: String, cx: &App| {
        let fs = create_fs.clone();
        cx.spawn(async move |_| {
            let path = std::path::PathBuf::from(path);
            fs.create_dir(&path).await?;
            Ok(fs.canonicalize(&path).await?.to_string_lossy().into_owned())
        })
    });
    let (picker, _confirmed, cx) = build_new_path_prompt(project, create_directory, cx);

    let mut path = String::from("/root");
    for segment in ["one", "two", "three"] {
        path.push('/');
        path.push_str(segment);
        insert_query(&path, &picker, cx).await;
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        cx.run_until_parked();
    }

    assert!(
        app_state
            .fs
            .is_dir(std::path::Path::new(path!("/root/one/two/three")))
            .await,
        "every Enter creates and descends into one more path segment"
    );
    assert_eq!(
        picker.read_with(cx, |picker, _| picker.delegate.current_query.clone()),
        path!("/root/one/two/three/").to_string()
    );
}

#[gpui::test]
async fn test_new_path_prompt_reports_create_failure(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "dir1": {} }))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let create_directory: Arc<CreateDirectoryHandler> = Arc::new(move |path: String, cx: &App| {
        cx.spawn(async move |_| -> anyhow::Result<String> {
            anyhow::bail!("mkdir permission denied for {path}")
        })
    });
    let (picker, mut confirmed, cx) = build_new_path_prompt(project, create_directory, cx);

    insert_query(path!("/root/forbidden"), &picker, cx).await;
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.confirm(false, window, cx)
    });
    cx.run_until_parked();

    assert_eq!(
        picker.read_with(cx, |picker, _| picker.delegate.current_query.clone()),
        path!("/root/forbidden").to_string(),
        "a failed creation leaves the query intact so the user can correct it"
    );
    let error = picker.read_with(cx, |picker, _| picker.delegate.create_error.clone());
    assert!(
        error.is_some_and(|error| error.contains("mkdir permission denied")),
        "a failed creation is surfaced instead of being swallowed"
    );
    assert!(
        picker.read_with(cx, |picker, _| picker.delegate.should_dismiss),
        "the prompt can be dismissed again after a failed creation"
    );
    assert!(
        matches!(confirmed.try_recv(), Ok(None)),
        "a failed creation confirms nothing"
    );
}

#[gpui::test]
async fn test_create_directory_prompt_lists_matches_before_create_row(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "project-a": {},
                "project-b": {},
                "notes.txt": "N",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let create_fs = app_state.fs.clone();
    let create_directory: Arc<CreateDirectoryHandler> = Arc::new(move |path: String, cx: &App| {
        let fs = create_fs.clone();
        cx.spawn(async move |_| {
            let path = std::path::PathBuf::from(path);
            fs.create_dir(&path).await?;
            Ok(fs.canonicalize(&path).await?.to_string_lossy().into_owned())
        })
    });
    let (picker, mut confirmed, cx) = build_new_path_prompt(project, create_directory, cx);

    // Typing a prefix of existing directories lists them first and the create
    // row last, with the first match selected by default.
    insert_query(path!("/root/project"), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["project-a", "project-b", "project"]
    );
    assert_eq!(
        picker.read_with(cx, |picker, _| picker.delegate.selected_index()),
        0
    );
    picker.update_in(cx, |picker, window, cx| {
        for index in 0..picker.delegate.match_count() {
            assert!(
                picker
                    .delegate
                    .render_match(index, false, window, cx)
                    .is_some(),
                "every rendered row resolves to a candidate"
            );
        }
    });

    // Enter enters the first match instead of creating the typed prefix.
    assert_eq!(
        confirm_update_query(&picker, cx),
        Some(path!("/root/project-a/").to_string())
    );
    assert!(
        !app_state
            .fs
            .is_dir(std::path::Path::new(path!("/root/project")))
            .await,
        "entering a match must not create the typed prefix"
    );

    // A matching file is still opened rather than turned into a directory.
    insert_query(path!("/root/notes"), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec!["notes.txt", "notes"]
    );
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.confirm(false, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        confirmed.try_recv().ok().flatten(),
        Some(Some(vec![std::path::PathBuf::from(path!(
            "/root/notes.txt"
        ))])),
        "confirming a matched file opens it"
    );
    assert!(
        !app_state
            .fs
            .is_dir(std::path::Path::new(path!("/root/notes")))
            .await,
        "a matched file is not replaced by a directory with the typed prefix"
    );

    // Selecting the create row explicitly creates the typed directory.
    insert_query(path!("/root/project"), &picker, cx).await;
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.set_selected_index(2, window, cx)
    });
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.confirm(false, window, cx)
    });
    cx.run_until_parked();
    assert!(
        app_state
            .fs
            .is_dir(std::path::Path::new(path!("/root/project")))
            .await,
        "the last row creates the typed directory"
    );

    // With no matches at all, the create row is the only row and comes first.
    insert_query(path!("/root/brand-new"), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec!["brand-new"]);
    assert_eq!(
        picker.read_with(cx, |picker, _| picker.delegate.selected_index()),
        0
    );
}

#[gpui::test]
async fn test_open_path_prompt_panics_with_stale_highlight_positions(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({}))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.prompt_root = "/".to_string();
        picker.delegate.directory_state = DirectoryState::List {
            parent_path: picker.delegate.prompt_root.clone(),
            entries: vec![CandidateInfo {
                path: StringMatchCandidate::new(0, "éclair"),
                is_dir: false,
            }],
            error: None,
        };
        picker.delegate.string_matches = vec![StringMatch {
            candidate_id: 0,
            score: 0.0,
            positions: vec![1],
            string: "ab".to_string(),
        }];

        picker.delegate.render_match(0, false, window, cx);
    });
}

#[gpui::test]
async fn test_open_path_prompt_with_show_hidden(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "directory_1": {},
                "directory_2": {},
                ".hidden": {}
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;

    let (picker, cx) = build_open_path_prompt(project, false, true, cx);

    #[cfg(not(windows))]
    let expected_separator = "./";
    #[cfg(windows)]
    let expected_separator = ".\\";

    insert_query(path!("/root/"), &picker, cx).await;
    assert_eq!(
        collect_match_candidates(&picker, cx),
        vec![expected_separator, ".hidden", "directory_1", "directory_2"]
    );

    insert_query(path!("/root/."), &picker, cx).await;
    assert_eq!(collect_match_candidates(&picker, cx), vec![".hidden"]);
}

#[gpui::test]
async fn test_dismiss_cancels_in_flight_match(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({
                "a1": "A1",
                "a2": "A2",
                "a3": "A3",
            }),
        )
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let (picker, cx) = build_open_path_prompt(project, false, false, cx);

    insert_query(path!("/root/a"), &picker, cx).await;

    let cancel_flag = picker.read_with(cx, |picker, _| picker.delegate.cancel_flag.clone());
    assert!(
        !cancel_flag.load(atomic::Ordering::Acquire),
        "cancel flag should be clear while the prompt is active"
    );

    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.dismissed(window, cx);
    });

    assert!(
        cancel_flag.load(atomic::Ordering::Acquire),
        "dismissing the prompt must cancel the in-flight fuzzy match"
    );
}

fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
    cx.update(|cx| {
        let state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);

        editor::init(cx);
        state
    })
}

fn build_open_path_prompt(
    project: Entity<Project>,
    creating_path: bool,
    show_hidden: bool,
    cx: &mut TestAppContext,
) -> (Entity<Picker<OpenPathDelegate>>, &mut VisualTestContext) {
    let (tx, _) = futures::channel::oneshot::channel();
    let lister = project::DirectoryLister::Project(project.clone());

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    (
        workspace.update_in(cx, |_, window, cx| {
            let delegate = OpenPathDelegate::new(tx, lister.clone(), creating_path, cx);
            let delegate = if show_hidden {
                delegate.show_hidden()
            } else {
                delegate
            };
            cx.new(|cx| {
                let picker = Picker::uniform_list(delegate, window, cx).embedded();
                let query = lister.default_query(cx);
                picker.set_query(&query, window, cx);
                picker
            })
        }),
        cx,
    )
}

fn build_new_path_prompt(
    project: Entity<Project>,
    create_directory: Arc<CreateDirectoryHandler>,
    cx: &mut TestAppContext,
) -> (
    Entity<Picker<OpenPathDelegate>>,
    oneshot::Receiver<Option<Vec<std::path::PathBuf>>>,
    &mut VisualTestContext,
) {
    let (tx, rx) = oneshot::channel();
    let lister = project::DirectoryLister::Project(project.clone());

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    (
        workspace.update_in(cx, |_, window, cx| {
            let delegate = OpenPathDelegate::new(tx, lister.clone(), true, cx)
                .with_create_directory(create_directory);
            cx.new(|cx| {
                let picker = Picker::uniform_list(delegate, window, cx).embedded();
                let query = lister.default_query(cx);
                picker.set_query(&query, window, cx);
                picker
            })
        }),
        rx,
        cx,
    )
}

async fn insert_query(
    query: &str,
    picker: &Entity<Picker<OpenPathDelegate>>,
    cx: &mut VisualTestContext,
) {
    picker
        .update_in(cx, |f, window, cx| {
            f.delegate.update_matches(query.to_string(), window, cx)
        })
        .await;
}

fn confirm_completion(
    query: &str,
    select: usize,
    picker: &Entity<Picker<OpenPathDelegate>>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    picker.update_in(cx, |f, window, cx| {
        if f.delegate.selected_index() != select {
            f.delegate.set_selected_index(select, window, cx);
        }
        f.delegate.confirm_completion(query.to_string(), window, cx)
    })
}

fn confirm_update_query(
    picker: &Entity<Picker<OpenPathDelegate>>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    picker.update_in(cx, |picker, window, cx| {
        picker.delegate.confirm_update_query(window, cx)
    })
}

fn collect_match_candidates(
    picker: &Entity<Picker<OpenPathDelegate>>,
    cx: &mut VisualTestContext,
) -> Vec<String> {
    picker.update(cx, |f, _| f.delegate.collect_match_candidates())
}
