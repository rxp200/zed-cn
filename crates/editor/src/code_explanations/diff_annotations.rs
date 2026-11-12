use super::*;
use language::{Buffer, BufferId, BufferSnapshot};
use multi_buffer::{Anchor, MultiBufferSnapshot};
use std::ops::Range;
use text::ToOffset as _;

#[derive(Default)]
pub(super) struct DiffAnnotations {
    task: Option<Task<()>>,
    identity: String,
    blocks: HashSet<crate::CustomBlockId>,
    stopped: bool,
    coverage: Vec<(BufferId, Range<usize>)>,
}

struct VisibleCode {
    snapshot: BufferSnapshot,
    source: gpui::Entity<Buffer>,
    range: Range<usize>,
    deleted_anchor: Option<Anchor>,
}

struct PendingUnit {
    snapshot: BufferSnapshot,
    source: gpui::Entity<Buffer>,
    file: Arc<dyn language::File>,
    unit: crate::code_explanation_units::Unit,
    visible: Vec<(Range<usize>, Option<Anchor>)>,
    key: String,
    namespace: String,
}

pub(super) fn clear(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor
        .explanations
        .write_generation
        .fetch_add(1, Ordering::SeqCst);
    editor.explanations.diff.task = None;
    editor.explanations.diff.identity.clear();
    editor.explanations.diff.coverage.clear();
    editor.explanations.diff.stopped = false;
    let blocks = std::mem::take(&mut editor.explanations.diff.blocks);
    if !blocks.is_empty() {
        editor.remove_blocks(blocks, None, cx);
    }
    editor.explanations.busy = false;
}

pub(super) fn stop(editor: &mut Editor) {
    editor.explanations.diff.task = None;
    editor.explanations.diff.stopped = true;
}

pub(super) fn retry(editor: &mut Editor) {
    editor.explanations.diff.identity.clear();
    editor.explanations.diff.stopped = false;
}

fn visible_code(
    editor: &Editor,
    snapshot: &MultiBufferSnapshot,
    range: Range<language::Point>,
    cx: &App,
) -> Vec<VisibleCode> {
    snapshot
        .range_to_buffer_ranges_with_deleted_hunks(range)
        .filter_map(|(buffer, range, deleted_anchor)| {
            let owner_id = deleted_anchor
                .and_then(|anchor| snapshot.anchor_to_buffer_anchor(anchor))
                .map(|(anchor, _)| anchor.buffer_id)
                .unwrap_or(buffer.remote_id());
            let source = editor.buffer.read(cx).diff_source_buffer(owner_id)?;
            Some(VisibleCode {
                snapshot: buffer.clone(),
                source,
                range: range.start.0..range.end.0,
                deleted_anchor,
            })
        })
        .collect()
}

fn allowed(
    source: &gpui::Entity<Buffer>,
    project: &gpui::Entity<project::Project>,
    cx: &mut App,
) -> bool {
    let Some(file) = source.read(cx).file().cloned() else {
        return false;
    };
    let path = file.path().as_unix_str().to_ascii_lowercase();
    let filename = file.file_name(cx).to_ascii_lowercase();
    if file.is_private()
        || filename.starts_with(".env")
        || [".pem", ".key", ".min.js", ".lock"]
            .iter()
            .any(|suffix| filename.ends_with(suffix))
        || path
            .split('/')
            .any(|part| matches!(part, ".ssh" | ".aws" | ".gnupg"))
    {
        return false;
    }
    let store = project.read(cx).worktree_store();
    let Some(worktree) = store.read(cx).worktree_for_id(file.worktree_id(cx), cx) else {
        return false;
    };
    if project::WorktreeSettings::get(
        Some(settings::SettingsLocation {
            worktree_id: file.worktree_id(cx),
            path: file.path(),
        }),
        cx,
    )
    .is_path_private(file.path())
        || worktree
            .read(cx)
            .snapshot()
            .entry_for_path(file.path())
            .is_some_and(|entry| entry.is_private)
    {
        return false;
    }
    let Some(trust) = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx) else {
        return false;
    };
    trust.update(cx, |trust, cx| {
        trust.can_trust(&store, file.worktree_id(cx), cx)
    })
}

fn annotation_anchor(
    display: &MultiBufferSnapshot,
    snapshot: &BufferSnapshot,
    row: usize,
    column: usize,
    visible: &[(Range<usize>, Option<Anchor>)],
) -> Option<Anchor> {
    let point = language::Point::new(row as u32, column as u32);
    let offset = point.to_offset(snapshot);
    let (_, deleted) = visible.iter().find(|(range, _)| {
        range.contains(&offset) || (range.is_empty() && range.start == offset)
    })?;
    let anchor = snapshot.anchor_after(point);
    match deleted {
        Some(deleted) => Some(deleted.with_diff_base_anchor(anchor)),
        None => display.anchor_in_excerpt(anchor),
    }
}

pub(super) fn schedule(editor: &mut Editor, window: &gpui::Window, cx: &mut Context<Editor>) {
    let settings = CodeExplanationSettings::get_global(cx).clone();
    if !editor.explanations.diff_enabled
        || !settings.enabled
        || project::DisableAiSettings::get_global(cx).disable_ai
    {
        clear(editor, cx);
        return;
    }
    if editor.explanations.diff.stopped {
        return;
    }
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let display = editor.snapshot(window, cx);
    let visible = editor.multi_buffer_visible_range(&display.display_snapshot, cx);
    let preload = preload_range(
        visible.start.row..visible.end.row,
        settings.preload_lines,
        display.buffer_snapshot().max_point().row,
    );
    let range = language::Point::new(*preload.start(), 0)
        ..language::Point::new(preload.end().saturating_add(1), 0)
            .min(display.buffer_snapshot().max_point());
    let regions = visible_code(editor, display.buffer_snapshot(), range, cx)
        .into_iter()
        .filter(|region| allowed(&region.source, &project, cx))
        .collect::<Vec<_>>();
    if regions.is_empty() {
        clear(editor, cx);
        return;
    }
    let provider_configuration = selected_provider_configuration(&settings, cx);
    let epoch = CACHE_EPOCH.load(Ordering::SeqCst);
    let identity = format!(
        "{settings:?}:{provider_configuration}:{epoch}:{:?}",
        regions
            .iter()
            .map(|region| (
                region.snapshot.remote_id(),
                region.snapshot.version().clone(),
                region.snapshot.syntax_update_count(),
                region.source.entity_id(),
                region
                    .source
                    .read(cx)
                    .file()
                    .map(|file| (file.path().clone(), file.is_private())),
                region.deleted_anchor,
            ))
            .collect::<Vec<_>>()
    );
    let coverage = regions
        .iter()
        .map(|region| (region.snapshot.remote_id(), region.range.clone()))
        .collect::<Vec<_>>();
    if identity == editor.explanations.diff.identity
        && (editor.explanations.busy
            || coverage.iter().all(|(id, range)| {
                editor
                    .explanations
                    .diff
                    .coverage
                    .iter()
                    .any(|(old_id, old)| {
                        old_id == id && old.start <= range.start && old.end >= range.end
                    })
            }))
    {
        return;
    }
    clear(editor, cx);
    editor.explanations.diff.identity = identity.clone();
    editor.explanations.diff.coverage = coverage;
    if !editor.is_focused(window) {
        editor.explanations.diff.identity.clear();
        return;
    }
    let model = match resolve_model(&settings, cx) {
        Ok(model) => model,
        Err(error) => {
            editor.explanations.last_error = Some(error.to_string().into());
            return;
        }
    };
    let mut pending = Vec::<PendingUnit>::new();
    let mut seen = HashMap::<(BufferId, Range<usize>), usize>::default();
    for region in regions {
        let snapshot = &region.snapshot;
        let mut units = whole_file_unit(snapshot, settings.max_function_lines)
            .into_iter()
            .collect::<Vec<_>>();
        if units.is_empty() {
            let first = snapshot.offset_to_point(region.range.start).row;
            let last = snapshot.offset_to_point(region.range.end).row;
            let mut row = first;
            while row <= last {
                let candidates = units_at(snapshot, row);
                let next = candidates
                    .iter()
                    .map(|unit| unit.last_row as u32 + 1)
                    .max()
                    .unwrap_or(row + 1);
                units.extend(candidates);
                row = next.max(row + 1);
            }
        }
        let units = crate::code_explanation_units::fit_units_to_budget(
            snapshot,
            units,
            crate::code_explanation_units::request_code_budget(model.model.max_token_count()),
            |text| model.model.estimate_tokens(text),
        );
        for unit in units {
            if unit.owner_lines as u64 > settings.max_function_lines {
                continue;
            }
            let identity = (snapshot.remote_id(), unit.range.clone());
            if let Some(index) = seen.get(&identity) {
                pending[*index]
                    .visible
                    .push((region.range.clone(), region.deleted_anchor));
                continue;
            }
            let Some(file) = region.source.read(cx).file().cloned() else {
                continue;
            };
            let store = project.read(cx).worktree_store();
            let Some(worktree) = store.read(cx).worktree_for_id(file.worktree_id(cx), cx) else {
                continue;
            };
            let namespace = format!(
                "{:?}:{:?}",
                worktree.read(cx).abs_path(),
                project.read(cx).remote_connection_options(cx)
            );
            let language = snapshot
                .language()
                .map(|language| language.name().to_string())
                .unwrap_or_default();
            let code: String = snapshot.text_for_range(unit.range.clone()).collect();
            let key = format!(
                "v6:{provider_configuration}:{:?}:{:?}:{}:{}:{}",
                settings.provider,
                settings.model,
                settings.target_language,
                settings.detailed,
                content_hash(&format!("{language}\n{}\n{code}", unit.context))
            );
            seen.insert(identity, pending.len());
            pending.push(PendingUnit {
                snapshot: snapshot.clone(),
                source: region.source.clone(),
                file,
                unit,
                visible: vec![(region.range.clone(), region.deleted_anchor)],
                key,
                namespace,
            });
        }
    }
    editor.explanations.busy = !pending.is_empty();
    editor.explanations.progress = (0, pending.len());
    editor.explanations.diff.task = Some(cx.spawn(async move |this, cx| {
        cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
        for pending in pending {
            let authorized = |cx: &mut App| {
                this.read_with(cx, |editor, cx| {
                    editor.explanations.diff.identity == identity
                        && editor.project() == Some(&project)
                        && {
                            let display = editor.buffer.read(cx).snapshot(cx);
                            display.buffer_for_id(pending.snapshot.remote_id())
                                .or_else(|| display.buffers_with_paths().find_map(|(snapshot, _)| {
                                    display.diff_for_buffer_id(snapshot.remote_id())
                                        .map(|diff| diff.base_text())
                                        .filter(|base| base.remote_id() == pending.snapshot.remote_id())
                                }))
                                .is_some_and(|current| current.version() == pending.snapshot.version())
                        }
                }).unwrap_or(false)
                    && CodeExplanationSettings::get_global(cx).enabled
                    && !project::DisableAiSettings::get_global(cx).disable_ai
                    && format!("{:?}", CodeExplanationSettings::get_global(cx)) == format!("{settings:?}")
                    && selected_provider_configuration(&settings, cx) == provider_configuration
                    && epoch == CACHE_EPOCH.load(Ordering::SeqCst)
                    && pending.source.read(cx).file().is_some_and(|file| Arc::ptr_eq(file, &pending.file))
                    && allowed(&pending.source, &project, cx)
            };
            if !cx.update(authorized) { break; }
            let cache_path = paths::data_dir().join("code-explanations").join(format!("{}.sqlite", content_hash(&pending.namespace)));
            let request_key = format!("{}:{}", pending.namespace, pending.key);
            let shared_key = format!("{epoch}:{request_key}");
            let waiting = match CodeExplanationRequestWaiter::new(project.entity_id(), request_key, 1) {
                Ok(waiting) => waiting,
                Err(error) => { log::error!("{error}"); break; }
            };
            let permit = loop {
                if !cx.update(authorized) { break None; }
                if let Some(permit) = waiting.acquire(settings.max_concurrent_requests) { break Some(permit); }
                cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
            };
            let Some(permit) = permit else { break };
            let bypass_cache = this.read_with(cx, |editor, _| editor.explanations.bypass_cache).unwrap_or(false);
            let mut cached = if bypass_cache { None } else { shared_result(&shared_key, None) };
            if cached.is_none() && settings.cache_persist && !bypass_cache {
                let path = cache_path.clone();
                let key = pending.key.clone();
                cached = cx.background_spawn(async move { cache_access(&path, &key, None, 0) }).await.log_err().flatten().map(Into::into);
            }
            if !cx.update(authorized) { break; }
            let code: String = pending.snapshot.text_for_range(pending.unit.range.clone()).collect();
            let language = pending.snapshot.language().map(|language| language.name().to_string()).unwrap_or_default();
            let result = match cached {
                Some(text) => Ok(text),
                None => request_if_authorized(authorized, model.clone(), settings.clone(),
                    i18n::t_mix!("8e667b4105a98ce1"; pending.unit.context, code.lines().enumerate().map(|(index, line)| format!("{}: {line}", index + 1)).collect::<Vec<_>>().join("\n"); language = language), cx).await,
            };
            drop(permit);
            if !cx.update(authorized) { break; }
            match result.and_then(|text| {
                let annotations = parse_annotations(&text, &code, if settings.prefer_existing_comments { &pending.unit.commented_rows } else { &[] })?;
                Ok((text, annotations))
            }) {
                Ok((text, annotations)) => {
                    shared_result(&shared_key, Some(text.clone()));
                    if settings.cache_persist {
                        let path = cache_path.clone();
                        let key = pending.key.clone();
                        let text = text.to_string();
                        let budget = settings.cache_max_bytes;
                        let generation = this.read_with(cx, |editor, _| editor.explanations.write_generation.clone()).ok();
                        if let Some(generation) = generation {
                            let expected = generation.load(Ordering::SeqCst);
                            cx.background_spawn(async move {
                                cache_access_guarded(&path, &key, Some(&text), budget, || generation.load(Ordering::SeqCst) == expected && CACHE_EPOCH.load(Ordering::SeqCst) == epoch)
                            }).await.log_err();
                        }
                    }
                    if !cx.update(authorized) { break; }
                    this.update(cx, |editor, cx| {
                        let display = editor.buffer.read(cx).snapshot(cx);
                        for annotation in annotations {
                            let row = pending.unit.first_row + annotation.line - 1;
                            let column = code.lines().nth(annotation.line - 1).map(first_non_whitespace_column).unwrap_or(0);
                            if let Some(anchor) = annotation_anchor(&display, &pending.snapshot, row, column, &pending.visible) {
                                let before = editor.explanations.blocks.clone();
                                show(editor, anchor, annotation.explanation.into(), false, cx);
                                let added = editor.explanations.blocks.difference(&before).copied().collect::<Vec<_>>();
                                for id in added { editor.explanations.blocks.remove(&id); editor.explanations.diff.blocks.insert(id); }
                            }
                        }
                        editor.explanations.progress.0 += 1;
                        cx.notify();
                    }).log_err();
                }
                Err(error) => {
                    this.update(cx, |editor, cx| {
                        editor.explanations.last_error = Some(error.to_string().into());
                        editor.explanations.progress.0 += 1;
                        cx.notify();
                    }).log_err();
                }
            }
        }
        this.update(cx, |editor, cx| {
            if editor.explanations.diff.identity == identity {
                editor.explanations.busy = false;
                editor.explanations.bypass_cache = false;
                cx.notify();
            }
        }).log_err();
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use language::Point;
    use language_model::fake_provider::FakeLanguageModelProvider;
    use multi_buffer::{MultiBuffer, ToPoint as _};

    #[gpui::test]
    async fn git_annotations_generate_reuse_cache_and_clear_on_disable(cx: &mut TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});
        let mut context = crate::test::editor_test_context::EditorTestContext::new(cx).await;
        context.set_state("fn example() {\n    perform();\n}ˇ\n");
        let (source, project) = context.update_editor(|editor, _, cx| {
            let source = editor.buffer.read(cx).as_singleton().expect("source");
            source.update(cx, |buffer, cx| {
                buffer.set_language(Some(language::rust_lang()), cx)
            });
            (source, editor.project().expect("project").clone())
        });
        let provider = Arc::new(FakeLanguageModelProvider::new(
            LanguageModelProviderId::from("fake".to_owned()),
            language_model::LanguageModelProviderName::from("Fake".to_owned()),
        ));
        let model = provider.model("fake");
        context.cx.update(|_, cx| {
            project::trusted_worktrees::init(Default::default(), cx);
            let file = source.read(cx).file().expect("file").clone();
            let store = project.read(cx).worktree_store();
            let trust = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx).expect("trust");
            trust.update(cx, |trust, cx| trust.trust(&store,
                HashSet::from_iter([project::trusted_worktrees::PathTrust::Worktree(file.worktree_id(cx))]), cx));
            LanguageModelRegistry::test(cx);
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| registry.register_provider(provider.clone(), cx));
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.code_explanations = Some(serde_json::from_value(serde_json::json!({
                        "enabled": true, "provider": "fake", "model": "fake", "cache_persist": false,
                    })).expect("settings"));
                });
            });
        });
        context.update_editor(|editor, window, cx| {
            let snapshot = source.read(cx).snapshot();
            let diff = cx.new(|cx| buffer_diff::BufferDiff::new_with_base_text("", &snapshot, cx));
            let multibuffer = cx.new(|cx| {
                let mut multibuffer = MultiBuffer::new(language::Capability::ReadOnly);
                multibuffer.set_excerpts_for_buffer(
                    source.clone(),
                    [Point::zero()..snapshot.max_point()],
                    0,
                    cx,
                );
                multibuffer.add_diff(diff, cx);
                multibuffer.set_all_diff_hunks_expanded(cx);
                multibuffer
            });
            *editor = Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
            editor.explanations.diff_enabled = true;
            window.focus(&editor.focus_handle(cx), cx);
        });
        context.update_editor(|editor, window, cx| schedule(editor, window, cx));
        context
            .cx
            .background_executor
            .timer(std::time::Duration::from_millis(600))
            .await;
        context.cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);
        provider.send_last_text(
            &model,
            r#"[{"line":1,"explanation":"运行示例"},{"line":2,"explanation":"执行操作"}]"#,
        );
        provider.end_last(&model);
        context.cx.run_until_parked();
        context.update_editor(|editor, _, cx| {
            assert_eq!(editor.explanations.diff.blocks.len(), 2);
            assert_eq!(
                source.read(cx).text(),
                "fn example() {\n    perform();\n}\n"
            );
            clear(editor, cx);
        });
        context.update_editor(|editor, window, cx| schedule(editor, window, cx));
        context
            .cx
            .background_executor
            .timer(std::time::Duration::from_millis(600))
            .await;
        context.cx.run_until_parked();
        assert!(provider.pending_completions().is_empty());
        context.update_editor(|editor, _, _| assert_eq!(editor.explanations.diff.blocks.len(), 2));
        let base = context.update_editor(|editor, window, cx| {
            clear(editor, cx);
            let snapshot = source.read(cx).snapshot();
            let diff = cx.new(|cx| {
                buffer_diff::BufferDiff::new_with_base_text(
                    "fn previous() {\n    old_operation();\n}\n",
                    &snapshot,
                    cx,
                )
            });
            let base = diff.read(cx).base_text_buffer().clone();
            base.update(cx, |buffer, cx| {
                buffer.set_language(Some(language::rust_lang()), cx)
            });
            let multibuffer = cx.new(|cx| {
                let mut multibuffer = MultiBuffer::new(language::Capability::ReadOnly);
                multibuffer.set_excerpts_for_buffer(
                    base.clone(),
                    [Point::zero()..base.read(cx).snapshot().max_point()],
                    0,
                    cx,
                );
                multibuffer.add_inverted_diff(diff, source.clone(), cx);
                multibuffer
            });
            *editor = Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
            editor.explanations.diff_enabled = true;
            window.focus(&editor.focus_handle(cx), cx);
            base
        });
        context.update_editor(|editor, window, cx| schedule(editor, window, cx));
        context
            .cx
            .background_executor
            .timer(std::time::Duration::from_millis(600))
            .await;
        context.cx.run_until_parked();
        let requests = provider.pending_completions();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].messages[1]
                .string_contents()
                .contains("old_operation")
        );
        assert!(
            !requests[0].messages[1]
                .string_contents()
                .contains("perform()")
        );
        provider.send_last_text(&model, r#"[{"line":1,"explanation":"旧版本函数"}]"#);
        provider.end_last(&model);
        context.cx.run_until_parked();
        context.update_editor(|editor, _, cx| {
            assert_eq!(editor.explanations.diff.blocks.len(), 1);
            assert_eq!(
                base.read(cx).text(),
                "fn previous() {\n    old_operation();\n}\n"
            );
            assert!(
                editor
                    .buffer
                    .read(cx)
                    .diff_source_buffer(base.read(cx).remote_id())
                    .is_some()
            );
        });
        context.cx.update(|_, cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.code_explanations.get_or_insert_default().enabled = Some(false);
                })
            });
        });
        context.update_editor(|editor, window, cx| {
            schedule(editor, window, cx);
            assert!(editor.explanations.diff.blocks.is_empty());
            assert!(!editor.explanations.busy);
        });
        context.cx.update(|_, cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.code_explanations.get_or_insert_default().enabled = Some(true);
                    settings.project.disable_ai =
                        Some(serde_json::from_value(serde_json::json!(true)).expect("disable AI"));
                });
            });
        });
        context.update_editor(|editor, window, cx| {
            schedule(editor, window, cx);
            assert!(editor.explanations.diff.blocks.is_empty());
        });
        assert!(provider.pending_completions().is_empty());
        context.cx.update(|_, cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.disable_ai =
                        Some(serde_json::from_value(serde_json::json!(false)).expect("enable AI"));
                    settings.project.worktree.private_files = Some(
                        serde_json::from_value(serde_json::json!(["**/file"]))
                            .expect("private paths"),
                    );
                })
            });
        });
        context.update_editor(|editor, window, cx| {
            schedule(editor, window, cx);
            assert!(editor.explanations.diff.blocks.is_empty());
            assert!(editor.explanations.diff.task.is_none());
        });
        assert!(provider.pending_completions().is_empty());
        context.cx.update(|_, cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.private_files =
                        Some(serde_json::from_value(serde_json::json!([])).expect("private paths"));
                })
            });
            let file = source.read(cx).file().expect("source file").clone();
            let store = project.read(cx).worktree_store();
            let trust =
                project::trusted_worktrees::TrustedWorktrees::try_get_global(cx).expect("trust");
            trust.update(cx, |trust, cx| {
                trust.restrict(
                    store.downgrade(),
                    HashSet::from_iter([project::trusted_worktrees::PathTrust::Worktree(
                        file.worktree_id(cx),
                    )]),
                    cx,
                )
            });
        });
        context.update_editor(|editor, window, cx| {
            schedule(editor, window, cx);
            assert!(editor.explanations.diff.blocks.is_empty());
            assert!(editor.explanations.diff.task.is_none());
        });
        assert!(provider.pending_completions().is_empty());
    }

    #[gpui::test]
    async fn switching_the_visible_diff_drops_the_previous_response(cx: &mut TestAppContext) {
        crate::editor_tests::init_test(cx, |_| {});
        let mut context = crate::test::editor_test_context::EditorTestContext::new(cx).await;
        context.set_state("fn shown() {\n    work();\n}ˇ\n");
        let (source, project) = context.update_editor(|editor, _, cx| {
            let source = editor.buffer.read(cx).as_singleton().expect("source");
            source.update(cx, |buffer, cx| {
                buffer.set_language(Some(language::rust_lang()), cx)
            });
            (source, editor.project().expect("project").clone())
        });
        let provider = Arc::new(FakeLanguageModelProvider::new(
            LanguageModelProviderId::from("fake".to_owned()),
            language_model::LanguageModelProviderName::from("Fake".to_owned()),
        ));
        let model = provider.model("fake");
        context.cx.update(|_, cx| {
            project::trusted_worktrees::init(Default::default(), cx);
            let file = source.read(cx).file().expect("file").clone();
            let store = project.read(cx).worktree_store();
            let trust =
                project::trusted_worktrees::TrustedWorktrees::try_get_global(cx).expect("trust");
            trust.update(cx, |trust, cx| {
                trust.trust(
                    &store,
                    HashSet::from_iter([project::trusted_worktrees::PathTrust::Worktree(
                        file.worktree_id(cx),
                    )]),
                    cx,
                )
            });
            LanguageModelRegistry::test(cx);
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(provider.clone(), cx)
            });
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.code_explanations = Some(serde_json::from_value(serde_json::json!({
                    "enabled": true, "provider": "fake", "model": "fake", "cache_persist": false,
                })).expect("settings"));
                })
            });
        });
        context.update_editor(|editor, window, cx| {
            let snapshot = source.read(cx).snapshot();
            let diff = cx.new(|cx| buffer_diff::BufferDiff::new_with_base_text("", &snapshot, cx));
            let multibuffer = cx.new(|cx| {
                let mut multibuffer = MultiBuffer::new(language::Capability::ReadOnly);
                multibuffer.set_excerpts_for_buffer(
                    source.clone(),
                    [Point::zero()..snapshot.max_point()],
                    0,
                    cx,
                );
                multibuffer.add_diff(diff, cx);
                multibuffer.set_all_diff_hunks_expanded(cx);
                multibuffer
            });
            *editor = Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
            editor.explanations.diff_enabled = true;
            window.focus(&editor.focus_handle(cx), cx);
        });
        context.update_editor(|editor, window, cx| schedule(editor, window, cx));
        context
            .cx
            .background_executor
            .timer(std::time::Duration::from_millis(600))
            .await;
        context.cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);
        context.update_editor(|editor, window, cx| {
            let multibuffer = cx.new(|cx| {
                let mut multibuffer = MultiBuffer::new(language::Capability::ReadOnly);
                multibuffer.set_excerpts_for_buffer(
                    source.clone(),
                    [Point::new(1, 0)..Point::new(2, 0)],
                    0,
                    cx,
                );
                multibuffer
            });
            *editor = Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
            editor.explanations.diff_enabled = true;
            window.focus(&editor.focus_handle(cx), cx);
            schedule(editor, window, cx);
            assert!(editor.explanations.diff.blocks.is_empty());
            assert!(editor.explanations.diff.identity.is_empty());
        });
        provider.send_last_text(&model, r#"[{"line":1,"explanation":"过期讲解"}]"#);
        provider.end_last(&model);
        context.cx.run_until_parked();
        context.update_editor(|editor, _, _| assert!(editor.explanations.diff.blocks.is_empty()));
    }

    #[gpui::test]
    fn unified_deleted_hunks_and_split_base_keep_their_own_source_identity(cx: &mut gpui::App) {
        let current = cx.new(|cx| Buffer::local("new_line();\n", cx));
        let current_snapshot = current.read(cx).snapshot();
        let diff = cx.new(|cx| {
            buffer_diff::BufferDiff::new_with_base_text("old_line();\n", &current_snapshot, cx)
        });
        let base = diff.read(cx).base_text_buffer().clone();
        let base_snapshot = base.read(cx).snapshot();
        let unified = cx.new(|cx| {
            let mut buffer = MultiBuffer::new(language::Capability::ReadOnly);
            buffer.set_excerpts_for_buffer(
                current.clone(),
                [Point::zero()..current_snapshot.max_point()],
                0,
                cx,
            );
            buffer.add_diff(diff.clone(), cx);
            buffer.set_all_diff_hunks_expanded(cx);
            buffer
        });
        let display = unified.read(cx).snapshot(cx);
        let deleted = display
            .range_to_buffer_ranges_with_deleted_hunks(Point::zero()..display.max_point())
            .find(|(snapshot, _, deleted)| {
                snapshot.remote_id() == base_snapshot.remote_id() && deleted.is_some()
            })
            .expect("deleted hunk");
        let ranges = vec![(deleted.1.start.0..deleted.1.end.0, deleted.2)];
        let anchor =
            annotation_anchor(&display, &base_snapshot, 0, 0, &ranges).expect("old line anchor");
        assert!(anchor.is_valid(&display));
        assert_eq!(anchor.to_point(&display), Point::zero());
        assert_eq!(
            unified
                .read(cx)
                .diff_source_buffer(current_snapshot.remote_id()),
            Some(current.clone())
        );
        let split = cx.new(|cx| {
            let mut buffer = MultiBuffer::new(language::Capability::ReadOnly);
            buffer.set_excerpts_for_buffer(
                base.clone(),
                [Point::zero()..base_snapshot.max_point()],
                0,
                cx,
            );
            buffer.add_inverted_diff(diff, current.clone(), cx);
            buffer
        });
        assert_eq!(
            split.read(cx).diff_source_buffer(base_snapshot.remote_id()),
            Some(current)
        );
        let split_display = split.read(cx).snapshot(cx);
        let anchor = annotation_anchor(
            &split_display,
            &base_snapshot,
            0,
            0,
            &[(0..base_snapshot.len(), None)],
        )
        .expect("split anchor");
        assert!(anchor.is_valid(&split_display));
        assert!(
            annotation_anchor(
                &split_display,
                &current_snapshot,
                0,
                0,
                &[(0..current_snapshot.len(), None)]
            )
            .is_none()
        );
    }

    #[gpui::test]
    fn annotation_positions_are_limited_to_the_matching_version_and_excerpt(cx: &mut gpui::App) {
        let first = cx.new(|cx| Buffer::local("first\nsecond\nthird\n", cx));
        let other = cx.new(|cx| Buffer::local("first\nsecond\nthird\n", cx));
        let snapshot = first.read(cx).snapshot();
        let other_snapshot = other.read(cx).snapshot();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(language::Capability::ReadOnly);
            multibuffer.set_excerpts_for_buffer(first, [Point::new(1, 0)..Point::new(2, 0)], 0, cx);
            multibuffer
        });
        let display = multibuffer.read(cx).snapshot(cx);
        let visible = vec![(6..13, None)];
        assert!(annotation_anchor(&display, &snapshot, 1, 0, &visible).is_some());
        assert!(annotation_anchor(&display, &snapshot, 0, 0, &visible).is_none());
        assert!(annotation_anchor(&display, &other_snapshot, 1, 0, &visible).is_none());
    }
}
