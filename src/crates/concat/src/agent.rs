// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Native Codex chat. The event loop owns stable models; a cancellable
//! worker owns app-server IO and its own child process group.

mod dictation;
mod protocol;

use std::cell::RefCell;
use std::collections::HashMap;
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

use crate::ui::{Agent, AgentApprovalData, AgentMessageData, App};
use dictation::Dictation;
use protocol::{Context, Effect, Event, Protocol};

const DISPLAY_LIMIT: usize = 32_000;
const WIRE_LIMIT: u64 = 16 * 1024 * 1024;

thread_local! {
    static PANE: RefCell<Option<AgentPane>> = const { RefCell::new(None) };
}

/// Installs the models and callbacks without starting a process.
pub(crate) fn install(app: &App) {
    let messages = Rc::new(VecModel::default());
    let approvals = Rc::new(VecModel::default());
    let global = app.global::<Agent>();
    global.set_messages(messages.clone().into());
    global.set_approvals(approvals.clone().into());
    global.on_submitted(|| with_pane(AgentPane::submit));
    global.on_interrupt(|| with_pane(AgentPane::interrupt));
    global.on_new_thread(|| with_pane(|pane| pane.restart(true)));
    global.on_reconnect(|| with_pane(|pane| pane.restart(false)));
    global.on_choose_workspace(|| with_pane(AgentPane::choose_workspace));
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
        if pane.busy
            || pane.dictation.active()
            || (restarting
                && pane
                    .ui
                    .upgrade()
                    .is_some_and(|app| !app.global::<Agent>().get_draft().is_empty()))
        {
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
                || (restarting
                    && pane
                        .ui
                        .upgrade()
                        .is_some_and(|app| !app.global::<Agent>().get_draft().is_empty()))
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
            self.dictation.publish(&global);
        }
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
        if self.worker.is_some() {
            return Ok(());
        }
        let context = self
            .context
            .clone()
            .ok_or("Choose a working folder before starting Codex")?;
        self.worker = Some(Worker::start(
            context,
            self.thread.clone(),
            self.generation,
        )?);
        self.status = "Connecting to Codex…".into();
        self.error.clear();
        Ok(())
    }

    fn start_dictation(&mut self) {
        if !self.enabled() || self.maintenance || self.dictation.active() {
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
        if !self.allowed() || self.busy || self.dictation.active() {
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

    fn restart(&mut self, fresh: bool) {
        if !self.allowed() || self.busy || self.dictation.active() {
            return;
        }
        self.stop();
        if fresh {
            self.thread = None;
            self.local_message(
                "Context",
                "New conversation. Earlier messages remain visible above.".into(),
            );
        }
        if let Err(error) = self.start() {
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
                    self.busy = busy;
                    self.status = status;
                }
                Event::Error(error) => self.error = error,
                Event::Fatal(error) => {
                    self.restore_unsent();
                    self.error = error;
                    self.status = "Disconnected · reconnect to continue".into();
                    self.busy = false;
                    self.finish_streaming(true);
                    self.clear_approvals();
                    self.worker.take();
                }
                Event::Thread(id) => self.thread = Some(id),
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
                    {
                        self.unsent_draft = None;
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
        context: Context,
    },
    ContextChanged(Context),
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
                    context: updated,
                } => {
                    context = updated;
                    check_live(&context)?;
                    effects.extend(protocol.update_context(context.clone()));
                    effects.extend(protocol.submit(text, id)?);
                }
                Control::ContextChanged(updated) => {
                    context = updated;
                    effects.extend(protocol.update_context(context.clone()));
                }
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
}
