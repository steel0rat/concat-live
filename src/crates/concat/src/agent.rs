// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Native Codex chat. The event loop owns stable models; a cancellable
//! worker owns app-server IO and its own child process group.

mod dictation;
mod protocol;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{File, read_dir};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use concat_mcp::Backend;
use serde_json::Value;
use slint::{ComponentHandle, Model, VecModel};

use crate::ui::{Agent, AgentApprovalData, AgentMessageData, AgentSavedChatData, App};
use dictation::Dictation;
use protocol::{
    CLIENT_NAME, Context, Effect, Event, HistoryPage, ModelOption, ModelSelection, Protocol,
    SavedChat,
};

const DISPLAY_LIMIT: usize = 32_000;
const WIRE_LIMIT: u64 = 16 * 1024 * 1024;

thread_local! {
    static PANE: RefCell<Option<AgentPane>> = const { RefCell::new(None) };
}

/// Installs the controls; the initial Codex connection waits for the event loop.
pub(crate) fn install(app: &App) {
    let messages = Rc::new(VecModel::default());
    let approvals = Rc::new(VecModel::default());
    let global = app.global::<Agent>();
    global.set_messages(messages.clone().into());
    global.set_approvals(approvals.clone().into());
    global.on_submitted(|| with_pane(AgentPane::submit));
    global.on_interrupt(|| with_pane(AgentPane::interrupt));
    global.on_new_thread(|| with_pane(|pane| pane.change_chat(None, None, String::new())));
    global.on_reconnect(|| with_pane(AgentPane::reconnect));
    global.on_toggle_history(|| with_pane(AgentPane::toggle_history));
    global.on_chat_filter_edited(|query| {
        with_pane(|pane| {
            pane.history.query = query.to_string();
            pane.publish_history_rows();
        })
    });
    global.on_open_chat(|id| with_pane(|pane| pane.open_chat(id.as_str())));
    global.on_load_earlier(|| with_pane(AgentPane::load_earlier));
    global.on_choose_workspace(|| with_pane(AgentPane::choose_workspace));
    global.on_model_selected(|index| with_pane(|pane| pane.select_model(index)));
    global.on_effort_selected(|index| with_pane(|pane| pane.select_effort(index)));
    global.on_dictation_start(|| with_pane(AgentPane::start_dictation));
    global.on_dictation_stop(|| {
        with_pane(|pane| {
            pane.dictation.stop();
            pane.publish();
        })
    });
    global.on_dictation_cancel(|| {
        with_pane(|pane| {
            pane.dictation.cancel();
            pane.publish();
        })
    });
    global.on_approval(|id, accepted| with_pane(|pane| pane.approve(id.as_str(), accepted)));
    global.on_draft_edited(|text| {
        with_pane(|pane| {
            if let Some(app) = pane.ui.upgrade() {
                app.global::<Agent>().set_draft(text);
            }
        })
    });
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(30),
        || with_pane(AgentPane::poll),
    );
    let context = initial_workspace().map(|path| Context {
        path,
        project: None,
        name: String::new(),
        epoch: crate::host::project_epoch(),
    });
    if let Some(context) = &context {
        global.set_workspace_path(context.path.to_string_lossy().as_ref().into());
    }
    let status = if context.is_some() {
        "Ready · describe a project or an edit"
    } else {
        "Choose a working folder to start"
    };
    PANE.with_borrow_mut(|slot| {
        *slot = Some(AgentPane {
            ui: app.as_weak(),
            messages,
            approvals,
            rows: HashMap::new(),
            context,
            generation: 0,
            next_message: 0,
            thread: None,
            worker: None,
            connected: false,
            history: ChatHistory::default(),
            connect_pending: true,
            models: Models::default(),
            busy: false,
            maintenance: false,
            unsent_draft: None,
            status: status.into(),
            error: String::new(),
            dictation: Dictation::default(),
            _timer: timer,
        });
    });
    with_pane(|pane| pane.publish());
}

/// Publishes the visible project without replacing the workspace conversation.
pub(crate) fn set_context(path: Option<PathBuf>, name: &str, epoch: u64) {
    with_pane(|pane| {
        if let Some(app) = pane.ui.upgrade() {
            app.global::<Agent>().set_project_name(name.into());
            app.global::<Agent>().set_has_project(path.is_some());
        }
        if pane.context.as_ref().is_some_and(|previous| {
            previous.project.is_some() && (previous.project != path || previous.epoch != epoch)
        }) {
            pane.dictation.cancel();
        }
        let Some(context) = &mut pane.context else {
            return;
        };
        let changed = context.project != path || context.epoch != epoch;
        context.project = path;
        context.name = name.to_owned();
        context.epoch = epoch;
        if !changed {
            return;
        }
        let updated = context.clone();
        let note = match &updated.project {
            Some(path) => format!("Project: {}\n{}", updated.name, path.display()),
            None => format!(
                "No project is open. Working folder: {}",
                updated.path.display()
            ),
        };
        if let Some(worker) = &pane.worker
            && worker
                .commands
                .send(Control::ContextChanged(updated))
                .is_err()
        {
            pane.error = "Codex disconnected. Reconnect to continue this conversation.".into();
            pane.stop();
        }
        if !pane.busy {
            pane.status = "Ready · uses your Codex settings and sign-in".into();
        }
        pane.local_message("Context", note);
        pane.publish();
    });
}

fn initial_workspace() -> Option<PathBuf> {
    if let Some(roots) = std::env::var_os("CONCAT_MCP_ROOTS")
        && let Some(path) =
            std::env::split_paths(&roots).find(|path| path.is_absolute() && path.is_dir())
    {
        return path.canonicalize().ok();
    }
    let home = PathBuf::from(crate::studio::home_folder(""));
    (home.is_absolute() && home.is_dir())
        .then(|| home.canonicalize().ok())
        .flatten()
}

/// Stops our child group before the desktop's immediate exit, which
/// bypasses thread-local destructors. Repeated calls are harmless.
pub(crate) fn shutdown() {
    let _ = PANE.try_with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut()
            && let Some(mut pane) = slot.take()
        {
            pane.stop();
        }
    });
}

/// Reserves an idle chat until maintenance has finished.
pub(crate) fn pause_for_update(restarting: bool) -> bool {
    PANE.with_borrow_mut(|slot| {
        let Some(pane) = slot else { return true };
        if pane.busy || pane.dictation.active() || (restarting && pane.has_drafts()) {
            return false;
        }
        pane.maintenance = true;
        pane.publish();
        true
    })
}

/// Releases the chat when an update has completed or failed.
pub(crate) fn resume_after_update() {
    with_pane(|pane| {
        pane.maintenance = false;
        pane.publish();
    });
}

/// Dictation and an unsent draft also keep the editor from restarting.
pub(crate) fn busy_for_update(restarting: bool) -> bool {
    PANE.with_borrow(|slot| {
        slot.as_ref().is_some_and(|pane| {
            pane.busy
                || pane.maintenance
                || pane.dictation.active()
                || (restarting && pane.has_drafts())
        })
    })
}

fn with_pane(work: impl FnOnce(&mut AgentPane)) {
    PANE.with_borrow_mut(|slot| {
        if let Some(pane) = slot {
            work(pane);
        }
    });
}

#[derive(Default)]
struct Models {
    catalogue: Vec<ModelOption>,
    current: ModelSelection,
    explicit: bool,
}

impl Models {
    fn current_index(&self) -> Option<usize> {
        self.catalogue
            .iter()
            .position(|option| Some(option.model.as_str()) == self.current.model.as_deref())
    }

    fn current_model(&self) -> Option<&ModelOption> {
        self.current_index()
            .and_then(|index| self.catalogue.get(index))
    }

    fn select_model(&mut self, index: usize) {
        if let Some(option) = self.catalogue.get(index)
            && self.current.model.as_deref() != Some(option.model.as_str())
        {
            self.current = ModelSelection {
                model: Some(option.model.clone()),
                effort: option.default_effort.clone(),
            };
            self.explicit = true;
        }
    }

    fn select_effort(&mut self, index: usize) {
        if let Some(effort) = self
            .current_model()
            .and_then(|model| model.efforts.get(index))
        {
            self.current.effort = Some(effort.clone());
            self.explicit = true;
        }
    }

    fn selection(&self) -> ModelSelection {
        if self.explicit {
            self.current.clone()
        } else {
            ModelSelection::default()
        }
    }
}

#[derive(PartialEq, Default)]
enum ChatList {
    #[default]
    Idle,
    Waiting,
    Reading,
}

struct ChatOpening {
    draft_from: Option<String>,
    title: String,
}

#[derive(Default)]
struct ChatHistory {
    saved: Vec<SavedChat>,
    drafts: HashMap<String, String>,
    opening: Option<ChatOpening>,
    list: ChatList,
    open: bool,
    query: String,
    error: String,
    title: String,
    older: Option<String>,
    reading_older: bool,
}

impl ChatHistory {
    fn move_draft(&mut self, previous: Option<&str>, draft: String, selected: &str) -> String {
        if let Some(previous) = previous
            && !draft.is_empty()
        {
            self.drafts.insert(previous.to_owned(), draft);
        }
        self.drafts.remove(selected).unwrap_or_default()
    }
}

struct AgentPane {
    ui: slint::Weak<App>,
    messages: Rc<VecModel<AgentMessageData>>,
    approvals: Rc<VecModel<AgentApprovalData>>,
    rows: HashMap<String, usize>,
    context: Option<Context>,
    generation: u64,
    next_message: u64,
    thread: Option<String>,
    worker: Option<Worker>,
    connected: bool,
    history: ChatHistory,
    connect_pending: bool,
    models: Models,
    busy: bool,
    maintenance: bool,
    unsent_draft: Option<(String, String)>,
    status: String,
    error: String,
    dictation: Dictation,
    _timer: slint::Timer,
}

impl AgentPane {
    fn enabled(&self) -> bool {
        self.ui
            .upgrade()
            .is_some_and(|app| app.global::<Agent>().get_enabled())
    }

    fn allowed(&self) -> bool {
        self.context.is_some() && self.enabled() && !self.maintenance
    }

    fn publish(&self) {
        if let Some(app) = self.ui.upgrade() {
            let global = app.global::<Agent>();
            global.set_busy(self.busy);
            global.set_maintenance(self.maintenance);
            global.set_status(self.status.as_str().into());
            global.set_error(self.error.as_str().into());
            global.set_connected(self.connected);
            global.set_history_open(self.history.open);
            global.set_history_loading(self.history.list != ChatList::Idle);
            global.set_history_error(self.history.error.as_str().into());
            global.set_chat_opening(self.history.opening.is_some());
            global.set_chat_title(self.history.title.as_str().into());
            global.set_has_earlier(self.history.older.is_some());
            global.set_earlier_loading(self.history.reading_older);
            self.dictation.publish(&global);
        }
    }

    fn has_drafts(&self) -> bool {
        !self.history.drafts.is_empty()
            || self
                .ui
                .upgrade()
                .is_some_and(|app| !app.global::<Agent>().get_draft().is_empty())
    }

    fn publish_history_rows(&self) {
        let Some(app) = self.ui.upgrade() else { return };
        let global = app.global::<Agent>();
        let query = self.history.query.trim().to_lowercase();
        let mut rows = Vec::new();
        let mut add = |id: &str, title: &str, cwd: &str| {
            let title = chat_title(title);
            if !title.to_lowercase().contains(&query) && !cwd.to_lowercase().contains(&query) {
                return;
            }
            rows.push(AgentSavedChatData {
                id: id.into(),
                title: title.into(),
                workspace: cwd.into(),
                active: self.thread.as_deref() == Some(id),
                has_draft: self.history.drafts.contains_key(id),
            });
        };
        let mut drafts: Vec<_> = self
            .history
            .drafts
            .iter()
            .filter(|(id, _)| !self.history.saved.iter().any(|entry| entry.id == **id))
            .collect();
        drafts.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        for (id, draft) in drafts {
            add(id, draft, "");
        }
        for chat in &self.history.saved {
            add(&chat.id, &chat.title, &chat.cwd);
        }
        global.set_saved_chats(Rc::new(VecModel::from(rows)).into());
    }

    fn toggle_history(&mut self) {
        if !self.allowed() || self.busy || self.dictation.active() {
            return;
        }
        self.history.open = !self.history.open;
        if self.history.open {
            self.history.error.clear();
            if self.history.list == ChatList::Idle {
                self.history.list = ChatList::Waiting;
            }
            self.history.query.clear();
            if let Some(app) = self.ui.upgrade() {
                app.global::<Agent>().set_chat_query("".into());
            }
            self.publish_history_rows();
            if let Err(error) = self.start() {
                self.history.list = ChatList::Idle;
                self.history.error = error;
            }
        }
        self.publish();
    }

    fn open_chat(&mut self, id: &str) {
        if !self.history.open || !self.allowed() || self.busy || self.dictation.active() {
            return;
        }
        if self.thread.as_deref() == Some(id) {
            self.history.open = false;
            self.publish();
            return;
        }
        if let Some(chat) = self.history.saved.iter().find(|chat| chat.id == id) {
            self.change_chat(Some(id.to_owned()), None, chat_title(&chat.title));
        } else if let Some(draft) = self.history.drafts.get(id) {
            self.change_chat(None, Some(id.to_owned()), chat_title(draft));
        }
    }

    fn change_chat(&mut self, thread: Option<String>, draft_from: Option<String>, title: String) {
        if !self.allowed() || self.busy || self.dictation.active() {
            return;
        }
        if self.connected {
            if self.send_control(Control::ChangeChat(thread)).is_err() {
                self.error = "Codex disconnected. Reconnect to open another chat.".into();
                self.stop();
                self.publish();
                return;
            }
        } else if thread.is_none() && self.worker.is_none() {
            if let Err(error) = self.connect(None) {
                self.error = error;
                self.publish();
                return;
            }
        } else {
            return;
        }
        self.history.opening = Some(ChatOpening { draft_from, title });
        self.busy = true;
        self.status = "Opening chat…".into();
        self.error.clear();
        self.publish();
    }

    fn show_chat(&mut self, id: String, page: HistoryPage) {
        let Some(opening) = self.history.opening.take() else {
            return;
        };
        if let Some(app) = self.ui.upgrade() {
            let global = app.global::<Agent>();
            let draft = self.history.move_draft(
                self.thread.as_deref(),
                global.get_draft().to_string(),
                opening.draft_from.as_deref().unwrap_or(&id),
            );
            global.set_draft(draft.into());
        }
        self.thread = Some(id);
        self.history.title = opening.title;
        self.history.open = false;
        self.rows.clear();
        self.messages.set_vec(Vec::new());
        self.show_history_page(page);
        self.clear_approvals();
        self.clear_models("Loading models…");
        self.publish_history_rows();
    }

    fn show_history_page(&mut self, page: HistoryPage) {
        let mut rows = Vec::new();
        for message in page.messages {
            let key = format!("{}:{}", self.generation, message.id);
            if !self.rows.contains_key(&key)
                && !rows
                    .iter()
                    .any(|row: &AgentMessageData| row.id.as_str() == key)
            {
                rows.push(AgentMessageData {
                    id: key.into(),
                    role: message.role.into(),
                    text: cap(&message.text).into(),
                    streaming: false,
                    failed: false,
                });
            }
        }
        if !rows.is_empty() {
            rows.extend(self.messages.iter());
            self.rows = rows
                .iter()
                .enumerate()
                .map(|(i, row)| (row.id.to_string(), i))
                .collect();
            self.messages.set_vec(rows);
        }
        self.history.older = page.older;
        self.history.reading_older = false;
    }

    fn load_earlier(&mut self) {
        if !self.allowed()
            || self.busy
            || self.history.open
            || self.history.reading_older
            || self.dictation.active()
        {
            return;
        }
        if let Some(cursor) = self.history.older.clone() {
            if self.send_control(Control::OlderMessages(cursor)).is_ok() {
                self.history.reading_older = true;
                self.error.clear();
            } else {
                self.error = "Reconnect to load earlier messages.".into();
            }
            self.publish();
        }
    }

    fn send_control(&self, command: Control) -> Result<(), ()> {
        self.worker
            .as_ref()
            .ok_or(())?
            .commands
            .send(command)
            .map_err(|_| ())
    }

    fn publish_models(&self, placeholder: &str) {
        let Some(app) = self.ui.upgrade() else { return };
        let global = app.global::<Agent>();
        let labels: Vec<slint::SharedString> = self
            .models
            .catalogue
            .iter()
            .map(|model| cap(&model.label).into())
            .collect();
        global.set_model_options(Rc::new(VecModel::from(labels)).into());
        global.set_model_index(self.models.current_index().map_or(-1, |index| index as i32));
        global.set_model_placeholder(
            self.models
                .current
                .model
                .as_deref()
                .unwrap_or(placeholder)
                .into(),
        );
        self.publish_efforts(&global);
    }

    fn publish_efforts(&self, global: &Agent<'_>) {
        let efforts = self
            .models
            .current_model()
            .map_or(&[][..], |model| &model.efforts);
        let selected = self.models.current.effort.as_deref();
        let labels: Vec<slint::SharedString> = efforts
            .iter()
            .map(|effort| effort_label(effort).into())
            .collect();
        global.set_effort_options(Rc::new(VecModel::from(labels)).into());
        global.set_effort_index(
            efforts
                .iter()
                .position(|effort| Some(effort.as_str()) == selected)
                .map_or(-1, |index| index as i32),
        );
        global.set_effort_placeholder(selected.map_or("Default", effort_label).into());
    }

    fn select_model(&mut self, index: i32) {
        if self.allowed()
            && !self.busy
            && !self.dictation.active()
            && let Ok(index) = usize::try_from(index)
        {
            self.models.select_model(index);
            if let Some(app) = self.ui.upgrade() {
                let global = app.global::<Agent>();
                global
                    .set_model_index(self.models.current_index().map_or(-1, |index| index as i32));
                self.publish_efforts(&global);
            }
        }
    }

    fn select_effort(&mut self, index: i32) {
        if self.allowed()
            && !self.busy
            && !self.dictation.active()
            && let Ok(index) = usize::try_from(index)
        {
            self.models.select_effort(index);
            if let Some(app) = self.ui.upgrade() {
                self.publish_efforts(&app.global::<Agent>());
            }
        }
    }

    fn clear_models(&mut self, placeholder: &str) {
        self.models = Models::default();
        self.publish_models(placeholder);
    }

    fn clear_approvals(&self) {
        while self.approvals.row_count() > 0 {
            self.approvals.remove(self.approvals.row_count() - 1);
        }
    }

    fn restore_unsent(&mut self) {
        let Some((_, text)) = self.unsent_draft.take() else {
            return;
        };
        if let Some(app) = self.ui.upgrade() {
            let global = app.global::<Agent>();
            if global.get_draft().is_empty() {
                global.set_draft(text.into());
            }
        }
    }

    fn stop(&mut self) {
        self.restore_unsent();
        self.dictation.cancel();
        self.worker.take();
        self.connected = false;
        self.history.list = ChatList::Idle;
        self.history.opening = None;
        self.history.reading_older = false;
        self.clear_models("Connect to Codex");
        self.finish_streaming(true);
        self.generation = self.generation.wrapping_add(1);
        self.busy = false;
        self.clear_approvals();
    }

    fn finish_streaming(&self, failed: bool) {
        let prefix = format!("{}:", self.generation);
        for row in 0..self.messages.row_count() {
            if let Some(mut message) = self.messages.row_data(row)
                && message.streaming
                && message.id.starts_with(&prefix)
            {
                message.streaming = false;
                message.failed |= failed;
                self.messages.set_row_data(row, message);
            }
        }
    }

    fn message(
        &mut self,
        id: &str,
        role: &str,
        text: &str,
        append: bool,
        streaming: bool,
        failed: bool,
    ) {
        let key = format!("{}:{id}", self.generation);
        if let Some(&row) = self.rows.get(&key) {
            if let Some(mut message) = self.messages.row_data(row) {
                let text = if append {
                    format!("{}{text}", message.text)
                } else {
                    text.to_owned()
                };
                message.text = cap(&text).into();
                message.streaming = streaming;
                message.failed = failed;
                self.messages.set_row_data(row, message);
            }
        } else {
            self.rows.insert(key.clone(), self.messages.row_count());
            self.messages.push(AgentMessageData {
                id: key.into(),
                role: role.into(),
                text: cap(text).into(),
                streaming,
                failed,
            });
        }
    }

    fn local_message(&mut self, role: &str, text: String) -> String {
        self.next_message += 1;
        let id = format!("local-{}", self.next_message);
        self.message(&id, role, &text, false, false, false);
        id
    }

    fn start(&mut self) -> Result<(), String> {
        self.connect_pending = false;
        if self.worker.is_some() {
            return Ok(());
        }
        self.connect(self.thread.clone())
    }

    fn connect(&mut self, resume: Option<String>) -> Result<(), String> {
        let context = self
            .context
            .clone()
            .ok_or("Choose a working folder before starting Codex")?;
        self.worker = Some(Worker::start(context, resume, self.generation)?);
        self.status = "Connecting to Codex…".into();
        self.clear_models("Loading models…");
        self.error.clear();
        Ok(())
    }

    fn start_dictation(&mut self) {
        if !self.enabled()
            || self.maintenance
            || self.dictation.active()
            || self.history.opening.is_some()
        {
            return;
        }
        let Some(app) = self.ui.upgrade() else { return };
        let language = app.global::<Agent>().get_dictation_language().to_string();
        self.dictation.start(&language);
        self.publish();
    }

    fn choose_workspace(&mut self) {
        if self.busy
            || self.dictation.active()
            || self
                .context
                .as_ref()
                .is_some_and(|context| context.project.is_some())
        {
            return;
        }
        let current = self
            .context
            .as_ref()
            .map(|context| context.path.to_string_lossy().into_owned())
            .unwrap_or_default();
        crate::host::after_dialog(
            move || crate::platform::pick_folder("Choose a working folder for Codex", &current),
            |studio, _, _, folder| {
                studio.handle(crate::panes::Msg::Settings(
                    crate::panes::settings::SettingsMsg::LiveWorkspaceChanged(folder),
                ));
                let selected = studio.settings.live_workspace().map(Path::to_path_buf);
                let error = studio.settings.server_error.clone();
                with_pane(|pane| {
                    if !error.is_empty() {
                        pane.error = error;
                        pane.publish();
                        return;
                    }
                    let Some(path) = selected else { return };
                    pane.stop();
                    pane.context = Some(Context {
                        path: path.clone(),
                        project: None,
                        name: String::new(),
                        epoch: crate::host::project_epoch(),
                    });
                    pane.connect_pending = true;
                    pane.error.clear();
                    pane.status = "Ready · describe the project to create".into();
                    if let Some(app) = pane.ui.upgrade() {
                        app.global::<Agent>()
                            .set_workspace_path(path.to_string_lossy().as_ref().into());
                    }
                    pane.local_message("Context", format!("Working folder: {}", path.display()));
                    pane.publish();
                });
            },
        );
    }

    fn submit(&mut self) {
        if !self.allowed() || self.busy || self.dictation.active() || self.history.open {
            return;
        }
        let Some(app) = self.ui.upgrade() else { return };
        let text = app.global::<Agent>().get_draft().to_string();
        if text.trim().is_empty() {
            return;
        }
        if let Err(error) = self.start() {
            self.error = error;
            self.publish();
            return;
        }
        let id = self.local_message("You", text.clone());
        let unsent = (id.clone(), text.clone());
        self.message(&id, "You", &text, false, true, false);
        if let Some(worker) = &self.worker
            && worker
                .commands
                .send(Control::Send {
                    text,
                    id,
                    selection: self.models.selection(),
                    context: self
                        .context
                        .clone()
                        .expect("a started worker has a workspace"),
                })
                .is_err()
        {
            self.error = "Codex disconnected. Reconnect before sending again.".into();
            self.finish_streaming(true);
            self.worker.take();
            self.connected = false;
            self.clear_models("Disconnected");
            self.publish();
            return;
        }
        self.unsent_draft = Some(unsent);
        app.global::<Agent>().set_draft("".into());
        self.busy = true;
        self.error.clear();
        self.publish();
    }

    fn interrupt(&mut self) {
        if let Some(worker) = &self.worker {
            let _ = worker.commands.send(Control::Interrupt);
            self.status = "Interrupting…".into();
            self.publish();
        }
    }

    fn reconnect(&mut self) {
        if !self.allowed() || self.busy || self.dictation.active() {
            return;
        }
        self.stop();
        if self.history.open {
            self.history.list = ChatList::Waiting;
            self.history.error.clear();
        }
        if let Err(error) = self.start() {
            self.history.list = ChatList::Idle;
            self.error = error;
        } else {
            self.busy = true;
        }
        self.publish();
    }

    fn approve(&mut self, id: &str, accepted: bool) {
        if !self.allowed() {
            return;
        }
        if let Some(worker) = &self.worker
            && worker
                .commands
                .send(Control::Approve {
                    id: id.to_owned(),
                    accepted,
                })
                .is_err()
        {
            self.error = "Approval was not sent: Codex disconnected.".into();
            self.publish();
        }
    }

    fn resolve(&self, id: &str) {
        if let Some(row) = (0..self.approvals.row_count()).find(|&row| {
            self.approvals
                .row_data(row)
                .is_some_and(|approval| approval.id.as_str() == id)
        }) {
            self.approvals.remove(row);
        }
    }

    fn poll(&mut self) {
        if !self.enabled() || (self.context.is_none() && self.worker.is_some()) {
            self.stop();
            return;
        }
        if self.connect_pending
            && self.allowed()
            && let Err(error) = self.start()
        {
            self.error = error;
        }
        if self.history.list == ChatList::Waiting && self.connected {
            if self.send_control(Control::ListChats).is_ok() {
                self.history.list = ChatList::Reading;
            } else {
                self.history.list = ChatList::Idle;
                self.history.error = "Reconnect to load chat history.".into();
            }
        }
        if let Some(transcript) = self.dictation.poll()
            && let Some(app) = self.ui.upgrade()
        {
            let global = app.global::<Agent>();
            let draft = dictation::append_transcript(global.get_draft().as_str(), &transcript);
            global.set_draft(draft.into());
        }
        let events: Vec<_> = self
            .worker
            .as_ref()
            .map(|worker| worker.events.try_iter().take(512).collect())
            .unwrap_or_default();
        for (generation, event) in events {
            if generation != self.generation {
                continue;
            }
            match event {
                Event::State { busy, status } => {
                    self.busy =
                        busy || self.unsent_draft.is_some() || self.history.opening.is_some();
                    self.status = status;
                }
                Event::Error(error) => self.error = error,
                Event::Fatal(error) => {
                    self.stop();
                    self.error = error;
                    self.status = "Disconnected · reconnect to continue".into();
                    if self.history.open {
                        self.history.error = "Reconnect to load chat history.".into();
                    }
                    self.clear_models("Disconnected");
                }
                Event::Thread(id) => {
                    self.connected = true;
                    if self.history.opening.is_some() {
                        self.show_chat(id, HistoryPage::default());
                    } else {
                        self.thread = Some(id);
                    }
                }
                Event::SavedChats { chats, incomplete } => {
                    self.history.saved = chats;
                    self.history.list = ChatList::Idle;
                    self.history.error = if incomplete {
                        "Some chats could not be loaded.".into()
                    } else {
                        String::new()
                    };
                    self.publish_history_rows();
                }
                Event::ChatListFailed(error) => {
                    self.history.list = ChatList::Idle;
                    self.history.error = error;
                }
                Event::ChatOpened { id, page } => self.show_chat(id, page),
                Event::ChatOpenFailed => {
                    self.history.opening = None;
                    self.busy = false;
                    self.status = "Ready".into();
                    self.error = "Could not open the chat. Try again or reconnect.".into();
                }
                Event::OlderMessages(page) => self.show_history_page(page),
                Event::OlderMessagesFailed => {
                    self.history.reading_older = false;
                    self.error = "Could not load earlier messages. Try again.".into();
                }
                Event::Models { current, options } => {
                    self.models = Models {
                        catalogue: options,
                        current,
                        explicit: false,
                    };
                    self.publish_models("Models unavailable");
                }
                Event::ModelsUnavailable => self.clear_models("Models unavailable"),
                Event::Message {
                    id,
                    role,
                    text,
                    append,
                    streaming,
                    failed,
                } => self.message(&id, &role, &text, append, streaming, failed),
                Event::Sent(id) => {
                    if self
                        .unsent_draft
                        .as_ref()
                        .is_some_and(|(pending, _)| *pending == id)
                        && let Some((_, text)) = self.unsent_draft.take()
                        && self.history.title.is_empty()
                    {
                        self.history.title = chat_title(&text);
                    }
                    let key = format!("{}:{id}", self.generation);
                    if let Some(&row) = self.rows.get(&key)
                        && let Some(mut message) = self.messages.row_data(row)
                    {
                        message.streaming = false;
                        self.messages.set_row_data(row, message);
                    }
                }
                Event::Approval { id, title, detail } => {
                    self.resolve(&id);
                    self.approvals.push(AgentApprovalData {
                        id: id.into(),
                        title: title.into(),
                        detail: cap(&detail).into(),
                    });
                    self.status = "Waiting for your approval".into();
                }
                Event::Resolved(id) => self.resolve(&id),
                Event::Finished { failed } => {
                    self.restore_unsent();
                    self.finish_streaming(failed);
                    self.clear_approvals();
                }
            }
        }
        self.publish();
    }
}

enum Control {
    Send {
        text: String,
        id: String,
        selection: ModelSelection,
        context: Context,
    },
    ContextChanged(Context),
    ListChats,
    ChangeChat(Option<String>),
    OlderMessages(String),
    Interrupt,
    Approve {
        id: String,
        accepted: bool,
    },
    Stop,
}

struct Worker {
    commands: mpsc::Sender<Control>,
    events: mpsc::Receiver<(u64, Event)>,
    stopped: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Worker {
    fn start(context: Context, resume: Option<String>, generation: u64) -> Result<Self, String> {
        let (commands, controls) = mpsc::channel();
        let (publish, events) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let worker_stop = stopped.clone();
        let worker_child = child.clone();
        std::thread::Builder::new()
            .name("codex-chat".into())
            .spawn(move || {
                let result = run_worker(
                    &context,
                    resume,
                    &controls,
                    &publish,
                    generation,
                    &worker_stop,
                    worker_child,
                );
                if !worker_stop.load(Ordering::Acquire)
                    && let Err(error) = result
                {
                    let _ = publish.send((generation, Event::Fatal(error)));
                }
            })
            .map_err(|error| format!("Cannot start the Codex worker: {error}"))?;
        Ok(Self {
            commands,
            events,
            stopped,
            child,
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.commands.send(Control::Stop);
        if let Ok(mut child) = self.child.lock()
            && let Some(child) = child.as_mut()
        {
            terminate(child);
        }
    }
}

fn terminate(child: &mut Child) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let pid = child.id() as libc::pid_t;
        // SAFETY: the Child is still held under our mutex. All APIs take
        // integers; the editor's own group is explicitly excluded.
        unsafe {
            if pid > 1 && libc::getpgid(pid) == pid && libc::getpgrp() != pid {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

struct Process {
    child: Arc<Mutex<Option<Child>>>,
    input: Option<ChildStdin>,
    lines: mpsc::Receiver<Result<Value, String>>,
    readers: Vec<JoinHandle<()>>,
}

impl Process {
    fn spawn(context: &Context, child: Arc<Mutex<Option<Child>>>) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("Cannot find the Concat executable: {error}"))?;
        let mut command = Command::new(codex_executable());
        command
            .args(app_server_args(&executable)?)
            .current_dir(&context.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let spawned = command.spawn().map_err(|error| format!("Cannot start Codex: {error}. Install the Codex CLI and run codex login, then reconnect."))?;
        let (input, output, errors) = {
            let mut held = child
                .lock()
                .map_err(|_| "The Codex process lock was poisoned")?;
            *held = Some(spawned);
            let child = held.as_mut().ok_or("The Codex process is missing")?;
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        };
        let (send, lines) = mpsc::channel();
        let mut process = Self {
            child,
            input,
            lines,
            readers: Vec::new(),
        };
        if process.input.is_none() {
            return Err("Codex did not open standard input".into());
        }
        let output = output.ok_or("Codex did not open standard output")?;
        let errors = errors.ok_or("Codex did not open standard error")?;
        process.readers.push(
            std::thread::Builder::new()
                .name("codex-events".into())
                .spawn(move || {
                    let mut reader = BufReader::new(output);
                    loop {
                        let mut line = String::new();
                        let read = Read::by_ref(&mut reader)
                            .take(WIRE_LIMIT + 1)
                            .read_line(&mut line);
                        let result = match read {
                            Ok(0) => {
                                Err("Codex closed its event stream. Reconnect to continue."
                                    .to_owned())
                            }
                            Ok(size) if size as u64 > WIRE_LIMIT => {
                                Err("A Codex event exceeded the 16 MiB protocol limit.".to_owned())
                            }
                            Ok(_) => serde_json::from_str(&line)
                                .map_err(|_| "Codex sent an invalid JSON event.".to_owned()),
                            Err(error) => Err(format!("Cannot read Codex events: {error}")),
                        };
                        let failed = result.is_err();
                        if send.send(result).is_err() || failed {
                            break;
                        }
                    }
                })
                .map_err(|error| format!("Cannot start the Codex event reader: {error}"))?,
        );
        process.readers.push(
            std::thread::Builder::new()
                .name("codex-stderr".into())
                .spawn(move || {
                    // Drain provider diagnostics without exposing credentials or
                    // headers from third-party logs in the editor's logs or UI.
                    let _ = std::io::copy(&mut BufReader::new(errors), &mut std::io::sink());
                })
                .map_err(|error| format!("Cannot start the Codex diagnostic reader: {error}"))?,
        );
        Ok(process)
    }

    fn write(&mut self, value: &Value) -> Result<(), String> {
        let input = self.input.as_mut().ok_or("Codex is disconnected")?;
        serde_json::to_writer(&mut *input, value)
            .map_err(|error| format!("Cannot send a Codex request: {error}"))?;
        input
            .write_all(b"\n")
            .and_then(|()| input.flush())
            .map_err(|error| format!("Cannot flush a Codex request: {error}"))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.input.take();
        let child = self.child.lock().ok().and_then(|mut held| held.take());
        if let Some(mut child) = child {
            terminate(&mut child);
            let _ = child.wait();
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

fn codex_executable() -> PathBuf {
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|folder| folder.join(name))
            .find(|path| path.is_file())
    {
        return found;
    }
    for path in ["/opt/homebrew/bin/codex", "/usr/local/bin/codex"] {
        if Path::new(path).is_file() {
            return path.into();
        }
    }
    name.into()
}

fn app_server_args(executable: &Path) -> Result<Vec<String>, String> {
    let path = executable
        .to_str()
        .ok_or("The Concat executable path is not valid UTF-8")?;
    let quoted = serde_json::to_string(path)
        .map_err(|error| format!("Cannot encode the Concat executable path: {error}"))?;
    Ok(vec![
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
        "-c".into(),
        format!(
            "mcp_servers.concat={{command={quoted},args=[\"--mcp\",\"window\"],enabled=true,required=true}}"
        ),
    ])
}

fn check_live(context: &Context) -> Result<(), String> {
    if context.epoch != crate::host::project_epoch() {
        return Err(
            "The visible project changed before this request. Send your request again.".into(),
        );
    }
    let mut window = concat_mcp::Window::find()?;
    let version = window
        .call(concat_api::Request::Version)
        .map_err(|error| error.message)?;
    if !version["capabilities"]
        .as_array()
        .is_some_and(|names| names.iter().any(|name| name == "live"))
    {
        return Err("The Concat API does not support Live mode. Start Concat with --live.".into());
    }
    let path = context.project.as_ref().unwrap_or(&context.path);
    let read = window.call(concat_api::Request::ProjectGet {
        path: path.to_string_lossy().into_owned(),
    });
    match read {
        Ok(_) => {}
        Err(error)
            if context.project.is_none()
                && matches!(error.code, concat_api::ErrorCode::NotOpen) => {}
        Err(error) => return Err(error.message),
    }
    if context.epoch != crate::host::project_epoch() {
        return Err("The visible project changed while connecting.".into());
    }
    Ok(())
}

fn run_worker(
    context: &Context,
    resume: Option<String>,
    controls: &mpsc::Receiver<Control>,
    publish: &mpsc::Sender<(u64, Event)>,
    generation: u64,
    stopped: &AtomicBool,
    child: Arc<Mutex<Option<Child>>>,
) -> Result<(), String> {
    let mut context = context.clone();
    check_live(&context)?;
    if stopped.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut process = Process::spawn(&context, child)?;
    let mut protocol = Protocol::new(context.clone(), resume);
    process.write(&protocol.initialize())?;
    while !stopped.load(Ordering::Acquire) {
        let mut effects = Vec::new();
        while let Ok(control) = controls.try_recv() {
            match control {
                Control::Stop => return Ok(()),
                Control::Send {
                    text,
                    id,
                    selection,
                    context: updated,
                } => {
                    context = updated;
                    check_live(&context)?;
                    effects.extend(protocol.update_context(context.clone()));
                    protocol.selection = selection;
                    effects.extend(protocol.submit(text, id)?);
                }
                Control::ContextChanged(updated) => {
                    context = updated;
                    effects.extend(protocol.update_context(context.clone()));
                }
                Control::ListChats => match saved_chat_ids(&context.path) {
                    Ok(ids) => effects.extend(protocol.list_chats(ids)),
                    Err(error) => effects.push(Effect::Show(Event::ChatListFailed(error))),
                },
                Control::ChangeChat(thread) => effects.extend(protocol.change_chat(thread)?),
                Control::OlderMessages(cursor) => effects.extend(protocol.older_messages(cursor)),
                Control::Interrupt => {
                    if !protocol.connected() {
                        return Err("Connection cancelled before the conversation started.".into());
                    }
                    effects.extend(protocol.interrupt());
                }
                Control::Approve { id, accepted } => {
                    effects.extend(protocol.approve(&id, accepted))
                }
            }
        }
        match process.lines.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(value)) => effects.extend(protocol.receive(value)?),
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("The Codex event reader stopped.".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        effects.extend(protocol.expire_model_list());
        if protocol.timed_out() {
            return Err("Codex did not answer a protocol request within two minutes. Reconnect to continue.".into());
        }
        for effect in effects {
            if stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            match effect {
                Effect::Write(value) => process.write(&value)?,
                Effect::Show(event) => {
                    if publish.send((generation, event)).is_err() {
                        return Ok(());
                    }
                }
                Effect::Stop(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn saved_chat_ids(workspace: &Path) -> Result<Vec<String>, String> {
    let home = std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map_or_else(
            || PathBuf::from(crate::studio::home_folder(".codex")),
            |path| workspace.join(path),
        );
    if home.as_os_str().is_empty() {
        return Err("Codex home is unavailable.".into());
    }
    // Older Concat rollouts have a custom source omitted by the server's
    // interactive list. Read only the metadata row to discover their IDs.
    // Codex still reads the title, transcript and resume state itself.
    let mut folders = vec![(home.join("sessions"), 0)];
    let mut ids = HashSet::new();
    while let Some((folder, depth)) = folders.pop() {
        let children = match read_dir(folder) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("Could not read the Codex sessions folder.".into()),
        };
        for entry in children.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 3 {
                folders.push((entry.path(), depth + 1));
            } else if kind.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
                && let Ok(file) = File::open(entry.path())
                && let Some(id) = saved_chat_id(file)
            {
                ids.insert(id);
            }
        }
    }
    Ok(ids.into_iter().collect())
}

fn saved_chat_id(file: impl Read) -> Option<String> {
    let mut line = String::new();
    BufReader::new(file.take(64 * 1024))
        .read_line(&mut line)
        .ok()?;
    let value: Value = serde_json::from_str(&line).ok()?;
    let meta = &value["payload"];
    if value["type"] != "session_meta" || meta["originator"] != CLIENT_NAME {
        return None;
    }
    let id = meta["id"].as_str()?;
    (id.len() == 36
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'))
    .then(|| id.to_owned())
}

fn chat_title(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Untitled chat");
    line.chars().take(120).collect()
}

fn effort_label(effort: &str) -> &str {
    match effort {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Max",
        "ultra" => "Ultra",
        _ => effort,
    }
}

fn cap(text: &str) -> String {
    if text.chars().count() <= DISPLAY_LIMIT {
        return text.to_owned();
    }
    let mut shown: String = text.chars().take(DISPLAY_LIMIT).collect();
    shown.push_str("\n[Display truncated; the complete item remains in the Codex conversation.]");
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argv_is_structured_and_preserves_user_policy_and_model() {
        let args = app_server_args(Path::new(
            "/Applications/Concat Live.app/Contents/MacOS/concat",
        ))
        .expect("argv");
        assert_eq!(&args[..3], &["app-server", "--listen", "stdio://"]);
        let parsed: toml::Table = args[4].parse().expect("valid TOML");
        assert_eq!(
            parsed["mcp_servers"]["concat"]["command"].as_str(),
            Some("/Applications/Concat Live.app/Contents/MacOS/concat")
        );
        assert_eq!(
            parsed["mcp_servers"]["concat"]["args"][1].as_str(),
            Some("window")
        );
        assert!(!args.iter().any(|arg| arg.contains("dangerously")
            || arg.contains("approval_policy")
            || arg.contains("sandbox_mode")
            || arg.starts_with("model=")));
    }
    #[test]
    fn displayed_text_is_bounded_at_unicode_character_boundaries() {
        let text = "Ж".repeat(DISPLAY_LIMIT + 1);
        let shown = cap(&text);
        assert!(shown.starts_with(&"Ж".repeat(DISPLAY_LIMIT)));
        assert!(shown.contains("Display truncated"));
    }

    #[test]
    fn model_change_uses_the_new_default_and_refuses_an_out_of_range_effort() {
        let mut models = Models {
            catalogue: vec![
                ModelOption {
                    model: "primary".into(),
                    label: "Primary".into(),
                    efforts: vec!["medium".into(), "high".into()],
                    default_effort: Some("medium".into()),
                },
                ModelOption {
                    model: "secondary".into(),
                    label: "Secondary".into(),
                    efforts: vec!["low".into()],
                    default_effort: Some("low".into()),
                },
            ],
            current: ModelSelection {
                model: Some("primary".into()),
                effort: Some("high".into()),
            },
            explicit: false,
        };
        assert!(
            models.selection().model.is_none(),
            "inherit Codex configuration"
        );
        models.select_model(1);
        models.select_effort(1);
        let selection = models.selection();
        assert_eq!(selection.model.as_deref(), Some("secondary"));
        assert_eq!(selection.effort.as_deref(), Some("low"));
        models.select_model(0);
        models.select_effort(1);
        assert_eq!(models.selection().effort.as_deref(), Some("high"));
    }
    #[test]
    fn history_discovers_legacy_concat_sessions_without_importing_other_apps() {
        let id = "12345678-1234-1234-1234-123456789abc";
        for source in [
            serde_json::json!({"custom": "concat_native"}),
            serde_json::json!("vscode"),
        ] {
            let metadata = serde_json::json!({"type":"session_meta","payload":{
                "originator":CLIENT_NAME,"source":source,"id":id
            }});
            let rollout = format!("{metadata}\nany later items belong to Codex");
            assert_eq!(saved_chat_id(rollout.as_bytes()).as_deref(), Some(id));
        }
        let foreign = serde_json::json!({"type":"session_meta","payload":{
            "originator":"another_client","source":"vscode","id":id
        }})
        .to_string();
        assert!(saved_chat_id(foreign.as_bytes()).is_none());
    }

    #[test]
    fn switching_chats_restores_only_the_selected_draft() {
        let mut history = ChatHistory::default();
        assert!(
            history
                .move_draft(Some("one"), "Draft one".into(), "two")
                .is_empty()
        );
        assert_eq!(
            history.move_draft(Some("two"), "Draft two".into(), "one"),
            "Draft one"
        );
        assert_eq!(
            history.move_draft(Some("one"), "Updated".into(), "two"),
            "Draft two"
        );
        assert_eq!(
            history.drafts.get("one").map(String::as_str),
            Some("Updated")
        );
    }
}
