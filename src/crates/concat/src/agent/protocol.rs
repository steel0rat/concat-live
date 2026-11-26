// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Versioned Codex app-server messages, independent of the Slint event loop.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) const CLIENT_NAME: &str = "concat_native";
const DEFAULT_SKILL: &str = "steel0rat-video-maker-skill";
const CHAT_PAGE_SIZE: u32 = 16;
const APPLICATION_CONTEXT: &str = "Application context: working folder ";
const MODEL_PAGE_SIZE: u32 = 100;
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(super) struct Context {
    pub path: PathBuf,
    pub project: Option<PathBuf>,
    pub name: String,
    pub epoch: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ModelSelection {
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug)]
pub(super) struct ModelOption {
    pub model: String,
    pub label: String,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

impl ModelOption {
    fn from_catalogue(value: &Value) -> Option<Self> {
        if value["hidden"].as_bool() == Some(true) {
            return None;
        }
        let model = required(value, "model").ok()?;
        let label = required(value, "displayName").unwrap_or(model);
        let mut efforts = Vec::new();
        for effort in value["supportedReasoningEfforts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Ok(level) = required(effort, "reasoningEffort")
                && !efforts.iter().any(|existing| existing == level)
            {
                efforts.push(level.to_owned());
            }
        }
        let default_effort = efforts
            .iter()
            .find(|level| level.as_str() == string(value, "defaultReasoningEffort"))
            .cloned();
        Some(Self {
            model: model.to_owned(),
            label: label.to_owned(),
            efforts,
            default_effort,
        })
    }
}

struct ModelCatalogue {
    provider: Option<String>,
    current: ModelSelection,
    models: Vec<ModelOption>,
    seen_cursors: HashSet<String>,
}

#[derive(Debug)]
pub(super) struct SavedChat {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub updated_at: i64,
}

#[derive(Debug)]
pub(super) struct HistoryMessage {
    pub id: String,
    pub role: String,
    pub text: String,
}

#[derive(Debug, Default)]
pub(super) struct HistoryPage {
    pub messages: Vec<HistoryMessage>,
    pub older: Option<String>,
}

struct ChatLookup {
    remaining: usize,
    chats: Vec<SavedChat>,
    incomplete: bool,
}

#[derive(Debug)]
pub(super) enum Event {
    State {
        busy: bool,
        status: String,
    },
    Error(String),
    Fatal(String),
    Thread(String),
    SavedChats {
        chats: Vec<SavedChat>,
        incomplete: bool,
    },
    ChatListFailed(String),
    ChatOpened {
        id: String,
        page: HistoryPage,
    },
    ChatOpenFailed,
    OlderMessages(HistoryPage),
    OlderMessagesFailed,
    Models {
        current: ModelSelection,
        options: Vec<ModelOption>,
    },
    ModelsUnavailable,
    Message {
        id: String,
        role: String,
        text: String,
        append: bool,
        streaming: bool,
        failed: bool,
    },
    Sent(String),
    Approval {
        id: String,
        title: String,
        detail: String,
    },
    Resolved(String),
    Finished {
        failed: bool,
    },
}

#[derive(Debug)]
pub(super) enum Effect {
    Write(Value),
    Show(Event),
    Stop(String),
}

enum Pending {
    Initialize,
    Account,
    Thread,
    SavedChat(String),
    OpenTranscript(String),
    ChangeThread {
        expected: Option<String>,
        page: HistoryPage,
    },
    OlderMessages(String),
    Models(ModelCatalogue),
    Skills,
    Turn(String),
    Interrupt,
}

struct Approval {
    wire_id: Value,
    permissions: Option<Value>,
}

pub(super) struct Protocol {
    context: Context,
    resume: Option<String>,
    restore_history: bool,
    chat_lookup: Option<ChatLookup>,
    next_id: u64,
    pending: HashMap<u64, (Pending, Instant)>,
    thread: Option<String>,
    turn: Option<String>,
    completed: HashSet<String>,
    queued: Option<(String, String, String)>,
    pub(super) selection: ModelSelection,
    unacknowledged_user: Option<String>,
    active: bool,
    interrupt_requested: bool,
    approvals: HashMap<String, Approval>,
    items: HashMap<String, Value>,
}

impl Protocol {
    pub fn new(context: Context, resume: Option<String>, restore_history: bool) -> Self {
        Self {
            context,
            resume,
            restore_history,
            chat_lookup: None,
            next_id: 0,
            pending: HashMap::new(),
            thread: None,
            turn: None,
            completed: HashSet::new(),
            queued: None,
            selection: ModelSelection::default(),
            unacknowledged_user: None,
            active: false,
            interrupt_requested: false,
            approvals: HashMap::new(),
            items: HashMap::new(),
        }
    }

    fn request(&mut self, method: &str, params: Value, kind: Pending) -> Value {
        self.next_id += 1;
        self.pending.insert(self.next_id, (kind, Instant::now()));
        json!({"id": self.next_id, "method": method, "params": params})
    }

    pub fn initialize(&mut self) -> Value {
        self.request("initialize", json!({"clientInfo": {"name": CLIENT_NAME, "title": "Concat Live", "version": env!("CARGO_PKG_VERSION")}}), Pending::Initialize)
    }

    pub fn timed_out(&self) -> bool {
        self.pending
            .values()
            .any(|(_, since)| since.elapsed() > Duration::from_secs(120))
    }

    pub fn expire_model_list(&mut self) -> Option<Effect> {
        let count = self.pending.len();
        self.pending.retain(|_, (kind, since)| {
            !matches!(kind, Pending::Models(_)) || since.elapsed() < MODEL_LIST_TIMEOUT
        });
        (self.pending.len() != count).then_some(Effect::Show(Event::ModelsUnavailable))
    }

    pub fn connected(&self) -> bool {
        self.thread.is_some()
    }

    pub fn list_chats(&mut self, ids: Vec<String>) -> Vec<Effect> {
        if self.chat_lookup.is_some() {
            return Vec::new();
        }
        if ids.is_empty() {
            return vec![Effect::Show(Event::SavedChats {
                chats: Vec::new(),
                incomplete: false,
            })];
        }
        self.chat_lookup = Some(ChatLookup {
            remaining: ids.len(),
            chats: Vec::new(),
            incomplete: false,
        });
        ids.into_iter()
            .map(|id| {
                Effect::Write(self.request(
                    "thread/read",
                    json!({"threadId": id, "includeTurns": false}),
                    Pending::SavedChat(id),
                ))
            })
            .collect()
    }

    fn receive_chat(&mut self, id: &str, thread: &Value) -> Vec<Effect> {
        let Some(lookup) = &mut self.chat_lookup else {
            return Vec::new();
        };
        if thread["id"].as_str() == Some(id)
            && thread["originator"]
                .as_str()
                .is_none_or(|originator| originator == CLIENT_NAME)
        {
            if thread["ephemeral"].as_bool() != Some(true) {
                lookup.chats.push(SavedChat {
                    id: id.to_owned(),
                    title: required(thread, "name")
                        .or_else(|_| required(thread, "preview"))
                        .unwrap_or("Untitled chat")
                        .to_owned(),
                    cwd: string(thread, "cwd").to_owned(),
                    updated_at: thread["updatedAt"].as_i64().unwrap_or_default(),
                });
            }
        } else {
            lookup.incomplete = true;
        }
        lookup.remaining -= 1;
        if lookup.remaining != 0 {
            return Vec::new();
        }
        let mut result = self.chat_lookup.take().expect("completed chat lookup");
        result.chats.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        vec![Effect::Show(Event::SavedChats {
            chats: result.chats,
            incomplete: result.incomplete,
        })]
    }

    fn switching_chat(&self) -> bool {
        self.pending.values().any(|(kind, _)| {
            matches!(
                kind,
                Pending::OpenTranscript(_) | Pending::ChangeThread { .. }
            )
        })
    }

    pub fn change_chat(&mut self, thread: Option<String>) -> Result<Vec<Effect>, String> {
        if self.active || self.queued.is_some() || self.switching_chat() || !self.connected() {
            return Ok(vec![Effect::Show(Event::ChatOpenFailed)]);
        }
        let request = match thread {
            Some(id) => self.transcript_page(&id, None, Pending::OpenTranscript(id.clone())),
            None => self.thread_request(
                None,
                Pending::ChangeThread {
                    expected: None,
                    page: HistoryPage::default(),
                },
            )?,
        };
        Ok(vec![Effect::Write(request)])
    }

    fn chat_open_failed(&self) -> Result<Vec<Effect>, String> {
        if self.connected() {
            Ok(vec![Effect::Show(Event::ChatOpenFailed)])
        } else {
            Err(
                "Could not restore the last chat. Reconnect to try again or open a new chat."
                    .into(),
            )
        }
    }

    pub fn older_messages(&mut self, cursor: String) -> Option<Effect> {
        let thread = self.thread.clone()?;
        Some(Effect::Write(self.transcript_page(
            &thread,
            Some(&cursor),
            Pending::OlderMessages(cursor.clone()),
        )))
    }

    fn transcript_page(&mut self, thread: &str, cursor: Option<&str>, kind: Pending) -> Value {
        self.request(
            "thread/turns/list",
            json!({
                "threadId": thread, "itemsView": "summary", "sortDirection": "desc",
                "limit": CHAT_PAGE_SIZE, "cursor": cursor
            }),
            kind,
        )
    }

    fn thread_request(&mut self, resume: Option<&str>, kind: Pending) -> Result<Value, String> {
        let cwd = self
            .context
            .path
            .to_str()
            .ok_or("The project path is not valid UTF-8")?;
        let (method, params) = match resume {
            Some(thread) => (
                "thread/resume",
                json!({"threadId": thread, "cwd": cwd, "developerInstructions": self.instructions(), "excludeTurns": true}),
            ),
            None => (
                "thread/start",
                json!({"cwd": cwd, "developerInstructions": self.instructions()}),
            ),
        };
        Ok(self.request(method, params, kind))
    }

    fn load_models(&mut self, result: &Value) -> Effect {
        let catalogue = ModelCatalogue {
            provider: required(result, "modelProvider").ok().map(str::to_owned),
            current: ModelSelection {
                model: required(result, "model").ok().map(str::to_owned),
                effort: required(result, "reasoningEffort").ok().map(str::to_owned),
            },
            models: Vec::new(),
            seen_cursors: HashSet::new(),
        };
        self.model_page(catalogue, None)
    }

    fn model_page(&mut self, catalogue: ModelCatalogue, cursor: Option<String>) -> Effect {
        let params = json!({
            "modelProvider": catalogue.provider,
            "includeHidden": false,
            "limit": MODEL_PAGE_SIZE,
            "cursor": cursor
        });
        Effect::Write(self.request("model/list", params, Pending::Models(catalogue)))
    }

    fn receive_models(&mut self, mut catalogue: ModelCatalogue, result: &Value) -> Effect {
        let Some(entries) = result["data"].as_array() else {
            return Effect::Show(Event::ModelsUnavailable);
        };
        for option in entries.iter().filter_map(ModelOption::from_catalogue) {
            if !catalogue.models.iter().any(|old| old.model == option.model) {
                catalogue.models.push(option);
            }
        }
        if let Some(cursor) = result["nextCursor"].as_str() {
            if !catalogue.seen_cursors.insert(cursor.to_owned()) {
                return Effect::Show(Event::ModelsUnavailable);
            }
            return self.model_page(catalogue, Some(cursor.to_owned()));
        }
        Effect::Show(Event::Models {
            current: catalogue.current,
            options: catalogue.models,
        })
    }

    fn instructions(&self) -> String {
        format!(
            "You are the editing assistant inside the running Concat Live video editor. Your working folder is {}. {} Use the explicitly selected steel0rat-video-maker-skill for this request. Use the concat MCP server for every project read, creation, edit, import, preview, and export. Before any project operation, first call concat_version and require its capabilities to contain live and its mcp.mode to be window; stop with a clear error if either check fails. Plain conversation does not require tools. When no project is open, use project_create in the working folder with the name, frame width, height, frame rate and canvas requested by the user, then continue editing the returned visible project in this same conversation. Ask about missing scene parameters only when they change the intended result. Open or create another project only when the user requests it, and save existing work first. The visible in-memory project is authoritative, including unsaved manual edits. Never read or write concat.json or other project documents directly, and never open a headless session. Every project tool must name the exact absolute path of the visible project. Do not modify projects outside the working folder. The application supplies the latest visible-project context with each turn. If a context changes unexpectedly or a tool refuses it, stop that operation and explain the refusal. Give concise progress and results in the user's language; do not paste JSON, tool arguments, command output or protocol data unless the user explicitly asks. Respect the user's Codex model, sandbox, approval policy and existing instructions. Use normal approval requests whenever approval is required.",
            self.context.path.display(),
            self.context_note(),
        )
    }

    fn context_note(&self) -> String {
        match &self.context.project {
            Some(path) => format!(
                "Current visible project: {} (name: {:?}; project epoch: {}).",
                path.display(),
                self.context.name,
                self.context.epoch
            ),
            None => format!(
                "No project is open yet (project epoch: {}). Create the requested project under {}.",
                self.context.epoch,
                self.context.path.display()
            ),
        }
    }

    pub fn update_context(&mut self, context: Context) -> Vec<Effect> {
        let changed = self.context.path != context.path
            || self.context.project != context.project
            || self.context.epoch != context.epoch;
        let interrupt =
            changed && (self.queued.is_some() || (self.active && self.context.project.is_some()));
        self.context = context;
        if !interrupt {
            return Vec::new();
        }
        let mut effects = self
            .approvals
            .drain()
            .map(|(id, _)| Effect::Show(Event::Resolved(id)))
            .collect::<Vec<_>>();
        effects.extend(self.interrupt());
        effects.push(Effect::Show(Event::Error(
            "The visible project changed. The previous operation was interrupted; this conversation can continue on the current project.".into()
        )));
        effects
    }

    #[cfg(test)]
    pub fn submit(&mut self, text: String, id: String) -> Result<Vec<Effect>, String> {
        self.submit_with_detail(text, id, String::new())
    }

    pub fn submit_with_detail(
        &mut self,
        text: String,
        id: String,
        detail: String,
    ) -> Result<Vec<Effect>, String> {
        if !self.connected() && self.restore_history {
            return Err("Wait for the last chat to reopen before sending.".into());
        }
        if self.active || self.queued.is_some() || self.switching_chat() {
            return Err("Codex is already working on a request.".into());
        }
        if self.context.path.to_str().is_none() {
            return Err("The working folder path is not valid UTF-8".into());
        }
        self.queued = Some((text, id, detail));
        self.interrupt_requested = false;
        Ok(self.begin_turn())
    }

    fn begin_turn(&mut self) -> Vec<Effect> {
        if self.thread.is_none() || self.queued.is_none() {
            return Vec::new();
        }
        let request = self.request(
            "skills/list",
            json!({"cwds": [self.context.path.to_string_lossy()], "forceReload": true}),
            Pending::Skills,
        );
        vec![
            Effect::Write(request),
            state(true, "Refreshing the video-making skill…"),
        ]
    }

    fn default_skill(&self, result: &Value) -> Result<Value, String> {
        let failed = |detail: &str| {
            format!(
                "Default video-making skill {DEFAULT_SKILL}: {detail}. Install or enable the skill before retrying this request."
            )
        };
        let entries = result["data"]
            .as_array()
            .ok_or_else(|| failed("Codex did not return a skill catalogue"))?;
        let entry = entries
            .iter()
            .find(|entry| Path::new(string(entry, "cwd")) == self.context.path)
            .ok_or_else(|| failed("Codex did not inspect the current working folder"))?;
        let skills = entry["skills"]
            .as_array()
            .ok_or_else(|| failed("Codex did not return the working folder's skills"))?;
        let mut matches = skills
            .iter()
            .filter(|skill| string(skill, "name") == DEFAULT_SKILL);
        let skill = matches
            .next()
            .ok_or_else(|| failed("the skill is not installed or could not be loaded"))?;
        if matches.next().is_some() {
            return Err(failed("more than one copy has this name"));
        }
        if skill["enabled"].as_bool() != Some(true) {
            return Err(failed("the skill is disabled"));
        }
        let path = string(skill, "path");
        let file = Path::new(path);
        if !file.is_absolute()
            || file.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
            || file.components().any(|part| part == Component::ParentDir)
            || path.contains('\0')
        {
            return Err(failed(
                "Codex did not provide a valid absolute SKILL.md path",
            ));
        }
        Ok(json!({"type": "skill", "name": DEFAULT_SKILL, "path": path}))
    }

    fn send_turn(&mut self, skill: Value) -> Vec<Effect> {
        let Some(thread) = self.thread.clone() else {
            return self
                .fail_queued("Codex lost the conversation before the request was sent".into());
        };
        let Some((text, id, detail)) = self.queued.take() else {
            return Vec::new();
        };
        self.unacknowledged_user = Some(id.clone());
        self.active = true;
        let mut params = json!({
            "threadId": thread,
            "clientUserMessageId": id,
            "input": [
                {"type": "text", "text": text},
                {"type": "text", "text": format!("{APPLICATION_CONTEXT}{}. {}{detail}", self.context.path.display(), self.context_note())},
                skill
            ]
        });
        if let Some(model) = &self.selection.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = &self.selection.effort {
            params["effort"] = json!(effort);
        }
        let request = self.request("turn/start", params, Pending::Turn(id));
        vec![Effect::Write(request), state(true, "Codex is working…")]
    }

    fn acknowledge_user(&mut self) -> Option<Effect> {
        self.unacknowledged_user
            .take()
            .map(|id| Effect::Show(Event::Sent(id)))
    }

    fn finish_queued(&mut self, failed: bool, status: &str) -> Vec<Effect> {
        if self.queued.take().is_none() {
            return Vec::new();
        }
        self.pending
            .retain(|_, (kind, _)| !matches!(kind, Pending::Skills));
        self.active = false;
        vec![
            Effect::Show(Event::Finished { failed }),
            state(false, status),
        ]
    }

    fn fail_queued(&mut self, error: String) -> Vec<Effect> {
        let mut effects = vec![Effect::Show(Event::Error(error))];
        effects.extend(self.finish_queued(true, "Request not sent · ready"));
        effects
    }

    pub fn interrupt(&mut self) -> Vec<Effect> {
        self.interrupt_requested = true;
        if self.queued.is_some() {
            return self.finish_queued(false, "Interrupted · ready");
        }
        match (self.thread.clone(), self.turn.clone()) {
            (Some(thread), Some(turn)) => {
                let request = self.request(
                    "turn/interrupt",
                    json!({"threadId": thread, "turnId": turn}),
                    Pending::Interrupt,
                );
                vec![Effect::Write(request)]
            }
            _ => Vec::new(),
        }
    }

    pub fn approve(&mut self, id: &str, accepted: bool) -> Vec<Effect> {
        let Some(approval) = self.approvals.remove(id) else {
            return Vec::new();
        };
        let result = match approval.permissions {
            Some(permissions) => {
                json!({"permissions": if accepted { permissions } else { json!({}) }, "scope": "turn"})
            }
            None => json!({"decision": if accepted { "accept" } else { "decline" }}),
        };
        vec![
            Effect::Write(json!({"id": approval.wire_id, "result": result})),
            Effect::Show(Event::Resolved(id.to_owned())),
            state(true, "Codex is working…"),
        ]
    }

    pub fn receive(&mut self, message: Value) -> Result<Vec<Effect>, String> {
        if let Some(method) = message["method"].as_str() {
            let params = &message["params"];
            return match message.get("id") {
                Some(id) => self.server_request(method, id, params),
                None => self.notification(method, params),
            };
        }
        let id = message["id"]
            .as_u64()
            .ok_or("Codex sent a response without a numeric request ID")?;
        let Some((kind, _)) = self.pending.remove(&id) else {
            return Ok(Vec::new());
        };
        if let Pending::SavedChat(id) = &kind {
            return Ok(self.receive_chat(id, &message["result"]["thread"]));
        }
        if message.get("error").is_some() || message.get("result").is_none() {
            let failed = match &kind {
                Pending::OpenTranscript(_) | Pending::ChangeThread { .. } => {
                    return self.chat_open_failed();
                }
                Pending::OlderMessages(_) => Some(Event::OlderMessagesFailed),
                _ => None,
            };
            if let Some(event) = failed {
                return Ok(vec![Effect::Show(event)]);
            }
        }
        if matches!(&kind, Pending::Models(_))
            && (message.get("error").is_some() || message.get("result").is_none())
        {
            return Ok(vec![Effect::Show(Event::ModelsUnavailable)]);
        }
        if let Some(error) = message.get("error") {
            let detail = format!("Codex rejected a request: {}", string(error, "message"));
            return if matches!(&kind, Pending::Skills) {
                Ok(self.fail_queued(format!(
                    "Cannot refresh the default video-making skill {DEFAULT_SKILL}: {detail}"
                )))
            } else {
                Err(detail)
            };
        }
        let Some(result) = message.get("result") else {
            return if matches!(&kind, Pending::Skills) {
                Ok(self.fail_queued(format!(
                    "Codex did not return the default video-making skill {DEFAULT_SKILL}"
                )))
            } else {
                Err("Codex sent a response without a result".into())
            };
        };
        let mut effects = Vec::new();
        match kind {
            Pending::Initialize => {
                effects.push(Effect::Write(json!({"method": "initialized"})));
                effects.push(Effect::Write(self.request(
                    "account/read",
                    json!({"refreshToken": false}),
                    Pending::Account,
                )));
            }
            Pending::Account => {
                let requires_auth = result["requiresOpenaiAuth"]
                    .as_bool()
                    .ok_or("Codex did not report its authentication state")?;
                if requires_auth && result["account"].is_null() {
                    return Err("Codex is not signed in. Run codex login, then reconnect.".into());
                }
                let resume = self.resume.clone();
                let request = match resume {
                    Some(id) if self.restore_history => {
                        self.transcript_page(&id, None, Pending::OpenTranscript(id.clone()))
                    }
                    _ => self.thread_request(resume.as_deref(), Pending::Thread)?,
                };
                effects.push(Effect::Write(request));
            }
            Pending::Thread => {
                let id = required(&result["thread"], "id")?.to_owned();
                self.thread = Some(id.clone());
                effects.push(Effect::Show(Event::Thread(id)));
                effects.push(self.load_models(result));
                if self.queued.is_none() {
                    effects.push(state(false, "Ready"));
                }
                effects.extend(self.begin_turn());
            }
            Pending::SavedChat(_) => unreachable!("handled before general response errors"),
            Pending::OpenTranscript(id) => {
                if let Some(page) = history_page(result, None) {
                    effects.push(Effect::Write(self.thread_request(
                        Some(&id),
                        Pending::ChangeThread {
                            expected: Some(id.clone()),
                            page,
                        },
                    )?));
                } else {
                    return self.chat_open_failed();
                }
            }
            Pending::ChangeThread { expected, page } => {
                let Some(id) = result["thread"]["id"].as_str().filter(|id| {
                    !id.is_empty() && expected.as_deref().is_none_or(|expected| expected == *id)
                }) else {
                    return self.chat_open_failed();
                };
                // Wait for the transcript and resume to succeed before switching.
                // Responses still pending for the previous chat cannot update this one.
                self.pending
                    .retain(|_, (kind, _)| matches!(kind, Pending::SavedChat(_)));
                self.thread = Some(id.to_owned());
                self.turn = None;
                self.completed.clear();
                self.items.clear();
                self.approvals.clear();
                self.selection = ModelSelection::default();
                self.unacknowledged_user = None;
                self.interrupt_requested = false;
                effects.push(Effect::Show(Event::ChatOpened {
                    id: id.to_owned(),
                    page,
                }));
                effects.push(self.load_models(result));
                effects.push(state(false, "Ready"));
            }
            Pending::OlderMessages(cursor) => {
                let event = history_page(result, Some(&cursor))
                    .map_or(Event::OlderMessagesFailed, Event::OlderMessages);
                effects.push(Effect::Show(event));
            }
            Pending::Models(catalogue) => effects.push(self.receive_models(catalogue, result)),
            Pending::Skills => match self.default_skill(result) {
                Ok(skill) => effects.extend(self.send_turn(skill)),
                Err(error) => effects.extend(self.fail_queued(error)),
            },
            Pending::Turn(user) => {
                if self.unacknowledged_user.as_deref() == Some(user.as_str()) {
                    self.unacknowledged_user = None;
                }
                effects.push(Effect::Show(Event::Sent(user)));
                let turn = &result["turn"];
                let id = required(turn, "id")?.to_owned();
                if !self.completed.contains(&id) {
                    self.turn = Some(id);
                    if matches!(
                        string(turn, "status"),
                        "completed" | "failed" | "interrupted"
                    ) {
                        effects.extend(self.complete(turn));
                    } else if self.interrupt_requested {
                        effects.extend(self.interrupt());
                    }
                }
            }
            Pending::Interrupt => {}
        }
        Ok(effects)
    }

    fn server_request(
        &mut self,
        method: &str,
        wire_id: &Value,
        params: &Value,
    ) -> Result<Vec<Effect>, String> {
        let known = matches!(
            method,
            "item/commandExecution/requestApproval"
                | "item/fileChange/requestApproval"
                | "item/permissions/requestApproval"
        );
        if !known || self.thread.as_deref() != params["threadId"].as_str() {
            let explanation = if method == "account/chatgptAuthTokens/refresh" {
                "Codex needs a new sign-in. Run codex login, then reconnect.".to_owned()
            } else {
                format!(
                    "Unsupported Codex request: {method}. The connection was stopped; no approval was granted."
                )
            };
            let mut effects = vec![Effect::Write(
                json!({"id": wire_id, "error": {"code": -32601, "message": "This client does not support this server request"}}),
            )];
            effects.extend(self.interrupt());
            effects.push(Effect::Stop(explanation));
            return Ok(effects);
        }
        if self.interrupt_requested || self.completed.contains(string(params, "turnId")) {
            return Ok(vec![Effect::Write(json!({
                "id": wire_id,
                "error": {"code": -32000, "message": "This approval no longer belongs to the active project turn"}
            }))]);
        }
        let (title, detail, permissions) = match method {
            "item/commandExecution/requestApproval" => {
                let mut detail = join_nonempty(&[
                    string(params, "reason"),
                    string(params, "command"),
                    string(params, "cwd"),
                ]);
                if let Some(network) = params
                    .get("networkApprovalContext")
                    .filter(|value| !value.is_null())
                {
                    detail.push_str(&format!("\nNetwork access: {network}"));
                }
                ("Allow this Codex command?", detail, None)
            }
            "item/fileChange/requestApproval" => {
                let item = self.items.get(string(params, "itemId"))
                    .filter(|item| string(item, "type") == "fileChange"
                        && item["changes"].as_array().is_some_and(|changes| !changes.is_empty()))
                    .ok_or("Codex did not supply a reviewable file-change preview; no approval was granted")?;
                let preview = file_change_preview(item);
                (
                    "Allow these file changes?",
                    join_nonempty(&[
                        string(params, "reason"),
                        &preview,
                        string(params, "grantRoot"),
                    ]),
                    None,
                )
            }
            _ => {
                let permissions = params
                    .get("permissions")
                    .filter(|value| value.is_object())
                    .ok_or("Codex requested permissions without a permission profile")?
                    .clone();
                (
                    "Allow these permissions for this turn?",
                    join_nonempty(&[
                        string(params, "reason"),
                        &permissions.to_string(),
                        string(params, "cwd"),
                    ]),
                    Some(permissions),
                )
            }
        };
        let key = wire_id.to_string();
        self.approvals.insert(
            key.clone(),
            Approval {
                wire_id: wire_id.clone(),
                permissions,
            },
        );
        Ok(vec![Effect::Show(Event::Approval {
            id: key,
            title: title.to_owned(),
            detail,
        })])
    }

    fn notification(&mut self, method: &str, params: &Value) -> Result<Vec<Effect>, String> {
        if let Some(thread) = params["threadId"].as_str()
            && self.thread.as_deref() != Some(thread)
        {
            return Ok(Vec::new());
        }
        if let Some(turn) = params["turnId"].as_str()
            && self.completed.contains(turn)
        {
            return Ok(Vec::new());
        }
        let mut effects = Vec::new();
        match method {
            "turn/started" => {
                let id = required(&params["turn"], "id")?.to_owned();
                if !self.completed.contains(&id) {
                    self.turn = Some(id);
                    self.active = true;
                    effects.extend(self.acknowledge_user());
                    if self.interrupt_requested {
                        effects.extend(self.interrupt());
                    }
                }
            }
            "turn/completed" => effects.extend(self.complete(&params["turn"])),
            "item/started" | "item/completed" => {
                let item = &params["item"];
                let id = required(item, "id")?.to_owned();
                self.items.insert(id.clone(), item.clone());
                if string(item, "type") != "userMessage" {
                    let (role, text) = item_text(item);
                    effects.push(Effect::Show(Event::Message {
                        id,
                        role,
                        text,
                        append: false,
                        streaming: method == "item/started",
                        failed: item_failed(item),
                    }));
                }
            }
            "item/agentMessage/delta" | "item/plan/delta" | "item/reasoning/summaryTextDelta" => {
                let role = match method {
                    "item/reasoning/summaryTextDelta" => "Thinking",
                    "item/plan/delta" => "Plan",
                    _ => "Codex",
                };
                effects.push(Effect::Show(Event::Message {
                    id: required(params, "itemId")?.to_owned(),
                    role: role.to_owned(),
                    text: required(params, "delta")?.to_owned(),
                    append: true,
                    streaming: true,
                    failed: false,
                }));
            }
            // Raw stdout and MCP progress may contain whole JSON documents.
            // Tool start/completion already provide the activity's visible state.
            "item/commandExecution/outputDelta" | "item/mcpToolCall/progress" => {}
            "turn/plan/updated" => {
                let mut lines = vec![string(params, "explanation").to_owned()];
                if let Some(steps) = params["plan"].as_array() {
                    lines.extend(steps.iter().map(|step| {
                        format!("{} · {}", string(step, "status"), string(step, "step"))
                    }));
                }
                effects.push(Effect::Show(Event::Message {
                    id: format!("plan-{}", string(params, "turnId")),
                    role: "Plan".to_owned(),
                    text: lines.join("\n"),
                    append: false,
                    streaming: true,
                    failed: false,
                }));
            }
            "serverRequest/resolved" => {
                let key = params["requestId"].to_string();
                self.approvals.remove(&key);
                effects.push(Effect::Show(Event::Resolved(key)));
            }
            "error" => {
                effects.push(Effect::Show(Event::Error(
                    string(&params["error"], "message").to_owned(),
                )));
                if params["willRetry"].as_bool() == Some(true) {
                    effects.push(state(true, "Codex is retrying…"));
                }
            }
            "mcpServer/startupStatus/updated"
                if string(params, "name") == "concat" && string(params, "status") == "failed" =>
            {
                return Err(format!(
                    "The Concat MCP server failed to start: {}",
                    string(params, "error")
                ));
            }
            "account/updated" if params["authMode"].is_null() => {
                return Err("Codex signed out. Run codex login, then reconnect.".into());
            }
            _ => {}
        }
        Ok(effects)
    }

    fn complete(&mut self, turn: &Value) -> Vec<Effect> {
        let id = string(turn, "id");
        if !self.completed.insert(id.to_owned()) {
            return Vec::new();
        }
        let status = string(turn, "status");
        self.turn = None;
        self.active = false;
        self.interrupt_requested = false;
        self.approvals.clear();
        let mut effects = Vec::new();
        effects.extend(self.acknowledge_user());
        if let Some(items) = turn["items"].as_array() {
            for item in items {
                if string(item, "type") != "userMessage" {
                    let (role, text) = item_text(item);
                    effects.push(Effect::Show(Event::Message {
                        id: string(item, "id").to_owned(),
                        role,
                        text,
                        append: false,
                        streaming: false,
                        failed: item_failed(item),
                    }));
                }
            }
        }
        let error = string(&turn["error"], "message");
        if !error.is_empty() {
            effects.push(Effect::Show(Event::Error(error.to_owned())));
        }
        effects.push(Effect::Show(Event::Finished {
            failed: status == "failed",
        }));
        effects.push(state(
            false,
            match status {
                "interrupted" => "Interrupted · ready",
                "failed" => "Request failed · ready",
                _ => "Ready",
            },
        ));
        effects
    }
}

fn history_page(result: &Value, previous_cursor: Option<&str>) -> Option<HistoryPage> {
    let older = result["nextCursor"]
        .as_str()
        .filter(|cursor| !cursor.is_empty());
    if older.is_some() && older == previous_cursor {
        return None;
    }
    let messages = result["data"]
        .as_array()?
        .iter()
        .rev()
        .flat_map(|turn| turn["items"].as_array().into_iter().flatten())
        .filter_map(|item| {
            let id = required(item, "id").ok()?.to_owned();
            let (role, text) = match string(item, "type") {
                "userMessage" => (
                    "You".to_owned(),
                    item["content"]
                        .as_array()?
                        .iter()
                        .enumerate()
                        .filter_map(|(index, part)| {
                            let text = string(part, "text");
                            (string(part, "type") == "text"
                                && (index == 0 || !text.starts_with(APPLICATION_CONTEXT)))
                            .then_some(text)
                        })
                        .collect::<Vec<_>>()
                        .join("\\n\\n"),
                ),
                "agentMessage" | "plan" => item_text(item),
                _ => return None,
            };
            (!text.is_empty()).then_some(HistoryMessage { id, role, text })
        })
        .collect();
    Some(HistoryPage {
        messages,
        older: older.map(str::to_owned),
    })
}

fn state(busy: bool, status: &str) -> Effect {
    Effect::Show(Event::State {
        busy,
        status: status.to_owned(),
    })
}
fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("Codex protocol event is missing {key}"))
}
fn join_nonempty(parts: &[&str]) -> String {
    parts
        .iter()
        .copied()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
fn item_failed(item: &Value) -> bool {
    matches!(string(item, "status"), "failed" | "declined")
        || item["exitCode"].as_i64().is_some_and(|code| code != 0)
        || (string(item, "type") == "mcpToolCall"
            && (item["result"]["isError"].as_bool() == Some(true)
                || !item["error"].is_null()
                || !item["result"]["structuredContent"]["error"].is_null()))
}

fn file_change_preview(item: &Value) -> String {
    item["changes"]
        .as_array()
        .map(|changes| {
            changes
                .iter()
                .map(|change| {
                    let action = format!(
                        "{} {}",
                        string(&change["kind"], "type"),
                        string(change, "path")
                    );
                    join_nonempty(&[&action, string(change, "diff")])
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn tool_error(item: &Value) -> String {
    for value in [
        &item["error"],
        &item["result"]["structuredContent"]["error"],
    ] {
        if let Some(message) = value["message"].as_str() {
            return message.to_owned();
        }
    }
    if item["result"]["isError"].as_bool() == Some(true)
        && let Some(content) = item["result"]["content"].as_array()
    {
        for part in content {
            let Some(text) = part["text"].as_str() else {
                continue;
            };
            match serde_json::from_str::<Value>(text) {
                Ok(value) => {
                    if let Some(message) = value["error"]["message"]
                        .as_str()
                        .or_else(|| value["message"].as_str())
                    {
                        return message.to_owned();
                    }
                }
                Err(_) => return text.to_owned(),
            }
        }
    }
    String::new()
}

fn tool_label(name: &str) -> &str {
    match name {
        "concat_version" => "Проверка подключения",
        "project_create" => "Создание проекта",
        "project_list" => "Поиск проектов",
        "project_open" => "Открытие проекта",
        "project_close" => "Закрытие проекта",
        "project_save" => "Сохранение проекта",
        "project_set_video" => "Настройка сцены",
        "project_get" | "timeline_read" => "Чтение проекта",
        "media_import" | "media_probe" => "Подготовка медиа",
        "title_add" | "title_style_words" | "title_clear_word_styles" | "captions_add" => {
            "Работа с титрами"
        }
        "preview_frame" => "Проверка кадра",
        "export_start" => "Запуск экспорта",
        "export_status" | "export_wait" => "Проверка экспорта",
        "export_cancel" => "Остановка экспорта",
        "edit_undo" => "Отмена изменения",
        "edit_redo" => "Повтор изменения",
        "keyframe_set" | "keyframe_clear" | "effect_key_set" => "Настройка анимации",
        name if name.starts_with("template_") => "Работа с шаблоном",
        name if name.starts_with("effect") => "Настройка эффектов",
        name if name.starts_with("clip") || name.starts_with("track_") || name == "edit_apply" => {
            "Монтаж"
        }
        _ => "Действие агента",
    }
}

fn activity_status(item: &Value) -> &str {
    if item_failed(item) {
        "ошибка"
    } else if string(item, "status") == "completed" {
        "выполнено"
    } else {
        "выполняется"
    }
}

fn item_text(item: &Value) -> (String, String) {
    let (role, text) = match string(item, "type") {
        "agentMessage" => ("Codex", string(item, "text").to_owned()),
        "plan" => ("Plan", string(item, "text").to_owned()),
        "reasoning" => (
            "Thinking",
            item["summary"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        ),
        "commandExecution" => (
            "Command",
            match item["exitCode"].as_i64() {
                Some(code) if code != 0 => format!("Команда завершилась с кодом {code}"),
                _ => format!("Выполнение команды · {}", activity_status(item)),
            },
        ),
        "fileChange" => (
            "Files",
            format!("Изменение файлов · {}", activity_status(item)),
        ),
        "mcpToolCall" => {
            let label = format!(
                "{} · {}",
                tool_label(string(item, "tool")),
                activity_status(item)
            );
            let error = tool_error(item);
            ("Tool", join_nonempty(&[&label, &error]))
        }
        "webSearch" => ("Search", string(item, "query").to_owned()),
        "imageView" => ("Image", "Просмотр изображения".to_owned()),
        "contextCompaction" => ("Context", "Сокращение истории беседы".to_owned()),
        _ => (
            "Activity",
            format!("Действие агента · {}", activity_status(item)),
        ),
    };
    (role.to_owned(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocol() -> Protocol {
        Protocol::new(
            Context {
                path: "/tmp/concat project".into(),
                project: Some("/tmp/concat project".into()),
                name: "Visible".into(),
                epoch: 7,
            },
            None,
            false,
        )
    }
    fn write(effects: &[Effect], method: &str) -> Value {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Write(value) if value["method"] == method => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing {method}: {effects:?}"))
    }
    fn ready(protocol: &mut Protocol) {
        let init = protocol.initialize();
        let account = write(
            &protocol
                .receive(json!({"id":init["id"],"result":{}}))
                .expect("initialize"),
            "account/read",
        );
        let thread = write(&protocol.receive(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false,"account":null}})).expect("account"), "thread/start");
        protocol
            .receive(json!({"id":thread["id"],"result":{"thread":{"id":"thread"}}}))
            .expect("thread");
    }

    #[test]
    fn handshake_queues_input_and_inherits_user_model_and_permissions() {
        let mut p = protocol();
        let init = p.initialize();
        assert!(
            p.submit("Make a cut".into(), "user".into())
                .expect("queued")
                .is_empty()
        );
        let effects = p
            .receive(json!({"id":init["id"],"result":{}}))
            .expect("initialize");
        assert!(effects.iter().any(|effect| matches!(effect, Effect::Write(v) if v["method"]=="initialized" && v.get("id").is_none())));
        let account = write(&effects, "account/read");
        let thread = write(&p.receive(json!({"id":account["id"],"result":{"requiresOpenaiAuth":true,"account":{"type":"chatgpt"}}})).expect("account"), "thread/start");
        assert_eq!(thread["params"]["cwd"], "/tmp/concat project");
        for field in ["model", "approvalPolicy", "sandbox", "approvalsReviewer"] {
            assert!(thread["params"].get(field).is_none(), "inherit {field}");
        }
        let instructions = thread["params"]["developerInstructions"]
            .as_str()
            .expect("instructions");
        assert!(instructions.contains("project epoch: 7"));
        assert!(instructions.contains("Never read or write concat.json"));
        let effects = p
            .receive(json!({"id":thread["id"],"result":{"thread":{"id":"thread"}}}))
            .expect("thread");
        let turn = refreshed_turn(&mut p, &effects);
        assert_eq!(turn["params"]["input"][0]["text"], "Make a cut");
        assert_eq!(turn["params"]["clientUserMessageId"], "user");
    }

    #[test]
    fn missing_auth_never_starts_a_thread() {
        let mut p = protocol();
        let init = p.initialize();
        let account = write(
            &p.receive(json!({"id":init["id"],"result":{}}))
                .expect("initialize"),
            "account/read",
        );
        assert!(
            p.receive(
                json!({"id":account["id"],"result":{"requiresOpenaiAuth":true,"account":null}})
            )
            .expect_err("sign-in")
            .contains("codex login")
        );
        assert!(!p.connected());
    }

    #[test]
    fn approval_keeps_id_type_and_is_one_time() {
        let mut p = protocol();
        ready(&mut p);
        p.receive(json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn","item":{"id":"item","type":"fileChange","status":"inProgress","changes":[{"kind":{"type":"add"},"path":"/tmp/example","diff":"+example"}]}}})).expect("reviewable file preview");
        for (method, id) in [
            ("item/commandExecution/requestApproval", json!(17)),
            ("item/fileChange/requestApproval", json!("17")),
        ] {
            let effects = p.receive(json!({"id":id,"method":method,"params":{"threadId":"thread","turnId":"turn","itemId":"item","command":"touch example"}})).expect("request");
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::Approval { .. })))
            );
            let effects = p.approve(&id.to_string(), true);
            assert!(
                matches!(&effects[0],Effect::Write(v) if v["id"]==id && v["result"]["decision"]=="accept")
            );
            assert!(p.approve(&id.to_string(), true).is_empty());
        }
    }

    #[test]
    fn permissions_grant_exact_requested_subset_for_one_turn() {
        let mut p = protocol();
        ready(&mut p);
        let permissions = json!({"network":{"enabled":true}});
        let request = json!({"id":9,"method":"item/permissions/requestApproval","params":{"threadId":"thread","turnId":"turn","itemId":"item","permissions":permissions}});
        p.receive(request.clone()).expect("request");
        assert!(
            matches!(&p.approve("9",true)[0],Effect::Write(v) if v["result"]["permissions"]==permissions && v["result"]["scope"]=="turn")
        );
        p.receive(request).expect("request");
        assert!(
            matches!(&p.approve("9",false)[0],Effect::Write(v) if v["result"]["permissions"]==json!({}))
        );
    }

    #[test]
    fn unknown_request_fails_closed_with_a_protocol_error() {
        let mut p = protocol();
        ready(&mut p);
        let effects = p
            .receive(json!({"id":"new","method":"new/approval","params":{"threadId":"thread"}}))
            .expect("unknown");
        assert!(
            matches!(&effects[0],Effect::Write(v) if v["id"]=="new" && v["error"]["code"]==-32601)
        );
        assert!(
            matches!(effects.last(),Some(Effect::Stop(error)) if error.contains("no approval was granted"))
        );
    }

    #[test]
    fn interrupt_waits_for_turn_id_and_uses_the_real_protocol() {
        let mut p = protocol();
        ready(&mut p);
        submit_turn(&mut p, "Edit", "user");
        assert!(p.interrupt().is_empty());
        let effects = p.receive(json!({"method":"turn/started","params":{"threadId":"thread","turn":{"id":"turn","status":"inProgress","items":[]}}})).expect("started");
        assert_eq!(
            write(&effects, "turn/interrupt")["params"],
            json!({"threadId":"thread","turnId":"turn"})
        );
    }

    #[test]
    fn completed_items_replace_deltas_and_old_thread_is_ignored() {
        let mut p = protocol();
        ready(&mut p);
        let effects = p.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"Part"}})).expect("delta");
        assert!(
            matches!(&effects[0],Effect::Show(Event::Message{id,text,append:true,streaming:true,..}) if id=="answer" && text=="Part")
        );
        let effects = p.receive(json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"answer","type":"agentMessage","text":"Final"}}})).expect("completed");
        assert!(
            matches!(&effects[0],Effect::Show(Event::Message{text,append:false,streaming:false,..}) if text=="Final")
        );
        assert!(p.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"old","itemId":"answer","delta":"stale"}})).expect("stale").is_empty());
    }

    #[test]
    fn late_turn_start_reply_cannot_resurrect_a_completed_turn() {
        let mut p = protocol();
        ready(&mut p);
        let start = submit_turn(&mut p, "Edit", "user");
        p.receive(json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"completed","items":[]}}})).expect("completed");
        p.receive(json!({"id":start["id"],"result":{"turn":{"id":"turn","status":"inProgress","items":[]}}})).expect("late reply");
        assert!(p.turn.is_none());
        assert!(!p.active);
        assert!(p.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"stale"}})).expect("late delta").is_empty());
    }
    #[test]
    fn approval_from_a_completed_turn_is_not_presented_or_granted() {
        let mut p = protocol();
        ready(&mut p);
        p.receive(json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"old","status":"completed","items":[]}}})).expect("complete old turn");
        let effects = p.receive(json!({"id":42,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread","turnId":"old","itemId":"old-command","startedAtMs":0,"command":"touch stale"}})).expect("late approval");
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Event::Approval { .. }))),
            "an old turn must never offer an approval"
        );
        assert!(p.approve("42", true).is_empty());
        assert!(matches!(&effects[0], Effect::Write(value) if value["error"]["code"] == -32000));
    }

    #[test]
    fn file_approval_without_a_reviewable_diff_fails_clearly() {
        let mut p = protocol();
        ready(&mut p);
        let result = p.receive(json!({"id":43,"method":"item/fileChange/requestApproval","params":{"threadId":"thread","turnId":"turn","itemId":"missing-preview","startedAtMs":0}}));
        assert!(
            result
                .expect_err("missing preview must fail")
                .contains("file-change preview")
        );
        assert!(p.approve("43", true).is_empty());
    }
    #[test]
    fn routine_mcp_activity_is_a_summary_without_arguments_or_results() {
        let item = json!({
            "type":"mcpToolCall", "server":"concat", "tool":"project_create", "status":"completed",
            "arguments":{"location":"/tmp/private-wire-input","video":{"width":1080}},
            "result":{"content":[{"type":"text","text":"{\"createdId\":\"private-wire-result\"}"}]}
        });
        let (_, text) = item_text(&item);
        assert!(!text.contains("private-wire-input"));
        assert!(!text.contains("private-wire-result"));
        assert!(text.contains("Создание проекта"));
    }

    #[test]
    fn command_output_deltas_do_not_flood_the_chat() {
        let mut p = protocol();
        ready(&mut p);
        let effects = p.receive(json!({"method":"item/commandExecution/outputDelta","params":{
            "threadId":"thread","turnId":"turn","itemId":"command","delta":"{\"wirePayload\":123}"
        }})).expect("command output");
        assert!(!effects.iter().any(|effect| matches!(effect,
            Effect::Show(Event::Message { text, .. }) if text.contains("wirePayload"))));
    }

    #[test]
    fn mcp_progress_does_not_expose_wire_payloads() {
        let mut p = protocol();
        ready(&mut p);
        let effects = p.receive(json!({"method":"item/mcpToolCall/progress","params":{
            "threadId":"thread","turnId":"turn","itemId":"tool","message":"{\"wireProgress\":50}"
        }})).expect("tool progress");
        assert!(!effects.iter().any(|effect| matches!(effect,
            Effect::Show(Event::Message { text, .. }) if text.contains("wireProgress"))));
    }

    #[test]
    fn failed_mcp_results_keep_a_readable_error() {
        let item = json!({"type":"mcpToolCall","server":"concat","tool":"project_create","status":"completed",
            "result":{"isError":true,"content":[{"type":"text","text":"{\"error\":{\"code\":\"refused\",\"message\":\"The project already exists\"}}"}]}});
        assert!(item_failed(&item));
        let (_, text) = item_text(&item);
        assert!(text.contains("The project already exists"));
        assert!(!text.contains("\"code\""));
    }

    #[test]
    fn the_agent_can_create_a_project_when_the_user_requests_it() {
        let instructions = protocol().instructions();
        assert!(!instructions.contains("Do not create, open, close, or switch projects"));
        assert!(instructions.contains("project_create"));
    }

    #[test]
    fn file_approval_preserves_the_full_change_preview() {
        let mut p = protocol();
        ready(&mut p);
        p.receive(json!({"method":"item/started","params":{"threadId":"thread","turnId":"turn",
            "item":{"id":"change","type":"fileChange","status":"inProgress",
                "changes":[{"kind":{"type":"add"},"path":"/tmp/review.txt","diff":"+reviewable line"}]}}})).expect("preview");
        let effects = p
            .receive(json!({"id":97,"method":"item/fileChange/requestApproval",
            "params":{"threadId":"thread","turnId":"turn","itemId":"change"}}))
            .expect("approval");
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::Show(Event::Approval { detail, .. })
                if detail.contains("/tmp/review.txt") && detail.contains("+reviewable line"))));
    }

    #[test]
    fn creating_the_first_project_keeps_the_active_conversation() {
        let mut p = protocol();
        p.context.project = None;
        p.context.path = "/tmp/workspace".into();
        ready(&mut p);
        let first = submit_turn(&mut p, "Create a vertical scene", "first");
        p.receive(
            json!({"method":"turn/started","params":{"threadId":"thread",
            "turn":{"id":"create","status":"inProgress","items":[]}}}),
        )
        .expect("started");
        let effects = p.update_context(Context {
            path: "/tmp/workspace".into(),
            project: Some("/tmp/workspace/Film".into()),
            name: "Film".into(),
            epoch: 8,
        });
        assert!(
            effects.is_empty(),
            "initial project creation must not interrupt its own turn"
        );
        assert_eq!(p.thread.as_deref(), Some("thread"));
        assert!(p.active);
        p.receive(
            json!({"method":"turn/completed","params":{"threadId":"thread",
            "turn":{"id":"create","status":"completed","items":[]}}}),
        )
        .expect("completed");
        let next = submit_turn(&mut p, "Add a title", "second");
        assert_eq!(first["params"]["threadId"], next["params"]["threadId"]);
        assert_eq!(next["params"]["input"][0]["text"], "Add a title");
        assert!(
            next["params"]["input"][1]["text"]
                .as_str()
                .expect("context")
                .contains("/tmp/workspace/Film")
        );
    }

    #[test]
    fn changing_an_existing_project_interrupts_work_and_revokes_pending_approvals() {
        let mut p = protocol();
        ready(&mut p);
        submit_turn(&mut p, "Edit", "user");
        p.receive(
            json!({"method":"turn/started","params":{"threadId":"thread",
            "turn":{"id":"edit","status":"inProgress","items":[]}}}),
        )
        .expect("started");
        p.receive(
            json!({"id":51,"method":"item/commandExecution/requestApproval",
            "params":{"threadId":"thread","turnId":"edit","itemId":"cmd","command":"echo review"}}),
        )
        .expect("approval");
        let mut changed = p.context.clone();
        changed.project = Some("/tmp/another project".into());
        changed.epoch += 1;
        let effects = p.update_context(changed);
        assert_eq!(
            write(&effects, "turn/interrupt")["params"]["turnId"],
            "edit"
        );
        assert!(p.approve("51", true).is_empty());
        let late = p
            .receive(
                json!({"id":52,"method":"item/commandExecution/requestApproval",
            "params":{"threadId":"thread","turnId":"edit","itemId":"late","command":"echo late"}}),
            )
            .expect("late");
        assert!(
            !late
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Event::Approval { .. })))
        );
        assert_eq!(p.thread.as_deref(), Some("thread"));
    }

    #[test]
    fn a_structured_api_error_is_visible_even_when_the_tool_transport_succeeded() {
        let item = json!({"type":"mcpToolCall","server":"concat","tool":"project_save","status":"completed",
            "result":{"content":[],"structuredContent":{"error":{"code":"refused","message":"The folder is read-only"}}}});
        assert!(item_failed(&item));
        let (_, text) = item_text(&item);
        assert!(text.contains("The folder is read-only"));
        assert!(!text.contains("\"code\""));
    }

    fn skill_path(version: &str) -> String {
        std::env::temp_dir()
            .join("concat-protocol-skills")
            .join(version)
            .join("SKILL.md")
            .to_string_lossy()
            .into_owned()
    }

    fn available_skill(protocol: &Protocol, path: &str) -> Value {
        json!({"data": [{
            "cwd": protocol.context.path.to_string_lossy(),
            "skills": [{
                "name": "steel0rat-video-maker-skill",
                "path": path,
                "enabled": true
            }],
            "errors": []
        }]})
    }

    fn refreshed_turn(protocol: &mut Protocol, effects: &[Effect]) -> Value {
        let refresh = write(effects, "skills/list");
        let result = available_skill(protocol, &skill_path("current"));
        let effects = protocol
            .receive(json!({"id": refresh["id"], "result": result}))
            .expect("skills refresh");
        write(&effects, "turn/start")
    }

    fn submit_turn(protocol: &mut Protocol, text: &str, id: &str) -> Value {
        let effects = protocol.submit(text.into(), id.into()).expect("submit");
        refreshed_turn(protocol, &effects)
    }

    #[test]
    fn default_skill_is_selected_and_refreshed_for_every_turn_in_same_thread() {
        let mut p = protocol();
        ready(&mut p);
        for (index, version) in ["first", "second"].iter().enumerate() {
            let id = format!("user-{index}");
            let effects = p
                .submit("Create an edit".into(), id.clone())
                .expect("submit");
            assert!(
                !effects.iter().any(
                    |effect| matches!(effect, Effect::Write(v) if v["method"] == "turn/start")
                )
            );
            let refresh = write(&effects, "skills/list");
            assert_eq!(refresh["params"]["forceReload"], true);
            assert_eq!(refresh["params"]["cwds"], json!(["/tmp/concat project"]));
            assert!(p.submit("duplicate".into(), "duplicate".into()).is_err());
            let path = skill_path(version);
            let result = available_skill(&p, &path);
            let effects = p
                .receive(json!({"id": refresh["id"], "result": result}))
                .expect("skills");
            let turn = write(&effects, "turn/start");
            assert_eq!(turn["params"]["threadId"], "thread");
            assert_eq!(turn["params"]["clientUserMessageId"], id);
            assert_eq!(turn["params"]["input"][0]["text"], "Create an edit");
            assert_eq!(
                turn["params"]["input"][2],
                json!({
                    "type": "skill", "name": "steel0rat-video-maker-skill", "path": path
                })
            );
            for field in ["model", "approvalPolicy", "sandbox", "approvalsReviewer"] {
                assert!(turn["params"].get(field).is_none(), "inherit {field}");
            }
            let turn_id = format!("turn-{index}");
            p.receive(json!({"method":"turn/completed","params":{
                "threadId":"thread","turn":{"id":turn_id,"status":"completed","items":[]}
            }}))
            .expect("complete");
        }
    }

    #[test]
    fn default_skill_unavailable_or_invalid_never_sends_a_model_turn() {
        let path = skill_path("valid");
        for skills in [
            json!([]),
            json!([{"name":"steel0rat-video-maker-skill","enabled":false,"path":path}]),
            json!([{"name":"steel0rat-video-maker-skill","enabled":true,"path":"relative/SKILL.md"}]),
            json!([{"name":"steel0rat-video-maker-skill","enabled":true,"path":std::env::temp_dir().join("not-a-skill.txt")}]),
            json!([{"name":"steel0rat-video-maker-skill","enabled":true,"path":path},
                   {"name":"steel0rat-video-maker-skill","enabled":true,"path":skill_path("other")}]),
        ] {
            let mut p = protocol();
            ready(&mut p);
            let effects = p
                .submit("Do not lose my prompt".into(), "user".into())
                .expect("submit");
            let refresh = write(&effects, "skills/list");
            let result =
                json!({"data":[{"cwd":"/tmp/concat project","skills":skills,"errors":[]}]});
            let effects = p
                .receive(json!({"id":refresh["id"],"result":result}))
                .expect("readable failure");
            assert!(
                !effects.iter().any(
                    |effect| matches!(effect, Effect::Write(v) if v["method"] == "turn/start")
                )
            );
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::Sent(_))))
            );
            assert!(effects.iter().any(|effect|
                matches!(effect, Effect::Show(Event::Error(text)) if text.contains("steel0rat-video-maker-skill"))));
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::Finished { failed: true })))
            );
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::State { busy: false, .. })))
            );
            assert!(p.queued.is_none());
            assert!(!p.active);
            assert_eq!(p.thread.as_deref(), Some("thread"));
            let retry = submit_turn(&mut p, "Retry after installing", "retry");
            assert_eq!(retry["params"]["threadId"], "thread");
        }
    }

    #[test]
    fn default_skill_refresh_failure_and_wrong_workspace_do_not_stick_busy() {
        for response in [
            json!({"error":{"code":-32603,"message":"catalog unavailable"}}),
            json!({"result":{"data":[{"cwd":"/another/workspace","skills":[],"errors":[]}]}}),
            json!({"result":{}}),
        ] {
            let mut p = protocol();
            ready(&mut p);
            let effects = p.submit("Keep this".into(), "user".into()).expect("submit");
            let refresh = write(&effects, "skills/list");
            let mut response = response;
            response["id"] = refresh["id"].clone();
            let effects = p.receive(response).expect("recoverable refresh error");
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::Finished { failed: true })))
            );
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::State { busy: false, .. })))
            );
            assert!(p.queued.is_none());
            assert!(!p.active);
            assert_eq!(p.thread.as_deref(), Some("thread"));
        }
    }

    #[test]
    fn default_skill_refresh_can_be_cancelled_and_late_reply_is_ignored() {
        let mut p = protocol();
        ready(&mut p);
        let effects = p
            .submit("Old request".into(), "old".into())
            .expect("submit");
        let refresh = write(&effects, "skills/list");
        p.pending
            .get_mut(&refresh["id"].as_u64().expect("id"))
            .expect("pending")
            .1 = Instant::now() - Duration::from_secs(121);
        let cancelled = p.interrupt();
        assert!(
            cancelled
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Event::State { busy: false, .. })))
        );
        assert!(!p.timed_out());
        assert!(p.queued.is_none());
        let effects = p
            .submit("Current request".into(), "current".into())
            .expect("resubmit");
        let current = write(&effects, "skills/list");
        let result = available_skill(&p, &skill_path("latest"));
        assert!(
            p.receive(json!({"id":refresh["id"],"result":result}))
                .expect("late")
                .is_empty()
        );
        let result = available_skill(&p, &skill_path("latest"));
        let effects = p
            .receive(json!({"id":current["id"],"result":result}))
            .expect("current");
        assert_eq!(
            write(&effects, "turn/start")["params"]["clientUserMessageId"],
            "current"
        );
    }

    #[test]
    fn default_skill_context_change_cancels_refresh_before_any_model_turn() {
        for starts_without_project in [false, true] {
            let mut p = protocol();
            if starts_without_project {
                p.context.project = None;
            }
            ready(&mut p);
            let effects = p
                .submit("Edit the old context".into(), "old".into())
                .expect("submit");
            let refresh = write(&effects, "skills/list");
            let mut changed = p.context.clone();
            changed.project = Some("/tmp/concat project/Changed".into());
            changed.epoch += 1;
            let effects = p.update_context(changed);
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::State { busy: false, .. })))
            );
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Show(Event::Error(_))))
            );
            assert!(p.queued.is_none());
            assert!(!p.active);
            let result = available_skill(&p, &skill_path("current"));
            assert!(
                p.receive(json!({"id":refresh["id"],"result":result}))
                    .expect("late")
                    .is_empty()
            );
            let current = submit_turn(&mut p, "Work on current project", "current");
            assert!(
                current["params"]["input"][1]["text"]
                    .as_str()
                    .expect("context")
                    .contains("/tmp/concat project/Changed")
            );
            assert_eq!(p.thread.as_deref(), Some("thread"));
        }
    }

    #[test]
    fn default_skill_context_change_cancels_input_queued_during_handshake() {
        let mut p = protocol();
        assert!(
            p.submit("Old request".into(), "old".into())
                .expect("queued")
                .is_empty()
        );
        let mut changed = p.context.clone();
        changed.epoch += 1;
        changed.project = Some("/tmp/concat project/Current".into());
        let effects = p.update_context(changed);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Event::State { busy: false, .. })))
        );
        assert!(p.queued.is_none());
        ready(&mut p);
        assert!(!p.active);
        assert_eq!(p.thread.as_deref(), Some("thread"));
        let next = submit_turn(&mut p, "Current request", "current");
        assert_eq!(next["params"]["clientUserMessageId"], "current");
    }

    #[test]
    fn accepted_user_is_reported_when_started_precedes_ack() {
        let mut p = protocol();
        ready(&mut p);
        let request = submit_turn(&mut p, "Apply this once", "user");
        let started = p
            .receive(json!({"method":"turn/started","params":{
                "threadId":"thread","turn":{"id":"accepted-turn","status":"inProgress","items":[]}
            }}))
            .expect("started before ack");
        assert!(
            started
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Event::Sent(id)) if id == "user"))
        );
        let completed = p
            .receive(json!({"method":"turn/completed","params":{
                "threadId":"thread","turn":{"id":"accepted-turn","status":"completed","items":[]}
            }}))
            .expect("completed");
        assert!(
            completed
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Event::Finished { failed: false })))
        );
        assert!(
            !completed
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Event::Sent(_))))
        );
        p.receive(json!({"id":request["id"],"result":{"turn":{
            "id":"accepted-turn","status":"completed","items":[]
        }}}))
        .expect("late ack");
        assert!(!p.active);
    }

    #[test]
    fn accepted_user_is_reported_before_finish_and_old_ack_cannot_accept_a_new_request() {
        let mut p = protocol();
        ready(&mut p);
        let previous = submit_turn(&mut p, "Previous edit", "previous-user");
        let completed = p
            .receive(json!({"method":"turn/completed","params":{
                "threadId":"thread","turn":{"id":"previous-turn","status":"completed","items":[]}
            }}))
            .expect("completed before ack");
        let sent = completed
            .iter()
            .position(
                |effect| matches!(effect, Effect::Show(Event::Sent(id)) if id == "previous-user"),
            )
            .expect("accepted user before completion");
        let finished = completed
            .iter()
            .position(|effect| matches!(effect, Effect::Show(Event::Finished { .. })))
            .expect("finished");
        assert!(sent < finished);

        submit_turn(&mut p, "Current edit", "current-user");
        let old_ack = p
            .receive(json!({"id":previous["id"],"result":{"turn":{
                "id":"previous-turn","status":"inProgress","items":[]
            }}}))
            .expect("late previous ack");
        assert!(
            !old_ack.iter().any(
                |effect| matches!(effect, Effect::Show(Event::Sent(id)) if id == "current-user")
            )
        );
        assert!(p.active);
        assert!(p.turn.is_none());

        let started = p
            .receive(json!({"method":"turn/started","params":{
                "threadId":"thread","turn":{"id":"current-turn","status":"inProgress","items":[]}
            }}))
            .expect("current accepted");
        assert!(
            started.iter().any(
                |effect| matches!(effect, Effect::Show(Event::Sent(id)) if id == "current-user")
            )
        );
        assert_eq!(p.turn.as_deref(), Some("current-turn"));
    }

    #[test]
    fn model_catalogue_uses_the_threads_provider_and_follows_pages() {
        let mut p = protocol();
        let init = p.initialize();
        let account = write(
            &p.receive(json!({"id":init["id"],"result":{}}))
                .expect("initialize"),
            "account/read",
        );
        let thread = write(
            &p.receive(
                json!({"id":account["id"],"result":{"requiresOpenaiAuth":false,"account":null}}),
            )
            .expect("account"),
            "thread/start",
        );
        let effects = p
            .receive(json!({"id":thread["id"],"result":{
                "thread":{"id":"thread"}, "model":"primary", "modelProvider":"provider",
                "reasoningEffort":"high"
            }}))
            .expect("thread");
        let first = write(&effects, "model/list");
        assert_eq!(first["params"]["modelProvider"], "provider");
        assert_eq!(first["params"]["includeHidden"], false);
        let effects = p
            .receive(json!({"id":first["id"],"result":{
                "data":[
                    {"id":"registry-id", "model":"primary","displayName":"Primary",
                     "defaultReasoningEffort":"medium","supportedReasoningEfforts":[
                        {"reasoningEffort":"medium"}, {"reasoningEffort":"high"}
                     ]},
                    {"model":"hidden","hidden":true}
                ],
                "nextCursor":"second-page"
            }}))
            .expect("first page");
        let second = write(&effects, "model/list");
        assert_eq!(second["params"]["cursor"], "second-page");
        assert_eq!(second["params"]["modelProvider"], "provider");
        let effects = p.receive(json!({"id":second["id"],"result":{
            "data":[{"model":"secondary","displayName":"Secondary","defaultReasoningEffort":"low",
                     "supportedReasoningEfforts":[{"reasoningEffort":"low"}]}]
        }})).expect("second page");
        let Effect::Show(Event::Models { current, options }) = &effects[0] else {
            panic!("missing completed model list");
        };
        assert_eq!(current.model.as_deref(), Some("primary"));
        assert_eq!(current.effort.as_deref(), Some("high"));
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].model, "primary");
        assert_eq!(options[0].label, "Primary");
        assert_eq!(options[0].efforts, ["medium", "high"]);
        assert_eq!(options[0].default_effort.as_deref(), Some("medium"));
        assert_eq!(options[1].efforts, ["low"]);
    }

    #[test]
    fn selected_model_and_effort_apply_to_the_next_turn_in_the_same_thread() {
        let mut p = protocol();
        ready(&mut p);
        let original = submit_turn(&mut p, "First edit", "first");
        assert!(original["params"].get("model").is_none());
        assert!(original["params"].get("effort").is_none());
        p.receive(json!({"id":original["id"],"result":{"turn":{
            "id":"first-turn","status":"completed","items":[]
        }}}))
        .expect("first turn completed");

        p.selection = ModelSelection {
            model: Some("secondary".into()),
            effort: Some("xhigh".into()),
        };
        let turn = submit_turn(&mut p, "Another edit", "second");
        let params = &turn["params"];
        assert_eq!(params["threadId"], original["params"]["threadId"]);
        assert_eq!(params["model"], "secondary");
        assert_eq!(params["effort"], "xhigh");
        assert_eq!(params["input"][2]["name"], DEFAULT_SKILL);
        for field in [
            "approvalPolicy",
            "approvalsReviewer",
            "sandboxPolicy",
            "serviceTier",
        ] {
            assert!(params.get(field).is_none(), "inherit {field}");
        }
    }

    #[test]
    fn expired_model_list_does_not_block_chat_or_accept_a_late_page() {
        let mut p = protocol();
        ready(&mut p);
        let id = p
            .pending
            .iter_mut()
            .find_map(|(&id, (kind, since))| {
                if matches!(kind, Pending::Models(_)) {
                    *since = Instant::now() - Duration::from_secs(121);
                    Some(id)
                } else {
                    None
                }
            })
            .expect("catalogue pending");
        assert!(matches!(
            p.expire_model_list(),
            Some(Effect::Show(Event::ModelsUnavailable))
        ));
        assert!(!p.timed_out());
        assert!(
            p.receive(json!({"id":id,"result":{"data":[]}}))
                .expect("late list")
                .is_empty()
        );
        let turn = submit_turn(&mut p, "Keep working", "user");
        assert_eq!(turn["params"]["threadId"], "thread");
        assert!(turn["params"].get("model").is_none());
    }
    #[test]
    fn opening_a_saved_chat_restores_messages_then_continues_the_same_thread() {
        let mut p = protocol();
        ready(&mut p);
        let history = write(
            &p.change_chat(Some("saved".into())).expect("open"),
            "thread/turns/list",
        );
        assert_eq!(history["params"]["threadId"], "saved");
        assert_eq!(history["params"]["itemsView"], "summary");
        assert_eq!(p.thread.as_deref(), Some("thread"));
        let effects = p.receive(json!({"id":history["id"],"result":{"nextCursor":"older","data":[
            {"id":"newest","items":[{"id":"answer","type":"agentMessage","text":"Here is the edit."}]},
            {"id":"original","items":[{"id":"user","type":"userMessage","content":[
                {"type":"text","text":"Show the first frame"},
                {"type":"text","text":"Application context: working folder /tmp/private."},
                {"type":"skill","name":"video"}
            ]},{"id":"cmd","type":"commandExecution","aggregatedOutput":"hidden JSON"}]}
        ]}})).expect("transcript");
        let resume = write(&effects, "thread/resume");
        assert_eq!(resume["params"]["threadId"], "saved");
        assert_eq!(resume["params"]["cwd"], "/tmp/concat project");
        assert_eq!(resume["params"]["excludeTurns"], true);
        assert!(resume["params"].get("approvalPolicy").is_none());
        assert_eq!(p.thread.as_deref(), Some("thread"));
        let effects = p
            .receive(json!({"id":resume["id"],"result":{"thread":{"id":"saved"}}}))
            .expect("resumed");
        let Some(Effect::Show(Event::ChatOpened { id, page })) = effects.first() else {
            panic!("chat not opened");
        };
        assert_eq!(id, "saved");
        assert_eq!(page.older.as_deref(), Some("older"));
        assert_eq!(
            page.messages
                .iter()
                .map(|m| (m.role.as_str(), m.text.as_str()))
                .collect::<Vec<_>>(),
            [
                ("You", "Show the first frame"),
                ("Codex", "Here is the edit.")
            ]
        );
        let older = write(
            &[p.older_messages("older".into()).expect("older page")],
            "thread/turns/list",
        );
        assert_eq!(older["params"]["cursor"], "older");
        assert_eq!(older["params"]["threadId"], "saved");
        let next = submit_turn(&mut p, "Add titles", "next");
        assert_eq!(next["params"]["threadId"], "saved");
        assert_eq!(next["params"]["input"][2]["name"], DEFAULT_SKILL);
    }

    #[test]
    fn restoring_a_projects_chat_on_start_opens_the_transcript_before_resuming() {
        let mut p = protocol();
        p.resume = Some("remembered".into());
        p.restore_history = true;
        let init = p.initialize();
        let account = write(
            &p.receive(json!({"id":init["id"],"result":{}})).unwrap(),
            "account/read",
        );
        assert!(p.submit("Wait".into(), "user".into()).is_err());
        let history = write(
            &p.receive(json!({"id":account["id"],"result":{"requiresOpenaiAuth":false}}))
                .unwrap(),
            "thread/turns/list",
        );
        assert_eq!(history["params"]["threadId"], "remembered");
        let resume = write(
            &p.receive(json!({"id":history["id"],"result":{"data":[
                {"id":"turn","items":[{"id":"answer","type":"agentMessage","text":"Previously"}]}
            ]}}))
            .unwrap(),
            "thread/resume",
        );
        assert!(!p.connected());
        assert_eq!(resume["params"]["threadId"], "remembered");
        let effects = p
            .receive(json!({"id":resume["id"],"result":{"thread":{"id":"remembered"}}}))
            .unwrap();
        let Some(Effect::Show(Event::ChatOpened { id, page })) = effects.first() else {
            panic!("chat not restored");
        };
        assert_eq!(id, "remembered");
        assert_eq!(page.messages[0].text, "Previously");
        assert_eq!(
            submit_turn(&mut p, "Continue", "user")["params"]["threadId"],
            "remembered"
        );
    }

    #[test]
    fn failing_to_restore_on_start_does_not_create_a_replacement_thread() {
        let mut p = protocol();
        let request = p.transcript_page("saved", None, Pending::OpenTranscript("saved".into()));
        let result = p.receive(json!({"id":request["id"],"error":{"code":-32602}}));
        assert!(
            result
                .unwrap_err()
                .contains("Could not restore the last chat")
        );
        assert!(!p.connected());
    }

    #[test]
    fn failed_chat_restore_leaves_the_current_chat_sendable() {
        let mut p = protocol();
        ready(&mut p);
        let history = write(
            &p.change_chat(Some("stale".into())).expect("load"),
            "thread/turns/list",
        );
        let effects = p
            .receive(json!({"id":history["id"],"error":{"code":-32602,"message":"missing"}}))
            .expect("failure event");
        assert!(matches!(
            effects.as_slice(),
            [Effect::Show(Event::ChatOpenFailed)]
        ));
        assert_eq!(p.thread.as_deref(), Some("thread"));
        assert_eq!(
            submit_turn(&mut p, "Still in this chat", "user")["params"]["threadId"],
            "thread"
        );
    }

    #[test]
    fn saved_chats_are_ordered_by_recency_and_carry_no_transcript_payloads() {
        let mut p = protocol();
        ready(&mut p);
        let requests = p.list_chats(vec!["old".into(), "new".into(), "foreign".into()]);
        assert_eq!(requests.len(), 3);
        let mut events = Vec::new();
        for effect in requests {
            let Effect::Write(request) = effect else {
                panic!("expected metadata request");
            };
            assert_eq!(request["method"], "thread/read");
            assert_eq!(request["params"]["includeTurns"], false);
            let id = request["params"]["threadId"].as_str().expect("id");
            let originator = if id == "foreign" {
                "another_app"
            } else {
                CLIENT_NAME
            };
            events.extend(
                p.receive(json!({"id":request["id"],"result":{"thread":{
                    "id":id,"originator":originator,"preview":"Title","cwd":"/workspace",
                    "updatedAt":if id == "old" {1} else {2}, "turns":[{"items":[{"text":"raw"}]}]
                }}}))
                .expect("metadata"),
            );
        }
        let [Effect::Show(Event::SavedChats { chats, incomplete })] = events.as_slice() else {
            panic!("no catalogue");
        };
        assert!(*incomplete);
        assert_eq!(
            chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["new", "old"]
        );
    }
}
