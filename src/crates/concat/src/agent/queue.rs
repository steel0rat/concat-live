// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! The embedded agent's ordered, durable tasks, scoped to the current chat.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, VecModel};

use super::edit_bounds::EditBounds;
use super::edit_scope::LiveEditScope;
use super::protocol::ModelSelection;
use super::storage::{read_json, write_json};
use super::{AgentPane, MAX_ATTACHMENTS, PreparedTurn, Submission, cap};
use crate::host::Shell;
use crate::i18n::t;
use crate::ui::{Agent, AgentQueueItem};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct QueueKey {
    workspace: PathBuf,
    thread: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Waiting,
    Running,
    Complete,
    Failed,
    Interrupted,
    Cancelled,
}

impl Phase {
    fn label(self) -> String {
        match self {
            Self::Waiting => t("agent.queueWaiting"),
            Self::Running => t("agent.queueRunning"),
            Self::Complete => t("agent.queueComplete"),
            Self::Failed => t("agent.queueFailed"),
            Self::Interrupted => t("agent.queueInterrupted"),
            Self::Cancelled => t("agent.queueCancelled"),
        }
    }

    fn recoverable(self) -> bool {
        matches!(self, Self::Failed | Self::Interrupted | Self::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Task {
    id: u64,
    text: String,
    files: Vec<PathBuf>,
    project: Option<PathBuf>,
    timeline: Option<String>,
    detail: String,
    label: String,
    bounds: Option<EditBounds>,
    model: Option<String>,
    effort: Option<String>,
    phase: Phase,
    reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Tasks {
    last_id: u64,
    paused: bool,
    tasks: Vec<Task>,
}

impl Tasks {
    fn next_index(&self) -> Option<usize> {
        self.tasks
            .iter()
            .position(|task| task.phase == Phase::Waiting)
    }

    fn running(&self) -> Option<usize> {
        self.tasks
            .iter()
            .position(|task| task.phase == Phase::Running)
    }

    fn waiting_slots(&self) -> Vec<usize> {
        self.tasks
            .iter()
            .enumerate()
            .filter_map(|(index, task)| (task.phase == Phase::Waiting).then_some(index))
            .collect()
    }

    fn move_waiting(&mut self, id: u64, offset: i32) -> bool {
        if offset != -1 && offset != 1 {
            return false;
        }
        let waiting = self.waiting_slots();
        let Some(from) = waiting.iter().position(|index| self.tasks[*index].id == id) else {
            return false;
        };
        let Some(to) = from
            .checked_add_signed(offset as isize)
            .and_then(|at| waiting.get(at))
        else {
            return false;
        };
        self.tasks.swap(waiting[from], *to);
        true
    }

    fn reorder_waiting(&mut self, id: u64, anchor: u64, after: bool) -> bool {
        if id == anchor {
            return false;
        }
        let waiting = self.waiting_slots();
        let position = |id| waiting.iter().position(|index| self.tasks[*index].id == id);
        let (Some(from), Some(anchor)) = (position(id), position(anchor)) else {
            return false;
        };
        let insertion = anchor + usize::from(after);
        let to = insertion - usize::from(from < insertion);
        // Rotate through the waiting slots so that every other phase keeps its row.
        if from < to {
            for pair in waiting[from..=to].windows(2) {
                self.tasks.swap(pair[0], pair[1]);
            }
        } else if from > to {
            for pair in waiting[to..=from].windows(2).rev() {
                self.tasks.swap(pair[0], pair[1]);
            }
        }
        from != to
    }

    fn restore_cancelled(&mut self, id: u64) -> bool {
        let Some(task) = self
            .tasks
            .iter_mut()
            .find(|task| task.id == id && task.phase == Phase::Cancelled)
        else {
            return false;
        };
        task.phase = Phase::Waiting;
        task.reason.clear();
        true
    }

    fn restore(&mut self) {
        let mut interrupted = false;
        for task in self
            .tasks
            .iter_mut()
            .filter(|task| task.phase == Phase::Running)
        {
            task.phase = Phase::Interrupted;
            task.reason = t("agent.queueUnknown");
            interrupted = true;
        }
        self.paused |= self.next_index().is_some() || interrupted;
    }
}

#[derive(Serialize, Deserialize)]
struct StoredQueue {
    key: QueueKey,
    tasks: Tasks,
}

#[derive(Default)]
pub(super) struct TaskQueue {
    file: Option<PathBuf>,
    owner: Option<QueueKey>,
    drag_scope: u64,
    tasks: Tasks,
    persisted: Tasks,
    loaded: bool,
    open: bool,
    interrupted: bool,
    pub(super) current_prompt: Option<String>,
    error: String,
    retry_at: Option<Instant>,
    rows: Rc<VecModel<AgentQueueItem>>,
}

impl TaskQueue {
    pub(super) fn new(config: Option<&Path>) -> Self {
        Self {
            file: config.map(|folder| folder.join("agent-queues.json")),
            ..Self::default()
        }
    }

    fn sync(&mut self, owner: Option<QueueKey>) {
        if owner == self.owner || !self.flush(false) {
            return;
        }
        self.owner = owner;
        self.drag_scope = self.drag_scope.wrapping_add(1);
        self.tasks = Tasks::default();
        self.persisted = Tasks::default();
        self.loaded = false;
        self.error.clear();
        self.interrupted = false;
        let (Some(file), Some(owner)) = (&self.file, &self.owner) else {
            return;
        };
        match read_json::<Vec<StoredQueue>>(file) {
            Ok(records) => {
                self.persisted = records
                    .unwrap_or_default()
                    .into_iter()
                    .find(|queue| &queue.key == owner)
                    .map(|queue| queue.tasks)
                    .unwrap_or_default();
                self.tasks = self.persisted.clone();
                self.tasks.restore();
                self.loaded = true;
            }
            Err(error) => self.error = error,
        }
    }

    fn write(&mut self, tasks: Tasks) -> Result<(), String> {
        if self.persisted == tasks {
            self.tasks = tasks;
            return Ok(());
        }
        let (Some(file), Some(owner)) = (&self.file, &self.owner) else {
            return Err(t("agent.queueStoreUnavailable"));
        };
        fs::create_dir_all(file.parent().expect("queue data has a settings folder"))
            .map_err(|error| error.to_string())?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let lock = options
            .open(file.with_extension("lock"))
            .map_err(|error| error.to_string())?;
        lock.try_lock().map_err(|error| error.to_string())?;
        let mut queues = read_json::<Vec<StoredQueue>>(file)?.unwrap_or_default();
        if let Some(previous) = queues.iter_mut().find(|record| &record.key == owner) {
            if self.persisted != previous.tasks {
                return Err(t("agent.queueStoreChanged"));
            }
            previous.tasks = tasks.clone();
        } else {
            if self.persisted != Tasks::default() {
                return Err(t("agent.queueStoreChanged"));
            }
            queues.push(StoredQueue {
                key: owner.clone(),
                tasks: tasks.clone(),
            });
        }
        write_json(file, &queues)?;
        self.persisted = tasks.clone();
        self.tasks = tasks;
        self.error.clear();
        self.retry_at = None;
        Ok(())
    }

    fn flush(&mut self, force: bool) -> bool {
        if self.tasks == self.persisted {
            return true;
        }
        if !force
            && self
                .retry_at
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return false;
        }
        if let Err(error) = self.write(self.tasks.clone()) {
            self.error = error;
            self.tasks.paused = true;
            self.retry_at = Some(Instant::now() + Duration::from_secs(3));
            return false;
        }
        true
    }

    fn change(&mut self, tasks: Tasks) -> bool {
        match self.write(tasks) {
            Ok(()) => true,
            Err(error) => {
                self.error = error;
                false
            }
        }
    }

    fn finish(&mut self, phase: Phase, reason: String) {
        let Some(index) = self.tasks.running() else {
            return;
        };
        self.tasks.tasks[index].phase = phase;
        self.tasks.tasks[index].reason = reason;
        self.tasks.paused |= phase != Phase::Complete;
        self.interrupted = false;
        self.flush(true);
    }
}

impl AgentPane {
    fn queue_key(&self) -> Option<QueueKey> {
        Some(QueueKey {
            workspace: self.context.as_ref()?.path.clone(),
            thread: self.thread.clone()?,
        })
    }

    pub(super) fn install_queue_model(&self) {
        if let Some(app) = self.ui.upgrade() {
            app.global::<Agent>()
                .set_queue_items(self.queue.rows.clone().into());
        }
    }

    pub(super) fn close_queue(&mut self) {
        self.queue.open = false;
    }

    pub(super) fn toggle_queue(&mut self) {
        self.queue.open = !self.queue.open && self.enabled();
        if self.queue.open {
            self.close_notes_view();
            self.close_brief_view();
            self.close_edit_scope();
            self.history.open = false;
        }
        self.publish();
    }

    fn queue_visible_for_chat(&self) -> bool {
        self.queue.loaded && self.queue.owner.is_some() && self.queue.owner == self.queue_key()
    }

    fn queue_can_enqueue(&self) -> bool {
        self.queue_visible_for_chat()
            && self.allowed()
            && !self.dictation.active()
            && self.history.opening.is_none()
    }

    pub(super) fn publish_queue(&self, global: &Agent<'_>) {
        let visible = self.queue_visible_for_chat();
        let tasks = if visible {
            self.queue.tasks.tasks.as_slice()
        } else {
            &[]
        };
        global.set_queue_open(self.queue.open);
        global.set_queue_paused(self.queue.tasks.paused);
        global.set_queue_can_enqueue(self.queue_can_enqueue());
        global.set_queue_can_run(visible && self.allowed());
        global.set_queue_error(self.queue.error.as_str().into());
        let waiting = tasks
            .iter()
            .filter(|task| task.phase == Phase::Waiting)
            .count();
        global.set_queue_count(i32::try_from(waiting).unwrap_or(i32::MAX));
        if !self.queue.open {
            return;
        }
        let mut rows = Vec::new();
        if let Some(prompt) = &self.queue.current_prompt {
            rows.push(AgentQueueItem {
                id: String::new().into(),
                prompt: cap(prompt).into(),
                status: Phase::Running.label().into(),
                detail: self.status.as_str().into(),
                ..Default::default()
            });
        }
        let mut index = 0;
        for task in tasks {
            let pending = task.phase == Phase::Waiting;
            let can_move_up = pending && index > 0;
            index += usize::from(pending);
            let detail = if task.phase == Phase::Running {
                self.status.as_str()
            } else {
                task.reason.as_str()
            };
            rows.push(AgentQueueItem {
                id: task.id.to_string().into(),
                prompt: cap(&task.text).into(),
                status: task.phase.label().into(),
                detail: detail.into(),
                pending,
                can_move_up,
                can_move_down: pending && index < waiting,
                cancelled: task.phase == Phase::Cancelled,
                recoverable: task.phase.recoverable(),
                drag_token: if pending {
                    format!("queue:{}:{}", self.queue.drag_scope, task.id).into()
                } else {
                    Default::default()
                },
            });
        }
        let model = &self.queue.rows;
        if model.row_count() != rows.len()
            || model.iter().zip(&rows).any(|(old, new)| old.id != new.id)
        {
            model.set_vec(rows);
        } else {
            for (index, row) in rows.into_iter().enumerate() {
                if model.row_data(index).as_ref() != Some(&row) {
                    model.set_row_data(index, row);
                }
            }
        }
    }

    pub(super) fn enqueue_draft(&mut self) {
        if !self.queue_can_enqueue() {
            return;
        }
        let Some(app) = self.ui.upgrade() else { return };
        let global = app.global::<Agent>();
        let text = global.get_draft().to_string();
        if text.trim().is_empty() {
            return;
        }
        let timeline = global
            .get_include_timeline()
            .then(|| self.timeline_input())
            .flatten();
        let timeline_id = timeline.as_ref().map(|value| value.id().to_owned());
        let turn = match self.prepare_turn(timeline, &self.files, String::new()) {
            Ok(turn) => turn,
            Err(error) => {
                self.error = error;
                self.publish();
                return;
            }
        };
        let mut next = self.queue.tasks.clone();
        let Some(id) = next.last_id.checked_add(1) else {
            self.error = t("agent.queueIdExhausted");
            self.publish();
            return;
        };
        next.last_id = id;
        let selection = self.models.selection();
        next.tasks.push(Task {
            id,
            text,
            files: self.files.clone(),
            project: self
                .context
                .as_ref()
                .and_then(|context| context.project.clone()),
            timeline: timeline_id,
            detail: format!("{} This task retains the context selected when it was queued. Inspect the current timeline before editing.", turn.detail),
            label: turn.label,
            bounds: turn.scope.as_ref().map(|scope| scope.bounds().clone()),
            model: selection.model,
            effort: selection.effort,
            phase: Phase::Waiting,
            reason: String::new(),
        });
        if self.queue.change(next) {
            self.show_draft(String::new(), Vec::new());
            self.mark_chat_changed();
        } else {
            self.error = self.queue.error.clone();
        }
        self.publish();
    }

    pub(super) fn set_queue_paused(&mut self, paused: bool) {
        if self.queue_visible_for_chat() && self.allowed() {
            let mut next = self.queue.tasks.clone();
            next.paused = paused;
            self.queue.change(next);
            self.publish();
        }
    }

    pub(super) fn move_queued(&mut self, id: &str, offset: i32) {
        if let Ok(id) = id.parse()
            && self.queue_visible_for_chat()
            && self.allowed()
        {
            let mut next = self.queue.tasks.clone();
            if next.move_waiting(id, offset) {
                self.queue.change(next);
                self.publish();
            }
        }
    }

    fn queue_drop_ids(&self, source: &str, target: &str) -> Option<(u64, u64)> {
        if !self.queue.open
            || !self.queue_visible_for_chat()
            || !self.allowed()
            || self.history.opening.is_some()
        {
            return None;
        }
        let (scope, id) = source.strip_prefix("queue:")?.split_once(':')?;
        let id = id.parse().ok()?;
        let target = target.parse().ok()?;
        if scope.parse::<u64>().ok()? != self.queue.drag_scope || id == target {
            return None;
        }
        let is_waiting = |id| {
            self.queue
                .tasks
                .tasks
                .iter()
                .any(|task| task.id == id && task.phase == Phase::Waiting)
        };
        (is_waiting(id) && is_waiting(target)).then_some((id, target))
    }

    pub(super) fn can_drop_queued(&self, source: &str, target: &str) -> bool {
        self.queue_drop_ids(source, target).is_some()
    }

    pub(super) fn drop_queued(&mut self, source: &str, target: &str, after: bool) -> bool {
        let Some((id, target)) = self.queue_drop_ids(source, target) else {
            return false;
        };
        let mut next = self.queue.tasks.clone();
        if !next.reorder_waiting(id, target, after) {
            return true;
        }
        let saved = self.queue.change(next);
        self.publish();
        saved
    }

    pub(super) fn cancel_queued(&mut self, id: &str) {
        if let Ok(id) = id.parse::<u64>()
            && self.queue_visible_for_chat()
            && self.allowed()
        {
            let mut next = self.queue.tasks.clone();
            if let Some(task) = next
                .tasks
                .iter_mut()
                .find(|task| task.id == id && task.phase == Phase::Waiting)
            {
                task.phase = Phase::Cancelled;
                self.queue.change(next);
                self.publish();
            }
        }
    }

    pub(super) fn restore_cancelled_queued(&mut self, id: &str) {
        if let Ok(id) = id.parse()
            && self.queue_visible_for_chat()
            && self.allowed()
            && self.history.opening.is_none()
        {
            let mut next = self.queue.tasks.clone();
            if next.restore_cancelled(id) {
                self.queue.change(next);
                self.publish();
            }
        }
    }

    pub(super) fn restore_queued(&mut self, id: &str) {
        let Ok(id) = id.parse::<u64>() else { return };
        if !self.queue_visible_for_chat() || !self.allowed() || self.dictation.active() {
            return;
        }
        let Some(task) = self
            .queue
            .tasks
            .tasks
            .iter()
            .find(|task| task.id == id && task.phase.recoverable())
            .cloned()
        else {
            return;
        };
        let Some(app) = self.ui.upgrade() else { return };
        let global = app.global::<Agent>();
        let mut files = self.files.clone();
        for path in task.files {
            if !files.contains(&path) {
                files.push(path);
            }
        }
        if files.len() > MAX_ATTACHMENTS {
            self.queue.error = t("agent.queueRestoreFiles");
            self.publish();
            return;
        }
        let current = global.get_draft();
        let text = if current.is_empty() {
            task.text
        } else {
            format!("{}\n\n{current}", task.text)
        };
        self.show_draft(text, files);
        self.close_queue();
        self.queue.error.clear();
        self.mark_chat_changed();
        self.publish();
    }

    pub(super) fn interrupt_queue(&mut self) {
        if self.queue.tasks.running().is_some() {
            self.queue.interrupted = true;
        }
        if self.queue.tasks.next_index().is_some() {
            self.queue.tasks.paused = true;
            self.queue.flush(true);
        }
    }

    pub(super) fn complete_queue_turn(&mut self, failed: bool) {
        self.queue.current_prompt = None;
        let (phase, reason) = if self.queue.interrupted {
            (Phase::Interrupted, t("agent.queueUnknown"))
        } else if failed {
            (Phase::Failed, self.error.clone())
        } else {
            (Phase::Complete, String::new())
        };
        self.queue.finish(phase, reason);
    }

    pub(super) fn stop_queue(&mut self) {
        self.queue.current_prompt = None;
        self.queue
            .finish(Phase::Interrupted, t("agent.queueUnknown"));
        if self.queue.tasks.next_index().is_some() {
            self.queue.tasks.paused = true;
        }
        self.queue.flush(false);
    }

    pub(super) fn poll_queue(&mut self) {
        let owner = self.queue_key();
        self.queue.sync(owner);
        if !self.queue_visible_for_chat()
            || !self.queue.flush(false)
            || self.queue.tasks.paused
            || self.queue.tasks.running().is_some()
            || self.active_turn.is_some()
            || !self.connected
            || !self.can_submit()
            || self.pause_active()
        {
            return;
        }
        let Some(index) = self.queue.tasks.next_index() else {
            return;
        };
        let mut next = self.queue.tasks.clone();
        let task = next.tasks[index].clone();
        let prepared = self.prepare_queued(&task);
        let turn = match prepared {
            Ok(turn) => turn,
            Err(error) => {
                next.tasks[index].phase = Phase::Failed;
                next.tasks[index].reason = error;
                next.paused = true;
                self.queue.change(next);
                return;
            }
        };
        next.tasks[index].phase = Phase::Running;
        if !self.queue.change(next) {
            return;
        }
        let selection = ModelSelection {
            model: task.model,
            effort: task.effort,
        };
        if !self.send_prepared(task.text, turn, selection, Submission::Queued) {
            let reason = if self.error.is_empty() {
                t("agent.queueCouldNotStart")
            } else {
                self.error.clone()
            };
            self.queue.finish(Phase::Failed, reason);
        }
    }

    fn prepare_queued(&self, task: &Task) -> Result<PreparedTurn, String> {
        let Some(context) = &self.context else {
            return Err(t("agent.queueTargetChanged"));
        };
        if task.project.is_some() && task.project != context.project {
            return Err(t("agent.queueTargetChanged"));
        }
        if let Some(current) = self.saved_edit_bounds()
            && task.bounds.as_ref() != Some(current)
        {
            return Err(t("agent.queueBoundariesChanged"));
        }
        if task.files.iter().any(|path| !path.is_file()) {
            return Err(t("agent.queueFileMoved"));
        }
        let mut scope = Ok(None);
        if task.timeline.is_some() || task.bounds.is_some() {
            scope = Err(t("agent.queueTargetChanged"));
            Shell::with(|shell, _| {
                if let Ok(studio) = shell.studio.try_borrow()
                    && studio.session.is_some()
                    && task
                        .timeline
                        .as_deref()
                        .is_none_or(|id| studio.timeline().id == id)
                {
                    scope = task
                        .bounds
                        .clone()
                        .map(|bounds| LiveEditScope::rebind(bounds, &studio))
                        .transpose();
                }
            });
        }
        Ok(PreparedTurn {
            detail: task.detail.clone(),
            label: task.label.clone(),
            scope: scope?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::storage::Scratch;
    use super::*;

    fn task(id: u64, phase: Phase) -> Task {
        Task {
            id,
            text: format!("Task {id}"),
            files: Vec::new(),
            project: None,
            timeline: None,
            detail: String::new(),
            label: String::new(),
            bounds: None,
            model: None,
            effort: None,
            phase,
            reason: String::new(),
        }
    }

    #[test]
    fn reordering_only_changes_waiting_tasks() {
        let mut tasks = Tasks {
            last_id: 5,
            paused: false,
            tasks: vec![
                task(1, Phase::Running),
                task(2, Phase::Waiting),
                task(3, Phase::Failed),
                task(4, Phase::Waiting),
                task(5, Phase::Waiting),
            ],
        };
        assert!(!tasks.move_waiting(1, 1));
        assert!(!tasks.move_waiting(2, -1));
        assert!(tasks.move_waiting(2, 1));
        assert_eq!(
            tasks.tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
            [1, 4, 3, 2, 5]
        );
        assert_eq!(tasks.next_index(), Some(1));
        assert_eq!(tasks.running(), Some(0));
    }

    #[test]
    fn dropping_before_or_after_an_anchor_shifts_only_waiting_slots() {
        let mut tasks = Tasks {
            last_id: 6,
            paused: true,
            tasks: vec![
                task(1, Phase::Waiting),
                task(2, Phase::Running),
                task(3, Phase::Waiting),
                task(4, Phase::Cancelled),
                task(5, Phase::Waiting),
                task(6, Phase::Waiting),
            ],
        };
        let ids = |tasks: &Tasks| tasks.tasks.iter().map(|task| task.id).collect::<Vec<_>>();
        assert!(!tasks.reorder_waiting(1, 3, false));
        assert!(!tasks.reorder_waiting(1, 1, true));
        assert!(!tasks.reorder_waiting(2, 5, true));
        assert!(!tasks.reorder_waiting(1, 4, false));
        assert!(!tasks.reorder_waiting(99, 6, true));
        assert_eq!(ids(&tasks), [1, 2, 3, 4, 5, 6]);
        assert!(tasks.reorder_waiting(1, 6, true));
        assert_eq!(ids(&tasks), [3, 2, 5, 4, 6, 1]);
        assert!(tasks.reorder_waiting(1, 3, false));
        assert_eq!(ids(&tasks), [1, 2, 3, 4, 5, 6]);
        assert_eq!(tasks.next_index(), Some(0));
        assert_eq!(tasks.running(), Some(1));
        assert!(tasks.paused);
    }

    #[test]
    fn cancelled_task_returns_to_its_slot_with_its_context_after_reload() {
        let dir = Scratch::new();
        let key = QueueKey {
            workspace: dir.path().to_owned(),
            thread: "restore-cancelled".into(),
        };
        let mut cancelled = task(2, Phase::Cancelled);
        cancelled.files = vec![dir.path().join("voice.wav")];
        cancelled.project = Some(dir.path().join("project"));
        cancelled.timeline = Some("timeline-id".into());
        cancelled.model = Some("selected-model".into());
        cancelled.effort = Some("high".into());
        let mut expected = cancelled.clone();
        expected.phase = Phase::Waiting;
        let mut queue = TaskQueue::new(Some(dir.path()));
        queue.sync(Some(key.clone()));
        assert!(queue.change(Tasks {
            last_id: 4,
            paused: true,
            tasks: vec![
                task(1, Phase::Failed),
                cancelled,
                task(3, Phase::Waiting),
                task(4, Phase::Interrupted)
            ],
        }));

        let mut next = queue.tasks.clone();
        assert_eq!(next.next_index(), Some(2));
        assert!(!next.restore_cancelled(1));
        assert!(!next.restore_cancelled(3));
        assert!(!next.restore_cancelled(4));
        assert!(next.restore_cancelled(2));
        assert!(!next.restore_cancelled(2));
        assert!(queue.change(next));

        let mut reopened = TaskQueue::new(Some(dir.path()));
        reopened.sync(Some(key));
        assert_eq!(reopened.tasks.tasks[1], expected);
        assert_eq!(reopened.tasks.next_index(), Some(1));
        assert_eq!(reopened.tasks.last_id, 4);
        assert!(reopened.tasks.paused);
    }

    #[test]
    fn reopening_requires_resuming_and_does_not_replay_inflight_tasks() {
        let dir = Scratch::new();
        let key = QueueKey {
            workspace: dir.path().to_owned(),
            thread: "thread-one".into(),
        };
        let mut prior = TaskQueue::new(Some(dir.path()));
        prior.sync(Some(key.clone()));
        let tasks = Tasks {
            last_id: 2,
            paused: false,
            tasks: vec![task(1, Phase::Running), task(2, Phase::Waiting)],
        };
        prior.write(tasks).unwrap();

        let mut reopened = TaskQueue::new(Some(dir.path()));
        reopened.sync(Some(key.clone()));
        assert!(reopened.tasks.paused);
        assert_eq!(reopened.tasks.tasks[0].phase, Phase::Interrupted);
        assert_eq!(reopened.tasks.running(), None);
        assert_eq!(reopened.tasks.next_index(), Some(1));
        reopened.sync(Some(QueueKey {
            thread: "thread-two".into(),
            ..key
        }));
        assert!(reopened.tasks.tasks.is_empty());
        assert!(!reopened.tasks.paused);
    }

    #[test]
    fn updates_merge_other_chats_and_preserve_a_newer_same_chat_record() {
        let dir = Scratch::new();
        let key = QueueKey {
            workspace: dir.path().to_owned(),
            thread: "first".into(),
        };
        let second = QueueKey {
            thread: "second".into(),
            ..key.clone()
        };
        let mut first = TaskQueue::new(Some(dir.path()));
        first.sync(Some(key.clone()));
        let mut other = TaskQueue::new(Some(dir.path()));
        other.sync(Some(second.clone()));
        let mut stale = TaskQueue::new(Some(dir.path()));
        stale.sync(Some(key.clone()));
        let record = |id| Tasks {
            last_id: id,
            paused: true,
            tasks: vec![task(id, Phase::Waiting)],
        };
        first.write(record(1)).unwrap();
        other.write(record(2)).unwrap();
        assert!(stale.write(record(3)).is_err());

        let mut restored = TaskQueue::new(Some(dir.path()));
        restored.sync(Some(key));
        assert_eq!(restored.tasks.tasks[0].id, 1);
        restored.sync(Some(second));
        assert_eq!(restored.tasks.tasks[0].id, 2);
    }
}
