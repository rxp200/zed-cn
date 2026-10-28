use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, DisplayId, Entity, EntityId, Global,
    PlatformDisplay, SharedString, Subscription, WeakEntity, Window, WindowHandle,
};
use notifications::{PopupNotification, PopupNotificationEvent};
use terminal::TaskStatus;
use ui::{Color, IconName};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::{TerminalView, terminal_panel::TerminalPanel};

/// Maximum number of terminal notifications visible at the same time. When a
/// new one arrives while all slots are taken, the oldest is closed to make room.
const MAX_VISIBLE_NOTIFICATIONS: usize = 3;

/// Silence longer than this ends a busy period for the purposes of notifying
/// about long-running tasks. It is more forgiving than the tab indicator so that
/// a task which pauses for a few seconds between output bursts still counts as
/// one run.
const TERMINAL_TASK_IDLE_GAP: Duration = Duration::from_secs(5);

/// A run must last at least this long before its completion is worth a popup.
const TERMINAL_LONG_TASK_DURATION: Duration = Duration::from_secs(60);

/// How a long-running terminal run finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongTaskFinished {
    /// A task-backed terminal reported an exit status.
    Task { success: bool },
    /// A plain terminal stopped producing output.
    Idle,
}

/// Turns a terminal's output activity and task status into at most one
/// notification per run. A task-backed terminal reports its completion exactly;
/// a plain shell terminal is approximated by a silent gap after a busy period,
/// because Zed has no shell integration to delimit commands.
#[derive(Default)]
pub(crate) struct LongTaskTracker {
    started_at: Option<Instant>,
}

impl LongTaskTracker {
    /// A task terminal starts running as soon as it exists, even before it
    /// produces any output.
    pub(crate) fn for_task(now: Instant) -> Self {
        Self {
            started_at: Some(now),
        }
    }

    pub(crate) fn observe(
        &mut self,
        now: Instant,
        last_output: Option<Instant>,
        task_status: Option<TaskStatus>,
    ) -> Option<(LongTaskFinished, Instant)> {
        match task_status {
            Some(TaskStatus::Running) => {
                self.started_at.get_or_insert(now);
                None
            }
            Some(TaskStatus::Completed { success }) => {
                let started_at = self.started_at.take()?;
                let long_enough =
                    now.saturating_duration_since(started_at) >= TERMINAL_LONG_TASK_DURATION;
                long_enough.then_some((LongTaskFinished::Task { success }, started_at))
            }
            // A task whose exit was never reported has no outcome worth
            // announcing, and its run is over either way.
            Some(TaskStatus::Unknown) => {
                self.started_at = None;
                None
            }
            None => {
                let session_alive = last_output
                    .is_some_and(|at| now.saturating_duration_since(at) < TERMINAL_TASK_IDLE_GAP);
                if session_alive {
                    if let Some(at) = last_output {
                        self.started_at.get_or_insert(at);
                    }
                    return None;
                }

                let started_at = self.started_at.take()?;
                let long_enough =
                    now.saturating_duration_since(started_at) >= TERMINAL_LONG_TASK_DURATION;
                long_enough.then_some((LongTaskFinished::Idle, started_at))
            }
        }
    }
}

/// Everything the popup needs to describe a finished long-running terminal task.
#[derive(Clone)]
pub struct LongTaskNotification {
    pub title: SharedString,
    pub caption: SharedString,
    pub project_name: Option<SharedString>,
    pub icon: IconName,
    pub icon_color: Color,
}

#[cfg(test)]
mod tracker_tests {
    use super::*;

    fn seconds(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn idle_session_shorter_than_a_minute_does_not_notify() {
        let start = Instant::now();
        let mut tracker = LongTaskTracker::default();

        assert_eq!(tracker.observe(start, Some(start), None), None);
        assert_eq!(
            tracker.observe(start + seconds(30), Some(start + seconds(30)), None),
            None
        );
        assert_eq!(
            tracker.observe(start + seconds(36), Some(start + seconds(30)), None),
            None,
            "a short busy period must not produce a notification"
        );
    }

    #[test]
    fn idle_session_of_a_minute_notifies_once() {
        let start = Instant::now();
        let mut tracker = LongTaskTracker::default();

        tracker.observe(start, Some(start), None);
        tracker.observe(start + seconds(60), Some(start + seconds(60)), None);

        assert_eq!(
            tracker.observe(start + seconds(66), Some(start + seconds(60)), None),
            Some((LongTaskFinished::Idle, start)),
            "a busy period of at least a minute must notify when it ends"
        );
        assert_eq!(
            tracker.observe(start + seconds(72), Some(start + seconds(60)), None),
            None,
            "the finished run must not notify again"
        );
    }

    #[test]
    fn short_gaps_keep_one_busy_session_alive() {
        let start = Instant::now();
        let mut tracker = LongTaskTracker::default();

        for offset in (0..72).step_by(4) {
            tracker.observe(start + seconds(offset), Some(start + seconds(offset)), None);
        }

        assert_eq!(
            tracker.observe(start + seconds(78), Some(start + seconds(68)), None),
            Some((LongTaskFinished::Idle, start)),
            "output every few seconds must count as a single run"
        );
    }

    #[test]
    fn long_gap_splits_the_session() {
        let start = Instant::now();
        let mut tracker = LongTaskTracker::default();

        tracker.observe(start, Some(start), None);
        // The gap ends the first run long before a minute has passed.
        assert_eq!(
            tracker.observe(start + seconds(20), Some(start), None),
            None
        );

        let resumed = start + seconds(21);
        tracker.observe(resumed, Some(resumed), None);
        assert_eq!(
            tracker.observe(resumed + seconds(60), Some(resumed + seconds(60)), None),
            None,
            "the resumed run is only just under a minute old"
        );
        assert_eq!(
            tracker.observe(resumed + seconds(66), Some(resumed + seconds(60)), None),
            Some((LongTaskFinished::Idle, resumed)),
            "the second run must be measured from its own start"
        );
    }

    #[test]
    fn task_reports_its_own_exit_status() {
        let start = Instant::now();

        for (success, elapsed) in [(true, 61u64), (false, 61)] {
            let mut tracker = LongTaskTracker::for_task(start);
            assert_eq!(
                tracker.observe(start + seconds(30), None, Some(TaskStatus::Running)),
                None
            );
            assert_eq!(
                tracker.observe(
                    start + seconds(elapsed),
                    None,
                    Some(TaskStatus::Completed { success })
                ),
                Some((LongTaskFinished::Task { success }, start))
            );
            assert_eq!(
                tracker.observe(
                    start + seconds(elapsed + 10),
                    None,
                    Some(TaskStatus::Completed { success })
                ),
                None,
                "a completed task must not notify again"
            );
        }
    }

    #[test]
    fn short_or_unreported_tasks_do_not_notify() {
        let start = Instant::now();

        let mut tracker = LongTaskTracker::for_task(start);
        assert_eq!(
            tracker.observe(
                start + seconds(20),
                None,
                Some(TaskStatus::Completed { success: true })
            ),
            None,
            "a task under a minute is not worth a popup"
        );

        let mut tracker = LongTaskTracker::for_task(start);
        assert_eq!(
            tracker.observe(start + seconds(61), None, Some(TaskStatus::Unknown)),
            None,
            "a task without a reported exit status is not announced"
        );
    }
}

struct PopupWindow {
    handle: WindowHandle<PopupNotification>,
    popup: Entity<PopupNotification>,
    _subscription: Subscription,
}

struct NotificationEntry {
    content: LongTaskNotification,
    terminal: WeakEntity<TerminalView>,
    workspace: WeakEntity<Workspace>,
    /// The window that owns the terminal, used to bring it back to the front.
    source_window: AnyWindowHandle,
    /// Dropping the entry stops watching its terminal.
    _release_subscription: Subscription,
}

/// The popups shown on one display, newest first.
///
/// Slot `i` is the `i`-th popup from the top and is backed by `windows[i]` for
/// as long as the stack reaches that far. Reordering swaps what each window
/// renders instead of moving it, because GPUI cannot reposition an existing
/// window; that way dismissing a notification makes the ones below it scroll up
/// instead of leaving a hole.
struct DisplayStack {
    screen: Rc<dyn PlatformDisplay>,
    entries: Vec<NotificationEntry>,
    windows: Vec<PopupWindow>,
}

impl DisplayStack {
    fn new(screen: Rc<dyn PlatformDisplay>) -> Self {
        Self {
            screen,
            entries: Vec::new(),
            windows: Vec::new(),
        }
    }

    fn display_id(&self) -> DisplayId {
        self.screen.id()
    }

    /// Adds `entry` as the newest notification, dropping the oldest when the
    /// display already shows the maximum number of popups.
    fn push(&mut self, entry: NotificationEntry, cx: &mut Context<TerminalNotificationStack>) {
        if self.entries.len() >= MAX_VISIBLE_NOTIFICATIONS {
            self.entries.pop();
        }
        self.entries.insert(0, entry);

        while self.windows.len() < self.entries.len() {
            let slot = self.windows.len();
            let Some(content) = self.entries.get(slot).map(|entry| entry.content.clone()) else {
                break;
            };
            let Some(window) = self.open_window(slot, content, cx) else {
                self.entries.pop();
                break;
            };
            self.windows.push(window);
        }

        self.sync_contents(cx);
    }

    /// Removes the entry at `slot` and closes the bottom-most popup.
    ///
    /// Windows keep their screen position, so the surviving entries are pushed
    /// up by one slot. The window that hosted the removed entry is never the one
    /// that closes unless it was the bottom one.
    fn remove_at(&mut self, slot: usize, cx: &mut App) -> Option<NotificationEntry> {
        if slot >= self.entries.len() {
            return None;
        }

        let entry = self.entries.remove(slot);
        if let Some(window) = self.windows.pop() {
            window
                .handle
                .update(cx, |_, window, _| window.remove_window())
                .log_err();
        }
        self.sync_contents(cx);
        Some(entry)
    }

    fn open_window(
        &self,
        slot: usize,
        content: LongTaskNotification,
        cx: &mut Context<TerminalNotificationStack>,
    ) -> Option<PopupWindow> {
        let options = PopupNotification::window_options_at_slot(self.screen.clone(), slot, cx);
        let handle = cx
            .open_window(options, move |_window, cx| {
                cx.new(move |_| {
                    PopupNotification::new(
                        content.title,
                        Some(content.caption),
                        content.icon,
                        content.project_name,
                    )
                    .id("terminal-notification")
                    .icon_color(content.icon_color)
                    .accept_label(i18n::t!("afd0b9482dfa57f0"))
                })
            })
            .log_err()?;

        let popup = handle.entity(cx).log_err()?;
        let display_id = self.display_id();
        let subscription = cx.subscribe(
            &popup,
            move |stack, _, event: &PopupNotificationEvent, cx| {
                stack.handle_popup_event(
                    display_id,
                    slot,
                    matches!(event, PopupNotificationEvent::Accepted),
                    cx,
                );
            },
        );

        Some(PopupWindow {
            handle,
            popup,
            _subscription: subscription,
        })
    }

    /// Renders the entry currently assigned to each slot.
    fn sync_contents(&mut self, cx: &mut App) {
        for (window, entry) in self.windows.iter().zip(self.entries.iter()) {
            let content = entry.content.clone();
            window.popup.update(cx, |popup, cx| {
                popup.set_content(
                    content.title,
                    Some(content.caption),
                    content.icon,
                    content.icon_color,
                    content.project_name,
                    cx,
                );
            });
        }
    }
}

/// Tracks the terminal notifications that are currently on screen so that they
/// stack instead of overlapping, and so that a notification can be dismissed
/// when its terminal goes away or is brought back into focus.
#[derive(Default)]
pub struct TerminalNotificationStack {
    displays: Vec<DisplayStack>,
}

struct GlobalStack(Entity<TerminalNotificationStack>);

impl Global for GlobalStack {}

impl TerminalNotificationStack {
    /// Returns the process-wide stack, creating it on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalStack>() {
            return global.0.clone();
        }

        let stack = cx.new(|_| TerminalNotificationStack::default());
        cx.set_global(GlobalStack(stack.clone()));
        stack
    }

    pub fn init(cx: &mut App) {
        Self::global(cx);
    }

    /// Dismisses the notification belonging to `terminal_id`, if any.
    pub fn dismiss_for_terminal(&mut self, terminal_id: EntityId, cx: &mut Context<Self>) {
        for display in &mut self.displays {
            if let Some(slot) = display
                .entries
                .iter()
                .position(|entry| entry.terminal.entity_id() == terminal_id)
            {
                let _removed = display.remove_at(slot, cx);
            }
        }
        self.displays.retain(|display| !display.entries.is_empty());
    }

    pub fn push(
        &mut self,
        content: LongTaskNotification,
        terminal: Entity<TerminalView>,
        workspace: WeakEntity<Workspace>,
        source_window: AnyWindowHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reap_released(cx);
        // A terminal that finishes again replaces its own previous notification.
        self.dismiss_for_terminal(terminal.entity_id(), cx);

        let Some(screen) = window.display(cx).or_else(|| cx.primary_display()) else {
            log::error!("no display available for the terminal notification");
            return;
        };
        let display_id = screen.id();

        let terminal_id = terminal.entity_id();
        let release_subscription = cx.observe_release(&terminal, move |stack, _, cx| {
            stack.dismiss_for_terminal(terminal_id, cx);
        });

        let entry = NotificationEntry {
            content,
            terminal: terminal.downgrade(),
            workspace,
            source_window,
            _release_subscription: release_subscription,
        };

        if let Some(display) = self
            .displays
            .iter_mut()
            .find(|display| display.display_id() == display_id)
        {
            display.push(entry, cx);
        } else {
            let mut display = DisplayStack::new(screen);
            display.push(entry, cx);
            self.displays.push(display);
        }
    }

    /// Closes notifications whose terminal or workspace no longer exists.
    fn reap_released(&mut self, cx: &mut Context<Self>) {
        for display in &mut self.displays {
            let mut slot = 0;
            while slot < display.entries.len() {
                let released = display.entries.get(slot).is_none_or(|entry| {
                    entry.terminal.upgrade().is_none() || entry.workspace.upgrade().is_none()
                });
                if released {
                    let _removed = display.remove_at(slot, cx);
                } else {
                    slot += 1;
                }
            }
        }
        self.displays.retain(|display| !display.entries.is_empty());
    }

    /// Handles the accept or dismiss button of one popup.
    fn handle_popup_event(
        &mut self,
        display_id: DisplayId,
        slot: usize,
        accepted: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(display) = self
            .displays
            .iter_mut()
            .find(|display| display.display_id() == display_id)
        else {
            return;
        };
        let Some(entry) = display.remove_at(slot, cx) else {
            return;
        };
        self.displays.retain(|display| !display.entries.is_empty());

        if accepted {
            activate_terminal(entry.workspace, entry.terminal, entry.source_window, cx);
        }
    }
}

#[cfg(test)]
impl TerminalNotificationStack {
    /// The terminal and popup of every visible notification, top slot first,
    /// for regression tests that assert stacking order.
    pub(crate) fn visible_notifications(&self) -> Vec<(EntityId, Entity<PopupNotification>)> {
        self.displays
            .iter()
            .flat_map(|display| {
                display
                    .entries
                    .iter()
                    .zip(display.windows.iter())
                    .map(|(entry, window)| (entry.terminal.entity_id(), window.popup.clone()))
            })
            .collect()
    }
}

/// Brings the finished terminal back into view: activates its window and
/// workspace, reveals the terminal panel if the terminal is docked, and focuses
/// the terminal item itself.
fn activate_terminal(
    workspace: WeakEntity<Workspace>,
    terminal: WeakEntity<TerminalView>,
    source_window: AnyWindowHandle,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let Some(window_handle) = source_window.downcast::<MultiWorkspace>() else {
        log::error!("terminal notification belongs to a window that is not a MultiWorkspace");
        return;
    };

    cx.activate(true);
    cx.defer(move |cx| {
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.activate(workspace.clone(), None, window, cx);
                window.activate_window();

                workspace.update(cx, |workspace, cx| {
                    let Some(terminal) = terminal.upgrade() else {
                        return;
                    };

                    if workspace.activate_item(&terminal, true, true, window, cx) {
                        return;
                    }

                    let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
                        return;
                    };
                    workspace.reveal_panel::<TerminalPanel>(window, cx);
                    panel.update(cx, |panel, cx| {
                        if !panel.activate_terminal(&terminal, true, window, cx) {
                            log::error!("terminal notification lost track of its terminal");
                        }
                    });
                });
            })
            .log_err();
    });
}
