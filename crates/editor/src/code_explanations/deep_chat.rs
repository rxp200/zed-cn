use super::*;
use gpui::{Entity, Focusable as _, WeakEntity};

/// Upper bound on follow-up rounds kept in one dialog. Older rounds are dropped
/// first so a long conversation cannot grow without limit.
const MAX_RETAINED_TURNS: usize = 20;
/// Upper bound on earlier rounds included in one model request.
const MAX_PROMPT_TURNS: usize = 6;
/// A single question is rejected above this size instead of being truncated.
const MAX_QUESTION_BYTES: usize = 8 * 1024;
/// Total request budget, matching the inline deep explanation request.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

pub(super) struct DeepChatTurn {
    id: u64,
    question: SharedString,
    answer: Entity<Markdown>,
    status: Entity<DeepExplanationStatus>,
    error: Option<SharedString>,
    context_truncated: bool,
    cancelled: Arc<AtomicBool>,
    partial_output: Arc<std::sync::Mutex<String>>,
    task: Option<Task<Result<()>>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl DeepChatTurn {
    fn is_active(&self, cx: &App) -> bool {
        self.status.read(cx).is_active()
    }
}

/// Follow-up conversation shown below the inline explanation in the deep
/// explanation dialog. It owns the transcript and the composer, so the modal
/// and the pinned view can share one conversation through this entity.
pub(super) struct DeepExplanationChat {
    code: SharedString,
    explanation: Entity<Markdown>,
    explanation_status: Entity<DeepExplanationStatus>,
    project_id: gpui::EntityId,
    turns: Vec<DeepChatTurn>,
    composer: Entity<Editor>,
    composer_empty: bool,
    next_turn_id: u64,
    hidden_turns: usize,
    request_error: Option<SharedString>,
    languages: Arc<language::LanguageRegistry>,
    language: Option<language::LanguageName>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl DeepExplanationChat {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        code: SharedString,
        explanation: Entity<Markdown>,
        explanation_status: Entity<DeepExplanationStatus>,
        project_id: gpui::EntityId,
        languages: Arc<language::LanguageRegistry>,
        language: Option<language::LanguageName>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text(i18n::t!("b647f5530cb8300f"), window, cx);
            editor.set_read_only(explanation_status.read(cx).is_active());
            editor
        });

        let mut this = Self {
            code,
            explanation,
            explanation_status: explanation_status.clone(),
            project_id,
            turns: Vec::new(),
            composer: composer.clone(),
            composer_empty: true,
            next_turn_id: 0,
            hidden_turns: 0,
            request_error: None,
            languages,
            language,
            _subscriptions: Vec::new(),
        };

        this._subscriptions
            .push(cx.observe(&explanation_status, |this, _, cx| {
                this.sync_composer_enabled(cx);
                cx.notify();
            }));
        this._subscriptions
            .push(cx.observe(&composer, |this, _, cx| {
                let empty = this.composer.read(cx).text(cx).trim().is_empty();
                if empty != this.composer_empty {
                    this.composer_empty = empty;
                    cx.notify();
                }
            }));
        this._subscriptions.push(cx.subscribe(
            &composer,
            |_, _, event: &crate::EditorEvent, cx| {
                if matches!(
                    event,
                    crate::EditorEvent::FocusedIn | crate::EditorEvent::Blurred
                ) {
                    cx.notify();
                }
            },
        ));
        cx.on_release(|this, _| {
            for turn in &mut this.turns {
                turn.cancelled.store(true, Ordering::SeqCst);
            }
        })
        .detach();

        this
    }

    pub(super) fn composer(&self) -> Entity<Editor> {
        self.composer.clone()
    }

    pub(super) fn composer_text(&self, cx: &App) -> String {
        self.composer.read(cx).text(cx)
    }

    pub(super) fn composer_focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus_handle(cx)
    }

    pub(super) fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        self.composer.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub(super) fn is_ready(&self, cx: &App) -> bool {
        !self.explanation_status.read(cx).is_active()
    }

    pub(super) fn is_generating(&self, cx: &App) -> bool {
        self.turns.iter().any(|turn| turn.is_active(cx))
    }

    pub(super) fn active_status(&self, cx: &App) -> Option<DeepExplanationStatus> {
        self.turns
            .iter()
            .find(|turn| turn.is_active(cx))
            .map(|turn| *turn.status.read(cx))
    }

    pub(super) fn request_error(&self) -> Option<SharedString> {
        self.request_error.clone()
    }

    pub(super) fn has_turns(&self) -> bool {
        !self.turns.is_empty()
    }

    pub(super) fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Full transcript used by "copy all": the inline explanation plus every
    /// follow-up round, so the copied text matches what the dialog shows.
    pub(super) fn full_text(&self, cx: &App) -> String {
        let mut text = self.explanation.read(cx).source().to_string();
        for turn in &self.turns {
            text.push_str(&i18n::t!(
                "0fad38f14bc8ac19",
                question = turn.question.as_ref(),
                answer = turn.answer.read(cx).source().as_ref()
            ));
        }
        text
    }

    fn sync_composer_enabled(&mut self, cx: &mut Context<Self>) {
        let ready = self.is_ready(cx);
        self.composer
            .update(cx, |editor, _| editor.set_read_only(!ready));
    }

    pub(super) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question = self.composer.read(cx).text(cx);
        let question = question.trim().to_string();
        if question.is_empty() {
            return;
        }
        if self.submit(question, window, cx) {
            self.composer
                .update(cx, |editor, cx| editor.set_text("", window, cx));
        }
    }

    fn submit(&mut self, question: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.is_ready(cx) || self.is_generating(cx) {
            return false;
        }
        if question.len() > MAX_QUESTION_BYTES {
            self.request_error = Some(i18n::t!("b0e939d0ec40fce6").into());
            cx.notify();
            return false;
        }
        if project::DisableAiSettings::get_global(cx).disable_ai {
            self.request_error = Some(i18n::t!("0f4b4b34fffd444a").into());
            cx.notify();
            return false;
        }
        let settings = CodeExplanationSettings::get_global(cx).clone();
        let model = match resolve_model(&settings, cx) {
            Ok(model) => model,
            Err(error) => {
                self.request_error = Some(error.to_string().into());
                cx.notify();
                return false;
            }
        };
        let explanation = if *self.explanation_status.read(cx) == DeepExplanationStatus::Failed {
            SharedString::default()
        } else {
            self.explanation.read(cx).source().clone()
        };
        let (messages, context_truncated) = match build_follow_up_messages(
            &self.code,
            &explanation,
            &self.turns,
            &question,
            &settings,
            cx,
        ) {
            Ok(messages) => messages,
            Err(error) => {
                self.request_error = Some(error.to_string().into());
                cx.notify();
                return false;
            }
        };
        self.request_error = None;
        self.spawn_turn(
            question,
            messages,
            context_truncated,
            settings,
            model,
            window,
            cx,
        );
        true
    }

    fn spawn_turn(
        &mut self,
        question: String,
        messages: Vec<LanguageModelRequestMessage>,
        context_truncated: bool,
        settings: CodeExplanationSettings,
        model: ConfiguredModel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.wrapping_add(1);

        let answer = cx.new(|cx| {
            Markdown::new(
                i18n::t!("a0e88b50278d9816").into(),
                Some(self.languages.clone()),
                self.language.clone(),
                cx,
            )
        });
        let status = cx.new(|_| DeepExplanationStatus::Waiting);
        let cancelled = Arc::new(AtomicBool::new(false));
        let partial_output = Arc::new(std::sync::Mutex::new(String::new()));
        let subscriptions = vec![
            cx.observe(&answer, |_, _, cx| cx.notify()),
            cx.observe(&status, |_, _, cx| cx.notify()),
        ];

        self.turns.push(DeepChatTurn {
            id,
            question: question.into(),
            answer: answer.clone(),
            status: status.clone(),
            error: None,
            context_truncated,
            cancelled: cancelled.clone(),
            partial_output: partial_output.clone(),
            task: None,
            _subscriptions: subscriptions,
        });
        self.trim_turns(cx);

        let project_id = self.project_id;
        let request_key = format!("deep-chat:{}:{id}", content_hash(&self.code));
        let task = cx.spawn_in(window, async move |this, cx| {
            run_turn(
                this,
                id,
                project_id,
                request_key,
                messages,
                settings,
                model,
                cancelled,
                partial_output,
                answer,
                status,
                cx,
            )
            .await
        });
        if let Some(turn) = self.turns.iter_mut().find(|turn| turn.id == id) {
            turn.task = Some(task);
        }
        cx.notify();
    }

    fn trim_turns(&mut self, cx: &mut Context<Self>) {
        while self.turns.len() > MAX_RETAINED_TURNS {
            let excess = self.turns.len() - MAX_RETAINED_TURNS;
            for turn in self.turns.drain(..excess) {
                turn.cancelled.store(true, Ordering::SeqCst);
            }
            self.hidden_turns = self.hidden_turns.saturating_add(excess);
            cx.notify();
        }
    }

    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        let mut stopped = false;
        for turn in &mut self.turns {
            if turn.status.read(cx).is_active() {
                stop_turn_state(turn, cx);
                stopped = true;
            }
        }
        if stopped {
            cx.notify();
        }
    }

    fn stop_turn(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(turn) = self.turns.iter_mut().find(|turn| turn.id == id) else {
            return;
        };
        if turn.status.read(cx).is_active() {
            stop_turn_state(turn, cx);
            cx.notify();
        }
    }

    /// Removes this round and every round after it, then puts the removed
    /// question back into the composer so it can be edited and asked again.
    pub(super) fn remove_turn(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.turns.iter().position(|turn| turn.id == id) else {
            return;
        };
        let removed = self.turns.split_off(index);
        let question = removed.first().map(|turn| turn.question.clone());
        for turn in &removed {
            turn.cancelled.store(true, Ordering::SeqCst);
        }
        drop(removed);
        if let Some(question) = question {
            self.composer.update(cx, |editor, cx| {
                editor.set_text(question.as_ref(), window, cx)
            });
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    pub(super) fn regenerate_turn(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.turns.iter().position(|turn| turn.id == id) else {
            return;
        };
        if self.turns[index].is_active(cx) {
            return;
        }
        let question = self.turns[index].question.to_string();
        let removed = self.turns.split_off(index);
        for turn in &removed {
            turn.cancelled.store(true, Ordering::SeqCst);
        }
        drop(removed);
        self.submit(question, window, cx);
    }

    pub(super) fn clear_turns(&mut self, cx: &mut Context<Self>) {
        for turn in &self.turns {
            turn.cancelled.store(true, Ordering::SeqCst);
        }
        self.turns.clear();
        self.hidden_turns = 0;
        self.request_error = None;
        cx.notify();
    }

    fn set_turn_error(&mut self, id: u64, error: SharedString, cx: &mut Context<Self>) {
        if let Some(turn) = self.turns.iter_mut().find(|turn| turn.id == id) {
            turn.error = Some(error);
            cx.notify();
        }
    }

    fn clear_turn_error(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(turn) = self.turns.iter_mut().find(|turn| turn.id == id)
            && turn.error.take().is_some()
        {
            cx.notify();
        }
    }

    fn render_turn(
        &self,
        turn: &DeepChatTurn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let status = *turn.status.read(cx);
        let active = status.is_active();
        let turn_id = turn.id;
        let element_id = turn_id as usize;
        let truncated = turn.context_truncated;
        let error = turn.error.clone();

        let answer = MarkdownElement::new(
            turn.answer.clone(),
            MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
        )
        .code_block_renderer(CodeBlockRenderer::Default {
            copy_button_visibility: CopyButtonVisibility::AlwaysVisible,
            wrap_button_visibility: WrapButtonVisibility::AlwaysVisible,
            border: true,
        });

        v_flex()
            .id(("deep-chat-turn", element_id))
            .debug_selector(move || format!("deep-chat-turn-{turn_id}"))
            .w_full()
            .min_w_0()
            .gap_2()
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(colors.element_background)
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Icon::new(IconName::UserArrowUp)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(i18n::t!("a0c7716669b5ded0"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(Label::new(turn.question.clone()).size(LabelSize::Small)),
            )
            .child(answer)
            .when(active, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(SpinnerLabel::dots_variant().size(LabelSize::Small))
                        .child(
                            Label::new(status.follow_up_label())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new(
                                ("deep-chat-stop", element_id),
                                i18n::t!("9ad0aac32ea304be"),
                            )
                            .start_icon(Icon::new(IconName::Stop).size(IconSize::Small))
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.stop_turn(turn_id, cx)),
                            ),
                        ),
                )
            })
            .when_some(error, |this, error| {
                this.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            Icon::new(IconName::WarningCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        ),
                )
            })
            .when(truncated, |this| {
                this.child(
                    Label::new(i18n::t!("473f567b99d22553"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(!active, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            IconButton::new(
                                ("deep-chat-regenerate", element_id),
                                IconName::RotateCw,
                            )
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text(i18n::t!("3221a042ea3677a8")))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.regenerate_turn(turn_id, window, cx)
                                },
                            )),
                        )
                        .child(
                            IconButton::new(("deep-chat-copy", element_id), IconName::Copy)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text(i18n::t!("8765b0068d8b00de")))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let text = this
                                        .turns
                                        .iter()
                                        .find(|turn| turn.id == turn_id)
                                        .map(|turn| turn.answer.read(cx).source().to_string())
                                        .unwrap_or_default();
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                                })),
                        )
                        .child(
                            IconButton::new(("deep-chat-delete", element_id), IconName::Trash)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text(i18n::t!("b03c15438f9731b4")))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_turn(turn_id, window, cx)
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

impl gpui::Render for DeepExplanationChat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let turns = self
            .turns
            .iter()
            .map(|turn| self.render_turn(turn, window, cx))
            .collect::<Vec<_>>();

        v_flex()
            .id("deep-explanation-turns")
            .debug_selector(|| "deep-explanation-turns".into())
            .w_full()
            .min_w_0()
            .gap_3()
            .when(self.hidden_turns > 0, |this| {
                this.child(
                    Label::new(i18n::t!("896914267141886d", count = self.hidden_turns))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(!turns.is_empty(), |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .pt_2()
                        .child(div().flex_1().h(px(1.)).bg(colors.border_variant))
                        .child(
                            Label::new(i18n::t!("2f15bb7697969f33"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1().h(px(1.)).bg(colors.border_variant)),
                )
            })
            .children(turns)
    }
}

fn stop_turn_state(turn: &mut DeepChatTurn, cx: &mut Context<DeepExplanationChat>) {
    turn.cancelled.store(true, Ordering::SeqCst);
    let partial = turn
        .partial_output
        .lock()
        .map(|output| output.clone())
        .unwrap_or_default();
    let text = if partial.trim().is_empty() {
        i18n::t!("6d38ae52c5861b3a").to_string()
    } else {
        partial.trim().to_string()
    };
    turn.answer.update(cx, |answer, cx| {
        if answer.source().as_ref() != text.as_str() {
            answer.replace(text, cx);
        }
    });
    turn.status.update(cx, |status, cx| {
        *status = DeepExplanationStatus::Stopped;
        cx.notify();
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    this: WeakEntity<DeepExplanationChat>,
    id: u64,
    project_id: gpui::EntityId,
    request_key: String,
    messages: Vec<LanguageModelRequestMessage>,
    settings: CodeExplanationSettings,
    model: ConfiguredModel,
    cancelled: Arc<AtomicBool>,
    partial_output: Arc<std::sync::Mutex<String>>,
    answer: Entity<Markdown>,
    status: Entity<DeepExplanationStatus>,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let waiting = CodeExplanationRequestWaiter::new(project_id, request_key, 0)?;
    let permit = loop {
        if cancelled.load(Ordering::SeqCst) {
            status.update(cx, |status, cx| {
                *status = DeepExplanationStatus::Stopped;
                cx.notify();
            });
            return Ok(());
        }
        if let Some(permit) = waiting.acquire(settings.max_concurrent_requests) {
            break permit;
        }
        cx.background_executor()
            .timer(std::time::Duration::from_millis(100))
            .await;
    };
    drop(waiting);

    status.update(cx, |status, cx| {
        *status = DeepExplanationStatus::Generating;
        cx.notify();
    });
    answer.update(cx, |answer, cx| {
        answer.replace(i18n::t!("a69bf01f61d6be1e"), cx)
    });
    let executor = cx.background_executor().clone();
    let request = LanguageModelRequest {
        messages,
        temperature: Some(0.2),
        thinking_allowed: false,
        ..Default::default()
    };
    let request = request_model_response(
        model,
        request,
        {
            let answer = answer.clone();
            let cancelled = cancelled.clone();
            let partial_output = partial_output.clone();
            let mut last_display = std::time::Instant::now();
            move |output, cx| {
                anyhow::ensure!(
                    !cancelled.load(Ordering::SeqCst),
                    i18n::t!("988aca0875c41d8a")
                );
                if let Ok(mut partial) = partial_output.lock() {
                    partial.clear();
                    partial.push_str(output);
                }
                if last_display.elapsed() >= std::time::Duration::from_millis(100) {
                    last_display = std::time::Instant::now();
                    answer.update(cx, |answer, cx| answer.replace(output, cx));
                }
                Ok(())
            }
        },
        cx,
    );
    let cancellation = {
        let cancelled = cancelled.clone();
        async move {
            while !cancelled.load(Ordering::SeqCst) {
                executor.timer(std::time::Duration::from_millis(100)).await;
            }
        }
    };

    let result = match futures::future::select(Box::pin(request), Box::pin(cancellation)).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(((), _)) => {
            Err(anyhow::anyhow!(i18n::t!("988aca0875c41d8a")))
        }
    };
    drop(permit);

    let stopped = cancelled.load(Ordering::SeqCst);
    let final_status = if stopped {
        DeepExplanationStatus::Stopped
    } else if result.is_ok() {
        DeepExplanationStatus::Complete
    } else {
        DeepExplanationStatus::Failed
    };
    status.update(cx, |status, cx| {
        *status = final_status;
        cx.notify();
    });

    match result {
        Ok(output) => {
            answer.update(cx, |answer, cx| answer.replace(output, cx));
            this.update(cx, |chat, cx| chat.clear_turn_error(id, cx))
                .log_err();
        }
        Err(error) => {
            let partial = partial_output
                .lock()
                .map(|output| output.clone())
                .unwrap_or_default();
            let partial = if partial.trim().is_empty() {
                None
            } else {
                Some(partial)
            };
            let text = if stopped {
                partial.unwrap_or_else(|| i18n::t!("6d38ae52c5861b3a").to_string())
            } else if let Some(partial) = partial {
                format!(
                    "{partial}\n\n{}",
                    i18n::t!("d355a10af4702541", error = error)
                )
            } else {
                i18n::t!("d355a10af4702541", error = error)
            };
            answer.update(cx, |answer, cx| {
                if answer.source().as_ref() != text.as_str() {
                    answer.replace(text, cx);
                }
            });
            if !stopped {
                this.update(cx, |chat, cx| {
                    chat.set_turn_error(id, error.to_string().into(), cx)
                })
                .log_err();
            }
        }
    }
    Ok(())
}

fn build_follow_up_messages(
    code: &str,
    explanation: &str,
    turns: &[DeepChatTurn],
    question: &str,
    settings: &CodeExplanationSettings,
    cx: &App,
) -> Result<(Vec<LanguageModelRequestMessage>, bool)> {
    let system_prompt = i18n::t!(
        "b8d0b0640ad7e536",
        language = settings.target_language.clone()
    );
    let mut explanation = explanation.to_string();
    let mut truncated = false;
    let reserved = system_prompt.len() + question.len() + 512;
    if code.len() + reserved > MAX_REQUEST_BYTES {
        return Err(anyhow::anyhow!(i18n::t!("928fce573fee9391")));
    }
    let explanation_budget = MAX_REQUEST_BYTES - code.len() - reserved;
    if explanation.len() > explanation_budget {
        explanation = truncate_bytes(&explanation, explanation_budget);
        explanation.push_str(i18n::t!("1cbc22ce5cc0e5ea"));
        truncated = true;
    }

    let context = deep_explanation_markdown(code, &explanation);
    let mut used = system_prompt.len() + context.len() + question.len();
    if used > MAX_REQUEST_BYTES {
        return Err(anyhow::anyhow!(i18n::t!("928fce573fee9391")));
    }

    let mut history = Vec::new();
    for turn in turns.iter().rev() {
        let answer = turn.answer.read(cx).source();
        let cost = turn.question.len() + answer.len() + 32;
        if history.len() >= MAX_PROMPT_TURNS || used + cost > MAX_REQUEST_BYTES {
            truncated = true;
            break;
        }
        used += cost;
        history.push((turn.question.clone(), answer.clone()));
    }
    history.reverse();

    let mut messages = vec![
        LanguageModelRequestMessage {
            role: Role::System,
            content: vec![MessageContent::Text(system_prompt)],
            cache: false,
            reasoning_details: None,
        },
        LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(i18n::t!(
                "a2a1c0d6bc72075f",
                context = context
            ))],
            cache: false,
            reasoning_details: None,
        },
    ];
    for (question, answer) in history {
        messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(question.to_string())],
            cache: false,
            reasoning_details: None,
        });
        messages.push(LanguageModelRequestMessage {
            role: Role::Assistant,
            content: vec![MessageContent::Text(answer.to_string())],
            cache: false,
            reasoning_details: None,
        });
    }
    messages.push(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(question.to_string())],
        cache: false,
        reasoning_details: None,
    });
    Ok((messages, truncated))
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use language_model::LanguageModelRegistry;
    use language_model::fake_provider::FakeLanguageModelProvider;
    use std::sync::atomic::AtomicU64;

    /// The request waiter registry is process global while test `EntityId`s are
    /// per app, so tests must not reuse the same scope id across parallel apps.
    fn test_project_id() -> gpui::EntityId {
        static NEXT: AtomicU64 = AtomicU64::new(1 << 40);
        gpui::EntityId::from(NEXT.fetch_add(1, Ordering::SeqCst))
    }

    fn init_model(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeLanguageModelProvider>,
        language_model::LanguageModel,
    ) {
        crate::editor_tests::init_test(cx, |_| {});
        let provider = cx.update(LanguageModelRegistry::test);
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        r#"{"code_explanations":{"enabled":true,"provider":"fake","model":"fake"}}"#,
                        cx,
                    )
                    .unwrap();
            });
        });
        let model = provider.model("fake");
        (provider, model)
    }

    fn add_chat(
        cx: &mut TestAppContext,
        explanation_status: DeepExplanationStatus,
    ) -> (
        Entity<DeepExplanationChat>,
        Entity<DeepExplanationStatus>,
        &mut VisualTestContext,
    ) {
        let project_id = test_project_id();
        let languages = Arc::new(language::LanguageRegistry::test(
            cx.update(|cx| cx.background_executor().clone()),
        ));
        let mut status_slot = None;
        let (chat, cx) = cx.add_window_view(|window, cx| {
            let markdown = cx.new(|cx| Markdown::new("## 讲解\n正文".into(), None, None, cx));
            let explanation_status = cx.new(|_| explanation_status);
            status_slot = Some(explanation_status.clone());
            DeepExplanationChat::new(
                "fn example() {}".into(),
                markdown,
                explanation_status,
                project_id,
                languages.clone(),
                None,
                window,
                cx,
            )
        });
        (chat, status_slot.expect("status entity"), cx)
    }

    fn ask(cx: &mut VisualTestContext, chat: &Entity<DeepExplanationChat>, question: &str) {
        let composer = chat.read_with(cx, |chat, _| chat.composer());
        composer.update_in(cx, |editor, window, cx| {
            editor.set_text(question, window, cx)
        });
        cx.update(|window, cx| {
            chat.update(cx, |chat, cx| chat.send(window, cx));
        });
        cx.run_until_parked();
    }

    fn request_text(request: &language_model::LanguageModelRequest) -> String {
        request
            .messages
            .iter()
            .map(|message| message.string_contents())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[gpui::test]
    async fn follow_up_rounds_stream_and_regenerate(cx: &mut TestAppContext) {
        let (provider, model) = init_model(cx);
        let (chat, _status, cx) = add_chat(cx, DeepExplanationStatus::Complete);

        ask(cx, &chat, "为什么要用 match？");
        let requests = provider.pending_completions();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .messages
                .last()
                .expect("question message")
                .string_contents(),
            "为什么要用 match？"
        );
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|message| message.string_contents().contains("fn example() {}"))
        );

        provider.send_last_text(&model, "## 原因\n平台不同");
        provider.end_last(&model);
        cx.run_until_parked();
        chat.read_with(cx, |chat, cx| {
            assert_eq!(chat.turns.len(), 1);
            assert_eq!(
                *chat.turns[0].status.read(cx),
                DeepExplanationStatus::Complete
            );
            assert!(chat.full_text(cx).contains("平台不同"));
        });

        let turn_id = chat.read_with(cx, |chat, _| chat.turns[0].id);
        cx.update(|window, cx| {
            chat.update(cx, |chat, cx| chat.regenerate_turn(turn_id, window, cx));
        });
        cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);
        chat.read_with(cx, |chat, cx| {
            assert_eq!(chat.turns.len(), 1);
            assert!(chat.turns[0].is_active(cx));
        });
    }

    #[gpui::test]
    async fn deleting_a_round_restores_the_question_and_drops_later_history(
        cx: &mut TestAppContext,
    ) {
        let (provider, model) = init_model(cx);
        let (chat, _status, cx) = add_chat(cx, DeepExplanationStatus::Complete);

        ask(cx, &chat, "问题A");
        provider.send_last_text(&model, "回答A");
        provider.end_last(&model);
        cx.run_until_parked();

        ask(cx, &chat, "问题B");
        provider.send_last_text(&model, "回答B");
        provider.end_last(&model);
        cx.run_until_parked();
        chat.read_with(cx, |chat, _| assert_eq!(chat.turns.len(), 2));

        let first_id = chat.read_with(cx, |chat, _| chat.turns[0].id);
        cx.update(|window, cx| {
            chat.update(cx, |chat, cx| chat.remove_turn(first_id, window, cx));
        });
        chat.read_with(cx, |chat, cx| {
            assert!(chat.turns.is_empty());
            assert_eq!(chat.composer_text(cx), "问题A");
            assert!(!chat.full_text(cx).contains("回答B"));
        });

        ask(cx, &chat, "问题C");
        let requests = provider.pending_completions();
        assert_eq!(requests.len(), 1);
        let history = request_text(&requests[0]);
        assert!(!history.contains("问题A"));
        assert!(!history.contains("回答B"));
        assert!(history.contains("问题C"));
    }

    #[gpui::test]
    async fn stopping_a_round_keeps_partial_output(cx: &mut TestAppContext) {
        let (provider, model) = init_model(cx);
        let (chat, _status, cx) = add_chat(cx, DeepExplanationStatus::Complete);

        ask(cx, &chat, "长问题");
        provider.send_last_text(&model, "部分回答");
        cx.run_until_parked();
        cx.update(|_window, cx| {
            chat.update(cx, |chat, cx| chat.stop(cx));
        });
        chat.read_with(cx, |chat, cx| {
            assert_eq!(
                *chat.turns[0].status.read(cx),
                DeepExplanationStatus::Stopped
            );
            assert!(chat.turns[0].answer.read(cx).source().contains("部分回答"));
        });
    }

    #[gpui::test]
    async fn request_budget_prefers_recent_rounds(cx: &mut TestAppContext) {
        let (_provider, _model) = init_model(cx);
        let (_chat, _status, cx) = add_chat(cx, DeepExplanationStatus::Complete);

        let mut turns = Vec::new();
        for index in 0..8u64 {
            let answer = cx.new(|cx| Markdown::new("x".repeat(9000).into(), None, None, cx));
            let status = cx.new(|_| DeepExplanationStatus::Complete);
            turns.push(DeepChatTurn {
                id: index,
                question: format!("问题{index}").into(),
                answer,
                status,
                error: None,
                context_truncated: false,
                cancelled: Arc::new(AtomicBool::new(false)),
                partial_output: Arc::new(std::sync::Mutex::new(String::new())),
                task: None,
                _subscriptions: Vec::new(),
            });
        }
        let settings = cx.update(|_, cx| CodeExplanationSettings::get_global(cx).clone());
        let (messages, truncated) = cx
            .read(|cx| {
                build_follow_up_messages(
                    "fn example() {}",
                    "讲解正文",
                    &turns,
                    "新问题",
                    &settings,
                    cx,
                )
            })
            .expect("request within budget");
        assert!(truncated);
        assert!(messages.len() < 3 + turns.len() * 2);
        let joined = messages
            .iter()
            .map(|message| message.string_contents())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("问题7"));
        assert!(!joined.contains("问题0"));
    }

    #[gpui::test]
    async fn overlong_question_is_reported_without_a_request(cx: &mut TestAppContext) {
        let (provider, _model) = init_model(cx);
        let (chat, _status, cx) = add_chat(cx, DeepExplanationStatus::Complete);

        ask(cx, &chat, &"问".repeat(MAX_QUESTION_BYTES + 1));
        assert!(provider.pending_completions().is_empty());
        chat.read_with(cx, |chat, _| {
            assert!(chat.request_error().is_some());
            assert!(chat.turns.is_empty());
        });
    }

    #[gpui::test]
    async fn composer_stays_read_only_until_the_explanation_finishes(cx: &mut TestAppContext) {
        let (_provider, _model) = init_model(cx);
        let (chat, status, cx) = add_chat(cx, DeepExplanationStatus::Generating);

        chat.read_with(cx, |chat, cx| {
            assert!(!chat.is_ready(cx));
            assert!(chat.composer.read(cx).read_only(cx));
        });
        status.update(cx, |status, cx| {
            *status = DeepExplanationStatus::Complete;
            cx.notify();
        });
        cx.run_until_parked();
        chat.read_with(cx, |chat, cx| {
            assert!(chat.is_ready(cx));
            assert!(!chat.composer.read(cx).read_only(cx));
        });
    }

    #[gpui::test]
    async fn composer_action_and_button_send_from_the_modal(cx: &mut TestAppContext) {
        let (provider, model) = init_model(cx);
        let project_id = test_project_id();
        let languages = Arc::new(language::LanguageRegistry::test(
            cx.update(|cx| cx.background_executor().clone()),
        ));
        let mut chat_slot = None;
        let (_modal, cx) = cx.add_window_view(|window, cx| {
            let markdown = cx.new(|cx| Markdown::new("## 讲解\n正文".into(), None, None, cx));
            let status = cx.new(|_| DeepExplanationStatus::Complete);
            let chat = cx.new(|cx| {
                DeepExplanationChat::new(
                    "fn example() {}".into(),
                    markdown.clone(),
                    status.clone(),
                    project_id,
                    languages.clone(),
                    None,
                    window,
                    cx,
                )
            });
            chat_slot = Some(chat.clone());
            DeepExplanationModal::from_markdown(
                markdown,
                status,
                Some(chat),
                "example.rs".into(),
                None,
                true,
                cx,
            )
        });
        let chat = chat_slot.expect("chat entity");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("deep-explanation-composer").is_some());

        let composer = chat.read_with(cx, |chat, _| chat.composer());
        composer.update_in(cx, |editor, window, cx| {
            editor.set_text("键盘发送", window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(crate::SendDeepExplanationFollowUp);
        cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);

        provider.end_last(&model);
        cx.run_until_parked();

        composer.update_in(cx, |editor, window, cx| {
            editor.set_text("按钮发送", window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let send = cx
            .debug_bounds("deep-explanation-send")
            .expect("send button");
        cx.simulate_click(send.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(provider.pending_completions().len(), 1);
        assert_eq!(
            provider.pending_completions()[0]
                .messages
                .last()
                .expect("question message")
                .string_contents(),
            "按钮发送"
        );
    }
}
