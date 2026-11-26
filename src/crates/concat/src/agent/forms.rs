// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Project brief and parameters shown before sending a saved command.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::json;
use slint::{ComponentHandle, Model, VecModel};

use super::brief::{BriefStore, ProjectBrief};
use super::turns::TimelineInput;
use super::{AgentPane, PANE, Submission, with_pane};
use crate::host::{self, Shell};
use crate::studio::{Gesture, Studio};
use crate::ui::{Agent, AgentParameterData, PaneKind};

#[derive(Default)]
pub(super) struct AgentForms {
    brief: BriefPane,
    parameters: Option<ParameterForm>,
}

#[derive(Default)]
struct BriefPane {
    folder: Option<PathBuf>,
    store: Option<BriefStore>,
    drafts: HashMap<PathBuf, ProjectBrief>,
    open: bool,
    error: String,
}

struct ParameterForm {
    id: String,
    title: String,
    timeline: TimelineInput,
    timeline_id: String,
    epoch: u64,
    revision: u64,
    fields: Rc<VecModel<AgentParameterData>>,
    error: String,
}

impl ParameterForm {
    fn matches(&self, studio: &Studio) -> bool {
        self.epoch == host::project_epoch()
            && self.revision == studio.edit_revision()
            && self.timeline_id == studio.timeline().id
            && matches!(studio.gesture, Gesture::None)
            && studio.confirm.is_none()
    }
}

pub(crate) fn pinned_shortcuts() -> Vec<(String, String)> {
    PANE.try_with(|slot| {
        let pane = slot.try_borrow().ok()?;
        let actions = pane.as_ref()?.quick_actions.as_ref()?;
        Some(
            actions
                .rows()
                .into_iter()
                .filter(|row| row.pinned)
                .map(|row| (row.id.to_string(), row.label.to_string()))
                .collect(),
        )
    })
    .ok()
    .flatten()
    .unwrap_or_default()
}

pub(crate) fn handle_shortcut(action: &str) -> bool {
    let mut handled = false;
    with_pane(|pane| {
        if !pane.enabled() || pane.maintenance || pane.history.opening.is_some() {
            return;
        }
        match action {
            "agent-dictation" => {
                let Some(app) = pane.ui.upgrade() else { return };
                if !app.global::<Agent>().get_dictation_available() {
                    return;
                }
                pane.open_chat_view();
                if pane.dictation.active() {
                    pane.dictation.stop();
                } else {
                    pane.start_dictation();
                }
                pane.publish();
                handled = true;
            }
            "agent-new-note" => handled = pane.open_note_from_shortcut(),
            action => {
                let Some(id) = action
                    .strip_prefix("agent-")
                    .filter(|id| id.starts_with("pin-"))
                else {
                    return;
                };
                if pane
                    .context
                    .as_ref()
                    .is_none_or(|context| context.project.is_none())
                    || !pane.quick_actions.as_ref().is_some_and(|actions| {
                        actions.prompt(id).is_some() || actions.template_fields(id).is_some()
                    })
                {
                    return;
                }
                pane.open_chat_view();
                pane.run_quick_action(id);
                pane.publish();
                handled = true;
            }
        }
    });
    handled
}

pub(super) fn reveal_agent_pane() {
    host::on_ui(|studio, _, _| {
        if studio.live_enabled && studio.session.is_some() && !studio.dock.contains(PaneKind::Agent)
        {
            crate::add_pane(studio, PaneKind::Agent);
        }
    });
}

fn brief_draft(global: &Agent) -> ProjectBrief {
    ProjectBrief {
        goal: global.get_brief_goal().to_string(),
        audience: global.get_brief_audience().to_string(),
        style: global.get_brief_style().to_string(),
    }
}

fn show_brief_draft(global: &Agent, brief: &ProjectBrief) {
    global.set_brief_goal(brief.goal.as_str().into());
    global.set_brief_audience(brief.audience.as_str().into());
    global.set_brief_style(brief.style.as_str().into());
}

impl AgentPane {
    pub(super) fn sync_forms(&mut self, studio: &Studio) {
        let folder = studio
            .session
            .as_ref()
            .map(|session| PathBuf::from(session.path()));
        if self.forms.brief.folder == folder {
            return;
        }
        self.cancel_parameter_form();
        let state = &mut self.forms.brief;
        if let Some(app) = self.ui.upgrade() {
            let global = app.global::<Agent>();
            let draft = brief_draft(&global);
            if let Some(previous) = state.folder.take()
                && state.store.as_ref().map_or_else(
                    || draft != ProjectBrief::default(),
                    |store| store.brief() != &draft,
                )
            {
                state.drafts.insert(previous, draft);
            }
            state.error.clear();
            state.store = folder
                .as_ref()
                .and_then(|path| match BriefStore::load(path) {
                    Ok(store) => Some(store),
                    Err(error) => {
                        state.error = error;
                        None
                    }
                });
            let displayed = folder
                .as_ref()
                .and_then(|path| state.drafts.remove(path))
                .or_else(|| state.store.as_ref().map(|store| store.brief().clone()))
                .unwrap_or_default();
            show_brief_draft(&global, &displayed);
        }
        state.open &= folder.is_some();
        state.folder = folder;
        self.publish();
    }

    pub(super) fn forms_have_drafts(&self) -> bool {
        let state = &self.forms.brief;
        !state.drafts.is_empty()
            || self.ui.upgrade().is_some_and(|app| {
                let draft = brief_draft(&app.global::<Agent>());
                state.store.as_ref().map_or_else(
                    || draft != ProjectBrief::default(),
                    |store| store.brief() != &draft,
                )
            })
            || self
                .forms
                .parameters
                .as_ref()
                .is_some_and(|form| form.fields.iter().any(|field| !field.value.is_empty()))
    }

    pub(super) fn publish_forms(&self, global: &Agent) {
        let state = &self.forms.brief;
        global.set_brief_open(state.open);
        global.set_brief_can_save(state.store.is_some() && self.enabled() && !self.maintenance);
        global.set_brief_error(state.error.as_str().into());
        let form = self.forms.parameters.as_ref();
        global.set_parameter_error(form.map_or("", |form| form.error.as_str()).into());
        global.set_parameter_can_send(
            self.can_submit()
                && form.is_some_and(|form| {
                    form.fields
                        .iter()
                        .all(|field| !field.value.trim().is_empty())
                }),
        );
    }

    fn open_chat_view(&mut self) {
        self.close_queue();
        self.close_edit_scope();
        self.history.open = false;
        self.close_notes_view();
        self.close_brief_view();
        reveal_agent_pane();
    }

    pub(super) fn toggle_brief(&mut self) {
        if self.forms.brief.folder.is_none()
            || !self.enabled()
            || self.maintenance
            || self.dictation.active()
        {
            return;
        }
        self.forms.brief.open = !self.forms.brief.open;
        if self.forms.brief.open {
            self.history.open = false;
            self.close_notes_view();
        }
        self.publish();
    }

    pub(super) fn close_brief_view(&mut self) {
        self.forms.brief.open = false;
    }

    pub(super) fn save_brief(&mut self) {
        if !self.enabled() || self.maintenance {
            return;
        }
        let Some(app) = self.ui.upgrade() else { return };
        let Some(store) = &mut self.forms.brief.store else {
            return;
        };
        let global = app.global::<Agent>();
        let draft = brief_draft(&global);
        match store.save(&draft.goal, &draft.audience, &draft.style) {
            Ok(()) => {
                show_brief_draft(&global, store.brief());
                self.forms.brief.error.clear();
                self.close_brief_view();
            }
            Err(error) => self.forms.brief.error = error,
        }
        self.publish();
    }

    pub(super) fn append_brief(&self, detail: &mut String) {
        let state = &self.forms.brief;
        if self
            .context
            .as_ref()
            .and_then(|context| context.project.as_deref())
            != state.folder.as_deref()
        {
            return;
        }
        let Some(brief) = state.store.as_ref().map(BriefStore::brief) else {
            return;
        };
        if brief != &ProjectBrief::default() {
            detail.push_str(&format!(
                " Saved project brief, user-provided context for this project {}.",
                json!({"goal": brief.goal, "audience": brief.audience, "style": brief.style}),
            ));
        }
    }

    pub(super) fn can_open_command(&mut self, id: &str) -> bool {
        if let Some(form) = &mut self.forms.parameters
            && form.id != id
        {
            form.error = "Finish or cancel this command before opening another.".into();
            self.publish();
            return false;
        }
        true
    }

    pub(super) fn open_parameter_form(&mut self, id: &str, names: Vec<String>) {
        let mut target = None;
        Shell::with(|shell, _| {
            if let Ok(studio) = shell.studio.try_borrow()
                && matches!(studio.gesture, Gesture::None)
                && studio.confirm.is_none()
                && let Some(timeline) = TimelineInput::capture(&studio, self.range.as_ref())
                    .filter(TimelineInput::has_target)
            {
                target = Some((
                    timeline,
                    studio.timeline().id.clone(),
                    studio.edit_revision(),
                ));
            }
        });
        let Some((timeline, timeline_id, revision)) = target else {
            self.error = crate::i18n::t("chatUi.selectTargetFirst");
            self.publish();
            return;
        };
        let Some(app) = self.ui.upgrade() else { return };
        let global = app.global::<Agent>();
        let title = self
            .quick_actions
            .as_ref()
            .and_then(|actions| actions.rows().into_iter().find(|row| row.id.as_str() == id))
            .map(|row| row.label.to_string())
            .unwrap_or_default();
        let fields = self
            .forms
            .parameters
            .as_ref()
            .filter(|form| form.id == id)
            .map_or_else(
                || {
                    Rc::new(VecModel::from(
                        names
                            .into_iter()
                            .map(|name| AgentParameterData {
                                name: name.into(),
                                value: Default::default(),
                            })
                            .collect::<Vec<_>>(),
                    ))
                },
                |form| form.fields.clone(),
            );
        let form = ParameterForm {
            id: id.to_owned(),
            title,
            timeline,
            timeline_id,
            epoch: host::project_epoch(),
            revision,
            fields,
            error: String::new(),
        };
        self.open_chat_view();
        global.set_parameter_fields(form.fields.clone().into());
        global.set_parameter_title(form.title.as_str().into());
        global.set_parameter_scope(form.timeline.label().into());
        self.forms.parameters = Some(form);
        self.publish();
    }

    pub(super) fn edit_parameter(&mut self, index: i32, value: &str) {
        if let Ok(index) = usize::try_from(index)
            && let Some(form) = &mut self.forms.parameters
            && let Some(mut field) = form.fields.row_data(index)
        {
            field.value = value.into();
            form.fields.set_row_data(index, field);
            form.error.clear();
            self.publish();
        }
    }

    pub(super) fn cancel_parameter_form(&mut self) {
        if self.forms.parameters.take().is_some()
            && let Some(app) = self.ui.upgrade()
        {
            let global = app.global::<Agent>();
            global.set_parameter_title("".into());
            global.set_parameter_scope("".into());
            global.set_parameter_fields(Default::default());
        }
    }

    pub(super) fn submit_parameter_form(&mut self) {
        if !self.can_submit() {
            return;
        }
        let Some(form) = &mut self.forms.parameters else {
            return;
        };
        let mut current = false;
        Shell::with(|shell, _| {
            if let Ok(studio) = shell.studio.try_borrow() {
                current = form.matches(&studio);
            }
        });
        if !current {
            form.error =
                "The timeline changed. Reopen the pinned command to use the current selection."
                    .into();
            self.publish();
            return;
        }
        let values: Vec<_> = form
            .fields
            .iter()
            .map(|field| field.value.to_string())
            .collect();
        let rendered = self
            .quick_actions
            .as_ref()
            .ok_or("The pinned command is unavailable.".to_owned())
            .and_then(|actions| actions.render_template(&form.id, &values));
        match rendered {
            Ok(text) => {
                let timeline = form.timeline.clone();
                if self.send_prompt(text, Some(timeline), Submission::QuickAction, String::new()) {
                    self.cancel_parameter_form();
                } else if let Some(form) = &mut self.forms.parameters {
                    form.error.clone_from(&self.error);
                }
            }
            Err(error) => form.error = error,
        }
        self.publish();
    }
}
