// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! The configured edit boundaries and their snapshot for the chat's live calls.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use concat_host::session::EditorView;
use concat_project::Command;
use serde_json::json;
use slint::{ComponentHandle, Model, VecModel};

use super::edit_bounds::{EditBounds, ScopeViolation};
use super::{AgentPane, agent_client_prefix};
use crate::format::frames_timecode;
use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::Studio;
use crate::ui::{Agent, AgentEditTrackData};

#[derive(Clone, Default)]
pub(crate) struct TurnAccess {
    pub(crate) scope: Option<Arc<LiveEditScope>>,
    pub(crate) read_only: bool,
}

struct ClientScope {
    generation: u64,
    access: TurnAccess,
}

static CLIENT_SCOPE: Mutex<ClientScope> = Mutex::new(ClientScope {
    generation: 0,
    access: TurnAccess {
        scope: None,
        read_only: false,
    },
});

pub(super) fn publish_turn_scope(
    generation: u64,
    scope: Option<Arc<LiveEditScope>>,
    read_only: bool,
) {
    *CLIENT_SCOPE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = ClientScope {
        generation,
        access: TurnAccess { scope, read_only },
    };
}

pub(crate) fn turn_access(client: Option<&str>) -> Result<TurnAccess, String> {
    let Some(generation) = client
        .and_then(|id| id.strip_prefix(agent_client_prefix().as_str()))
        .and_then(|generation| generation.parse::<u64>().ok())
    else {
        return Ok(TurnAccess::default());
    };
    let registered = CLIENT_SCOPE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if registered.generation != generation {
        return Err(t("agent.scopeEnded"));
    }
    Ok(registered.access.clone())
}

pub(crate) struct LiveEditScope {
    epoch: u64,
    bounds: EditBounds,
}

impl LiveEditScope {
    pub(super) fn bounds(&self) -> &EditBounds {
        &self.bounds
    }

    pub(super) fn rebind(bounds: EditBounds, studio: &Studio) -> Result<Arc<Self>, String> {
        let scope = Self {
            epoch: host::project_epoch(),
            bounds,
        };
        if scope.matches(studio) {
            Ok(Arc::new(scope))
        } else {
            Err(t("agent.scopeStale"))
        }
    }

    fn matches(&self, studio: &Studio) -> bool {
        host::project_epoch() == self.epoch
            && studio.session.as_ref().is_some_and(|session| {
                let timeline = session.project().active();
                timeline.id == self.bounds.timeline
                    && self
                        .bounds
                        .tracks
                        .iter()
                        .all(|id| timeline.track(id).is_some())
            })
    }

    fn label(&self, rate: f32) -> String {
        let tracks = self
            .bounds
            .tracks
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let timing = self
            .bounds
            .range
            .map_or_else(|| t("agent.scopeAllTime"), |range| time_label(range, rate));
        format!("{} · {timing}", tf("agent.scopeSummary", &[&tracks]))
    }

    pub(crate) fn apply(
        &self,
        studio: &mut Studio,
        command: Command,
    ) -> Result<EditorView, String> {
        let result = if self.matches(studio) {
            studio.try_apply_checked(command, |before, after| {
                self.bounds
                    .check(before, after)
                    .map_err(|violation| match violation {
                        ScopeViolation::Timeline => t("agent.scopeTimeline"),
                        ScopeViolation::SharedAssets => t("agent.scopeAssets"),
                        ScopeViolation::Track(track) => tf("agent.scopeOutsideTrack", &[&track]),
                        ScopeViolation::Range(clip) => tf("agent.scopeOutsideRange", &[&clip]),
                    })
            })
        } else {
            Err(t("agent.scopeStale"))
        };
        result.inspect_err(|error| {
            super::with_pane(|pane| {
                pane.error.clone_from(error);
                pane.publish();
            });
        })
    }
}

#[derive(Default)]
pub(super) struct EditScopePane {
    saved: Option<Arc<LiveEditScope>>,
    draft: Option<ScopeDraft>,
    rows: Rc<VecModel<AgentEditTrackData>>,
    error: String,
}

struct ScopeDraft {
    epoch: u64,
    timeline: String,
    tracks: BTreeSet<String>,
    limit_range: bool,
}

impl ScopeDraft {
    fn matches(&self, studio: &Studio) -> bool {
        self.epoch == host::project_epoch()
            && studio
                .session
                .as_ref()
                .is_some_and(|session| session.project().active_timeline_id == self.timeline)
    }

    fn tracks_available(&self, studio: &Studio) -> bool {
        !self.tracks.is_empty()
            && self
                .tracks
                .iter()
                .all(|id| studio.timeline().track(id).is_some())
    }
}

fn time_label([start, end]: [f64; 2], rate: f32) -> String {
    format!(
        "{}–{}",
        frames_timecode(start as f32, rate),
        frames_timecode(end as f32, rate)
    )
}

impl AgentPane {
    pub(super) fn saved_edit_bounds(&self) -> Option<&EditBounds> {
        self.edit_scope.saved.as_ref().map(|scope| scope.bounds())
    }

    pub(super) fn install_scope_model(&self) {
        if let Some(app) = self.ui.upgrade() {
            app.global::<Agent>()
                .set_edit_scope_tracks(self.edit_scope.rows.clone().into());
        }
    }

    pub(super) fn close_edit_scope(&mut self) {
        self.edit_scope.draft = None;
        self.edit_scope.error.clear();
    }

    pub(super) fn sync_edit_scope(&mut self, studio: &Studio) {
        let scope = &self.edit_scope;
        if scope
            .saved
            .as_ref()
            .is_some_and(|saved| studio.session.is_none() || saved.epoch != host::project_epoch())
        {
            self.edit_scope.saved = None;
        }
        if self
            .edit_scope
            .draft
            .as_ref()
            .is_some_and(|draft| !draft.matches(studio))
        {
            self.close_edit_scope();
        }
    }

    pub(super) fn publish_scope_controls(&self, global: &Agent<'_>) {
        global.set_edit_scope_open(self.edit_scope.draft.is_some());
        global.set_edit_scope_active(self.edit_scope.saved.is_some());
        global.set_edit_scope_error(self.edit_scope.error.as_str().into());
    }

    pub(super) fn publish_edit_scope(&self, studio: &Studio, global: &Agent<'_>) {
        self.publish_scope_controls(global);
        let scope = &self.edit_scope;
        global.set_edit_scope_summary(
            scope
                .saved
                .as_ref()
                .map_or_else(String::new, |saved| {
                    if saved.matches(studio) {
                        saved.label(studio.frame_rate())
                    } else {
                        t("agent.scopeStale")
                    }
                })
                .into(),
        );
        let Some(draft) = &scope.draft else {
            global.set_edit_scope_can_save(false);
            return;
        };
        let range = self.scope_range(studio);
        global.set_edit_scope_limit_range(draft.limit_range);
        global.set_edit_scope_range_ready(range.is_some());
        global.set_edit_scope_range_label(
            range
                .map_or_else(
                    || t("agent.scopeNeedRange"),
                    |range| time_label(range, studio.frame_rate()),
                )
                .into(),
        );
        global.set_edit_scope_can_save(
            self.can_configure_scope()
                && draft.matches(studio)
                && draft.tracks_available(studio)
                && (!draft.limit_range || range.is_some()),
        );
        let rows: Vec<_> = studio
            .timeline()
            .tracks
            .iter()
            .rev()
            .map(|track| AgentEditTrackData {
                id: track.id.as_str().into(),
                label: tf("studio.track", &[&track.id]).into(),
                selected: draft.tracks.contains(&track.id),
            })
            .collect();
        if scope.rows.row_count() != rows.len()
            || scope
                .rows
                .iter()
                .zip(&rows)
                .any(|(old, new)| old.id != new.id)
        {
            scope.rows.set_vec(rows);
        } else {
            for (index, row) in rows.into_iter().enumerate() {
                if scope.rows.row_data(index).as_ref() != Some(&row) {
                    scope.rows.set_row_data(index, row);
                }
            }
        }
    }

    fn can_configure_scope(&self) -> bool {
        self.enabled()
            && !self.busy
            && !self.maintenance
            && !self.pause_analysis_running()
            && !self.dictation.active()
            && self
                .context
                .as_ref()
                .is_some_and(|context| context.project.is_some())
    }

    fn scope_range(&self, studio: &Studio) -> Option<[f64; 2]> {
        self.range
            .as_ref()?
            .bounds(studio)
            .filter(|[start, end]| start.is_finite() && end.is_finite() && start < end)
    }

    fn refresh_edit_scope(&self) {
        self.publish();
        Shell::with(|shell, app| {
            if let Ok(studio) = shell.studio.try_borrow() {
                self.publish_edit_scope(&studio, &app.global::<Agent>());
            }
        });
    }

    pub(super) fn toggle_edit_scope(&mut self) {
        if self.edit_scope.draft.is_some() {
            self.close_edit_scope();
            self.refresh_edit_scope();
            return;
        }
        if !self.can_configure_scope() {
            return;
        }
        Shell::with(|shell, _| {
            if let Ok(studio) = shell.studio.try_borrow()
                && let Some(session) = &studio.session
            {
                let timeline = session.project().active();
                let saved = self
                    .edit_scope
                    .saved
                    .as_ref()
                    .filter(|saved| saved.matches(&studio));
                let tracks = saved.map_or_else(
                    || {
                        studio
                            .selection
                            .iter()
                            .filter_map(|id| timeline.clip(id))
                            .map(|clip| clip.track_id.clone())
                            .collect()
                    },
                    |saved| saved.bounds.tracks.clone(),
                );
                self.edit_scope.draft = Some(ScopeDraft {
                    epoch: host::project_epoch(),
                    timeline: timeline.id.clone(),
                    tracks,
                    limit_range: saved.map_or_else(
                        || self.scope_range(&studio).is_some(),
                        |saved| saved.bounds.range.is_some(),
                    ),
                });
            }
        });
        if self.edit_scope.draft.is_some() {
            self.history.open = false;
            self.close_queue();
            self.close_notes_view();
            self.close_brief_view();
        }
        self.refresh_edit_scope();
    }

    pub(super) fn edit_scope_track(&mut self, id: &str, included: bool) {
        if !self.can_configure_scope() || !self.edit_scope.rows.iter().any(|row| row.id == id) {
            return;
        }
        if let Some(draft) = &mut self.edit_scope.draft {
            if included {
                draft.tracks.insert(id.to_owned());
            } else {
                draft.tracks.remove(id);
            }
            self.edit_scope.error.clear();
            self.refresh_edit_scope();
        }
    }

    pub(super) fn edit_scope_limit_range(&mut self, enabled: bool) {
        if self.can_configure_scope()
            && let Some(draft) = &mut self.edit_scope.draft
        {
            draft.limit_range = enabled;
            self.edit_scope.error.clear();
            self.refresh_edit_scope();
        }
    }

    pub(super) fn save_edit_scope(&mut self) {
        if !self.can_configure_scope() || self.edit_scope.draft.is_none() {
            return;
        }
        let mut next = Err(t("agent.scopeStale"));
        Shell::with(|shell, _| {
            if let Ok(studio) = shell.studio.try_borrow()
                && let Some(draft) = &self.edit_scope.draft
                && draft.matches(&studio)
            {
                let range = self.scope_range(&studio);
                next = if !draft.tracks_available(&studio) {
                    Err(t("agent.scopeNeedTracks"))
                } else if draft.limit_range && range.is_none() {
                    Err(t("agent.scopeNeedRange"))
                } else {
                    Ok(Arc::new(LiveEditScope {
                        epoch: draft.epoch,
                        bounds: EditBounds {
                            timeline: draft.timeline.clone(),
                            tracks: draft.tracks.clone(),
                            range: if draft.limit_range { range } else { None },
                        },
                    }))
                };
            }
        });
        match next {
            Ok(scope) => {
                self.edit_scope.saved = Some(scope);
                self.close_edit_scope();
            }
            Err(error) => self.edit_scope.error = error,
        }
        self.refresh_edit_scope();
    }

    pub(super) fn clear_edit_scope(&mut self) {
        if self.can_configure_scope() {
            self.edit_scope.saved = None;
            self.close_edit_scope();
            self.refresh_edit_scope();
        }
    }

    pub(super) fn append_edit_scope(
        &self,
        detail: &mut String,
        labels: &mut Vec<String>,
    ) -> Result<Option<Arc<LiveEditScope>>, String> {
        let Some(scope) = &self.edit_scope.saved else {
            return Ok(None);
        };
        let mut label = None;
        Shell::with(|shell, _| {
            if let Ok(studio) = shell.studio.try_borrow()
                && scope.matches(&studio)
            {
                label = Some(scope.label(studio.frame_rate()));
            }
        });
        labels.push(label.ok_or_else(|| t("agent.scopeStale"))?);
        detail.push_str(&format!(
            " Enforced live edit scope {}. Keep each changed clip entirely on these tracks and inside the range when supplied. Use Concat live editing tools for changes.",
            json!({
                "timelineId": scope.bounds.timeline,
                "trackIds": scope.bounds.tracks,
                "rangeSeconds": scope.bounds.range,
            }),
        ));
        Ok(Some(scope.clone()))
    }
}
