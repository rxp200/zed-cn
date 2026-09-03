use std::sync::Arc;

use fuzzy::StringMatch;
use gpui::{AnyElement, App, Context, DismissEvent, SharedString, Task, Window};
use picker::{Picker, PickerDelegate};
use ui::{ListItem, ListItemSpacing, prelude::*};

type TranslationPicker = Picker<TranslationPickerDelegate>;

/// A searchable list of string options used by the hover-translation settings
/// pickers (translation provider and translation model).
pub struct TranslationPickerDelegate {
    /// `(value, label)` pairs. The value is what gets written to settings.
    options: Vec<(SharedString, SharedString)>,
    filtered: Vec<StringMatch>,
    selected_index: usize,
    current_value: SharedString,
    on_selected: Arc<dyn Fn(SharedString, &mut Window, &mut App) + 'static>,
}

impl TranslationPickerDelegate {
    fn new(
        options: Vec<(SharedString, SharedString)>,
        current_value: SharedString,
        on_selected: impl Fn(SharedString, &mut Window, &mut App) + 'static,
        _cx: &mut Context<TranslationPicker>,
    ) -> Self {
        // Make sure the picker always has at least the current value, so the
        // list is never empty even when no options are available.
        let mut options = options;
        if !options
            .iter()
            .any(|(value, _)| *value == current_value)
            && !current_value.is_empty()
        {
            options.push((current_value.clone(), current_value.clone()));
        }
        let selected_index = options
            .iter()
            .position(|(value, _)| *value == current_value)
            .unwrap_or(0);

        let filtered = options
            .iter()
            .enumerate()
            .map(|(index, (_, label))| StringMatch {
                candidate_id: index,
                string: label.to_string(),
                positions: Vec::new(),
                score: 0.0,
            })
            .collect();

        Self {
            options,
            filtered,
            selected_index,
            current_value,
            on_selected: Arc::new(on_selected),
        }
    }
}

impl PickerDelegate for TranslationPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "translation options picker"
    }

    fn match_count(&self) -> usize {
        self.filtered.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<TranslationPicker>) {
        self.selected_index = ix.min(self.filtered.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "搜索…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<TranslationPicker>,
    ) -> Task<()> {
        let options = self.options.clone();
        let current_value = self.current_value.clone();

        let matches: Vec<StringMatch> = if query.is_empty() {
            options
                .iter()
                .enumerate()
                .map(|(index, (_, label))| StringMatch {
                    candidate_id: index,
                    string: label.to_string(),
                    positions: Vec::new(),
                    score: 0.0,
                })
                .collect()
        } else {
            options
                .iter()
                .enumerate()
                .filter(|(_, (_, label))| label.to_lowercase().contains(&query.to_lowercase()))
                .map(|(index, (_, label))| StringMatch {
                    candidate_id: index,
                    string: label.to_string(),
                    positions: Vec::new(),
                    score: 0.0,
                })
                .collect()
        };

        let selected_index = if query.is_empty() {
            options
                .iter()
                .position(|(value, _)| *value == current_value)
                .unwrap_or(0)
        } else {
            matches
                .iter()
                .position(|m| options[m.candidate_id].0 == current_value)
                .unwrap_or(0)
        };

        self.filtered = matches;
        self.selected_index = selected_index;
        cx.notify();

        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<TranslationPicker>) {
        if let Some(match_result) = self.filtered.get(self.selected_index) {
            let value = self.options[match_result.candidate_id].0.clone();
            (self.on_selected)(value, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<TranslationPicker>) {
        cx.defer_in(window, |picker, window, cx| {
            picker.set_query("", window, cx);
        });
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<TranslationPicker>,
    ) -> Option<Self::ListItem> {
        let match_result = self.filtered.get(ix)?;
        let label = self.options[match_result.candidate_id].1.clone();

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(label))
                .into_any_element(),
        )
    }
}

pub fn translation_picker(
    options: Vec<(SharedString, SharedString)>,
    current_value: SharedString,
    on_selected: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<TranslationPicker>,
) -> TranslationPicker {
    let delegate = TranslationPickerDelegate::new(options, current_value, on_selected, cx);

    Picker::uniform_list(delegate, window, cx)
        .show_scrollbar(true)
        .initial_width(rems_from_px(210_f32))
        .max_height(rems(18.))
        .popover()
}
