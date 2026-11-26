// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! A review holds the exact revision and clip intervals the user saw.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use concat_host::playback::Playback;
use concat_project::Command;
use concat_project::commands::{ClipPause, validate_clip_pauses};
use concat_project::model::{Clip, ClipKind};
use serde_json::json;
use slint::{ComponentHandle, VecModel};

use super::{AgentPane, Submission, agent_client_id, pause_audio, with_pane};
use crate::format::frames_timecode;
use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::studio::{Gesture, Studio};
use crate::ui::{Agent, PauseCutData};

const MAX_PAUSE_CUTS: usize = 64;
static REVIEW_CLIENT: Mutex<Option<String>> = Mutex::new(None);

pub(crate) fn is_pause_review_client(client: Option<&str>) -> bool {
    client.is_some()
        && REVIEW_CLIENT
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_deref()
            == client
}

pub(crate) fn stage_pause_review(
    studio: &Studio,
    client: Option<&str>,
    ids: &[u32],
) -> Result<(), String> {
    if !is_pause_review_client(client) {
        return Err("Pause proposals belong to the active Trim pauses turn.".into());
    }
    let mut result = Err("The pause review is no longer open.".into());
    with_pane(|pane| {
        if let Some(review) = &mut pane.pause_review
            && review.scope.current(studio)
            && let Phase::Choosing { cuts, selection } = &mut review.phase
        {
            result = validate_selection(cuts, ids).map(|selected| *selection = Some(selected));
        }
    });
    result
}

fn validate_selection(cuts: &[Cut], ids: &[u32]) -> Result<HashSet<u32>, String> {
    let selected: HashSet<_> = ids.iter().copied().collect();
    if selected.len() != ids.len()
        || selected.len() > cuts.len()
        || !selected
            .iter()
            .all(|id| cuts.iter().any(|cut| cut.id == *id))
    {
        return Err("Use each supplied pause candidate ID at most once.".into());
    }
    Ok(selected)
}

struct Target {
    clips: Vec<String>,
    start: f64,
    end: f64,
}

struct Scope {
    epoch: u64,
    revision: u64,
    timeline: String,
    rate: f64,
    end: Option<f64>,
    targets: Vec<Target>,
}

impl Scope {
    fn capture(studio: &Studio, range: Option<[f64; 2]>) -> Result<Self, String> {
        if !matches!(studio.gesture, Gesture::None) || studio.confirm.is_some() {
            return Err(t("chatUi.pauseFinishEdit"));
        }
        let session = studio
            .session
            .as_ref()
            .ok_or_else(|| t("chatUi.pauseProjectRequired"))?;
        let project = session.project();
        let timeline = project.active();
        let owner = |clip: &Clip| clip.detached_from.as_deref().unwrap_or(&clip.id).to_owned();
        let selected: HashSet<_> = studio
            .selection
            .iter()
            .filter_map(|id| timeline.clip(id))
            .map(owner)
            .collect();
        let may_hear = |clip: &Clip| {
            matches!(clip.kind, ClipKind::Audio | ClipKind::Video)
                && clip.muted != Some(true)
                && clip.volume > 0.0
                && timeline
                    .track(&clip.track_id)
                    .is_some_and(|track| !track.muted)
                && project
                    .media
                    .iter()
                    .any(|media| media.id == clip.media_id && media.has_audio && !media.placeholder)
        };
        let mut targets = Vec::new();
        for clip in &timeline.clips {
            if clip.detached_from.is_some() || (range.is_none() && !selected.contains(&clip.id)) {
                continue;
            }
            let mut linked = vec![clip.as_ref()];
            linked.extend(
                timeline
                    .clips
                    .iter()
                    .filter(|item| item.detached_from.as_deref() == Some(&clip.id))
                    .map(|item| item.as_ref()),
            );
            if linked.iter().any(|item| studio.locked(&item.track_id))
                || !linked.iter().any(|item| may_hear(item))
            {
                continue;
            }
            let [in_time, out_time] = range.unwrap_or([clip.start, clip.start + clip.duration]);
            let start = clip.start.max(in_time);
            let end = (clip.start + clip.duration).min(out_time);
            if end > start {
                targets.push(Target {
                    clips: linked.iter().map(|item| item.id.clone()).collect(),
                    start,
                    end,
                });
            }
        }
        if targets.is_empty() {
            return Err(t("chatUi.pauseSelectAudio"));
        }
        Ok(Self {
            epoch: host::project_epoch(),
            revision: studio.edit_revision(),
            timeline: timeline.id.clone(),
            rate: timeline.video.rate(),
            end: range.map(|bounds| bounds[1]),
            targets,
        })
    }

    fn current(&self, studio: &Studio) -> bool {
        self.epoch == host::project_epoch()
            && self.revision == studio.edit_revision()
            && studio
                .session
                .as_ref()
                .is_some_and(|session| session.project().active_timeline_id == self.timeline)
    }

    fn windows(&self) -> Vec<[f64; 2]> {
        let mut windows: Vec<_> = self.targets.iter().map(|t| [t.start, t.end]).collect();
        windows.sort_by(|a, b| a[0].total_cmp(&b[0]));
        let mut merged: Vec<[f64; 2]> = Vec::new();
        for window in windows {
            if let Some(last) = merged.last_mut()
                && window[0] <= last[1]
            {
                last[1] = last[1].max(window[1]);
            } else {
                merged.push(window);
            }
        }
        merged
    }

    fn candidates(&self, studio: &Studio, detected: &[[f64; 2]]) -> Vec<Cut> {
        let mut cuts = Vec::new();
        for &[from, to] in detected {
            let Some([start, end]) = frame_bounds(from, to, self.rate) else {
                continue;
            };
            let changes: Vec<_> = self
                .targets
                .iter()
                .filter_map(|target| {
                    if start <= target.start || end >= target.end {
                        return None;
                    }
                    let group: Vec<_> = target
                        .clips
                        .iter()
                        .map(|clip_id| ClipPause {
                            clip_id: clip_id.clone(),
                            start,
                            end,
                        })
                        .collect();
                    validate_clip_pauses(studio.timeline(), &group, self.end)
                        .is_ok()
                        .then_some(group)
                })
                .flatten()
                .collect();
            if !changes.is_empty() {
                cuts.push(Cut {
                    id: (cuts.len() + 1) as u32,
                    start,
                    end,
                    changes,
                    included: true,
                });
                if cuts.len() == MAX_PAUSE_CUTS {
                    break;
                }
            }
        }
        cuts
    }

    fn command(&self, cuts: impl Iterator<Item = ClipPause>) -> Command {
        Command::TrimClipPauses {
            timeline_id: self.timeline.clone(),
            scope_end: self.end,
            cuts: cuts.collect(),
        }
    }
}

fn frame_bounds(start: f64, end: f64, rate: f64) -> Option<[f64; 2]> {
    if !start.is_finite() || !end.is_finite() || !rate.is_finite() || rate <= 0.0 {
        return None;
    }
    let from = (start * rate - 1e-6).ceil();
    let to = (end * rate + 1e-6).floor();
    (to - from >= 2.0).then_some([from / rate, to / rate])
}

struct Cut {
    id: u32,
    start: f64,
    end: f64,
    changes: Vec<ClipPause>,
    included: bool,
}

enum Phase {
    Analyzing(Arc<AtomicBool>),
    Choosing {
        cuts: Vec<Cut>,
        selection: Option<HashSet<u32>>,
    },
    Review(Vec<Cut>),
}

pub(super) struct PauseReview {
    scope: Scope,
    phase: Phase,
}

pub(super) struct PauseAudition {
    id: u32,
    cancelled: Arc<AtomicBool>,
    playback: Arc<Playback>,
    rendering: bool,
}

impl AgentPane {
    pub(super) fn pause_analysis_running(&self) -> bool {
        self.pause_review
            .as_ref()
            .is_some_and(|review| matches!(review.phase, Phase::Analyzing(_)))
    }

    pub(super) fn pause_active(&self) -> bool {
        self.pause_review.is_some() || self.pause_audition.is_some()
    }

    pub(super) fn begin_pause_review(&mut self) {
        if !self.can_submit() {
            return;
        }
        let mut captured = Err(t("chatUi.pauseEditorBusy"));
        Shell::with(|shell, _| {
            if let Ok(mut studio) = shell.studio.try_borrow_mut() {
                if !matches!(studio.gesture, Gesture::None) || studio.confirm.is_some() {
                    return;
                }
                studio.flush_commit();
                let range = self.range.as_ref().and_then(|range| range.bounds(&studio));
                captured = Scope::capture(&studio, range).and_then(|scope| {
                    let session = studio
                        .session
                        .as_ref()
                        .ok_or_else(|| t("chatUi.pauseProjectRequired"))?;
                    Ok((scope, session.snapshot()))
                });
            }
        });
        let (scope, snapshot) = match captured {
            Ok(captured) => captured,
            Err(error) => {
                self.error = error;
                self.publish();
                return;
            }
        };
        self.clear_pause_review();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = cancelled.clone();
        let windows = scope.windows();
        self.pause_review = Some(PauseReview {
            scope,
            phase: Phase::Analyzing(cancelled.clone()),
        });
        self.error.clear();
        self.publish();
        host::spawn_long_in_project(
            move || pause_audio::detect_pauses(&snapshot, &windows, &worker),
            move |studio, _, _, result| {
                with_pane(|pane| pane.on_pauses_detected(studio, &cancelled, result));
            },
        );
    }

    fn on_pauses_detected(
        &mut self,
        studio: &Studio,
        cancelled: &Arc<AtomicBool>,
        result: Result<Vec<[f64; 2]>, String>,
    ) {
        let Some(review) = &mut self.pause_review else {
            return;
        };
        if !matches!(&review.phase, Phase::Analyzing(token) if Arc::ptr_eq(token, cancelled)) {
            return;
        }
        if !review.scope.current(studio) {
            self.invalidate_pause_review(studio);
            return;
        }
        let detected = match result {
            Ok(detected) => detected,
            Err(error) => {
                self.clear_pause_review();
                self.error = error;
                self.status = t("chatUi.ready");
                self.publish();
                return;
            }
        };
        let cuts = review.scope.candidates(studio, &detected);
        if cuts.is_empty() {
            self.clear_pause_review();
            self.status = t("chatUi.noPauses");
            self.publish();
            return;
        }
        let candidates: Vec<_> = cuts
            .iter()
            .map(|cut| json!({"id": cut.id, "startSeconds": cut.start, "endSeconds": cut.end}))
            .collect();
        let detail = format!(
            " This is a read-only review. Concat measured these quiet intervals in the audible timeline mix: {}. Use only these candidates. Call pause_review_submit with the visible project folder and the IDs you recommend cutting, or an empty list. Keep natural pacing. Show the result in the editor. Do not run commands or change the project.",
            json!(candidates),
        );
        review.phase = Phase::Choosing {
            cuts,
            selection: None,
        };
        self.pause_turn = true;
        *REVIEW_CLIENT
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(agent_client_id(self.generation));
        if !self.send_prompt(
            t("chatUi.reviewPauses"),
            None,
            Submission::QuickAction,
            detail,
        ) {
            self.end_pause_turn();
            self.clear_pause_review();
            self.publish();
        }
    }

    pub(super) fn end_pause_turn(&mut self) {
        if std::mem::take(&mut self.pause_turn) {
            *REVIEW_CLIENT
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
    }

    pub(super) fn finish_pause_turn(&mut self, failed: bool) {
        if !self.pause_turn {
            return;
        }
        self.end_pause_turn();
        let Some(mut review) = self.pause_review.take() else {
            return;
        };
        let Phase::Choosing {
            mut cuts,
            selection,
        } = review.phase
        else {
            return;
        };
        match (failed, selection) {
            (false, Some(selected)) if !selected.is_empty() => {
                cuts.retain(|cut| selected.contains(&cut.id));
                review.phase = Phase::Review(cuts);
                self.pause_review = Some(review);
            }
            (false, Some(_)) => {
                self.local_message("Context", t("chatUi.pauseNoSuggestions"));
            }
            (false, None) if self.error.is_empty() => {
                self.error = t("chatUi.noPauseProposals");
            }
            _ => {}
        }
        self.publish_pause_rows();
    }

    pub(super) fn invalidate_pause_review(&mut self, studio: &Studio) {
        if self
            .pause_review
            .as_ref()
            .is_some_and(|review| !review.scope.current(studio))
        {
            let was_analyzing = self.pause_analysis_running();
            self.clear_pause_review();
            if was_analyzing {
                self.status = t("chatUi.ready");
            }
            self.error = t("chatUi.timelineChanged");
            self.publish();
        }
    }

    pub(super) fn clear_pause_review(&mut self) {
        if let Some(review) = self.pause_review.take()
            && let Phase::Analyzing(cancelled) = review.phase
        {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.stop_pause_audition();
        self.publish_pause_rows();
    }

    pub(super) fn cancel_pause_analysis(&mut self) -> bool {
        if !self.pause_analysis_running() {
            return false;
        }
        self.clear_pause_review();
        self.status = t("chatUi.pauseReviewCancelled");
        self.publish();
        true
    }

    fn reviewed(&self) -> Option<(&Scope, &[Cut])> {
        let review = self.pause_review.as_ref()?;
        match &review.phase {
            Phase::Review(cuts) => Some((&review.scope, cuts)),
            _ => None,
        }
    }

    fn publish_pause_rows(&self) {
        let Some(app) = self.ui.upgrade() else { return };
        let rows = self.reviewed().map_or_else(Vec::new, |(scope, cuts)| {
            cuts.iter()
                .map(|cut| {
                    let start = frames_timecode(cut.start as f32, scope.rate as f32);
                    let end = frames_timecode(cut.end as f32, scope.rate as f32);
                    PauseCutData {
                        id: cut.id as i32,
                        start: cut.start as f32,
                        end: cut.end as f32,
                        label: format!("{start}–{end}").into(),
                        included: cut.included,
                    }
                })
                .collect()
        });
        let global = app.global::<Agent>();
        global.set_pause_selected_count(rows.iter().filter(|row| row.included).count() as i32);
        global.set_pause_cuts(std::rc::Rc::new(VecModel::from(rows)).into());
    }

    pub(super) fn publish_pause_controls(&self, global: &Agent) {
        global.set_pause_audition(
            self.pause_audition
                .as_ref()
                .map_or(-1, |item| item.id as i32),
        );
        global.set_pause_can_apply(self.pause_audition.is_none() && self.can_submit());
    }

    pub(super) fn set_pause_included(&mut self, id: i32, included: bool) {
        if self.pause_audition.is_some() || !self.can_submit() {
            return;
        }
        if let Some(PauseReview {
            phase: Phase::Review(cuts),
            ..
        }) = &mut self.pause_review
            && let Some(cut) = cuts.iter_mut().find(|cut| cut.id == id as u32)
        {
            cut.included = included;
            self.publish_pause_rows();
        }
    }

    pub(super) fn jump_to_pause(&self, id: i32) {
        host::on_ui(move |studio, _, _| {
            let mut start = None;
            with_pane(|pane| {
                if let Some((scope, rows)) = pane.reviewed()
                    && scope.current(studio)
                    && let Some(cut) = rows.iter().find(|cut| cut.id == id as u32)
                {
                    start = Some(cut.start);
                }
            });
            if let Some(start) = start {
                studio.seek(start as f32);
            }
        });
    }

    pub(super) fn audition_pause(&mut self, id: i32) {
        if !self.can_submit() {
            return;
        }
        let id = id as u32;
        if self
            .pause_audition
            .as_ref()
            .is_some_and(|item| item.id == id)
        {
            self.stop_pause_audition();
            self.publish();
            return;
        }
        let Some((scope, rows)) = self.reviewed() else {
            return;
        };
        let Some(cut) = rows.iter().find(|cut| cut.id == id) else {
            return;
        };
        let mut captured = None;
        Shell::with(|shell, _| {
            if let Ok(mut studio) = shell.studio.try_borrow_mut()
                && scope.current(&studio)
                && matches!(studio.gesture, Gesture::None)
                && studio.confirm.is_none()
            {
                studio.close_source();
                studio.pause();
                if let Some(session) = &studio.session {
                    captured = Some((session.snapshot(), studio.host.playback.clone()));
                }
            }
        });
        let Some((mut snapshot, playback)) = captured else {
            return;
        };
        let chosen: Vec<_> = rows
            .iter()
            .filter(|row| row.included || row.id == id)
            .collect();
        let first_clip = &cut.changes[0].clip_id;
        let removed: f64 = chosen
            .iter()
            .filter(|row| {
                row.start < cut.start
                    && row
                        .changes
                        .iter()
                        .any(|change| &change.clip_id == first_clip)
            })
            .map(|row| row.end - row.start)
            .sum();
        let at = cut.start - removed;
        let command = scope.command(
            chosen
                .into_iter()
                .flat_map(|row| row.changes.iter().cloned()),
        );
        self.stop_pause_audition();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = cancelled.clone();
        self.pause_audition = Some(PauseAudition {
            id,
            cancelled: cancelled.clone(),
            playback,
            rendering: true,
        });
        self.error.clear();
        self.publish();
        host::spawn_long_in_project(
            move || {
                snapshot.apply(command).map_err(|error| {
                    log::warn!("Preparing the audio for a pause-cut preview failed: {error}");
                    t("chatUi.pausePreviewPrepareFailed")
                })?;
                pause_audio::render_join(&snapshot, at, &worker)
            },
            move |studio, _, _, result| {
                with_pane(|pane| pane.play_pause_audition(studio, &cancelled, result));
            },
        );
    }

    fn play_pause_audition(
        &mut self,
        studio: &Studio,
        cancelled: &Arc<AtomicBool>,
        result: Result<Arc<[[i16; 2]]>, String>,
    ) {
        if !self
            .reviewed()
            .is_some_and(|(scope, _)| scope.current(studio))
        {
            return;
        }
        let Some(item) = &mut self.pause_audition else {
            return;
        };
        if !Arc::ptr_eq(&item.cancelled, cancelled) {
            return;
        }
        match result {
            Ok(samples) if !studio.playing => {
                item.playback.play_audition(samples);
                item.rendering = false;
            }
            Ok(_) => self.stop_pause_audition(),
            Err(error) => {
                self.stop_pause_audition();
                self.error = error;
            }
        }
        self.publish();
    }

    fn stop_pause_audition(&mut self) {
        if let Some(item) = self.pause_audition.take() {
            item.cancelled.store(true, Ordering::Relaxed);
            item.playback.stop_audition();
        }
    }

    pub(super) fn poll_pause_audition(&mut self) {
        if self
            .pause_audition
            .as_ref()
            .is_some_and(|item| !item.rendering && !item.playback.is_auditioning())
        {
            self.stop_pause_audition();
        }
    }

    pub(super) fn apply_pause_cuts(&self) {
        if !self.can_submit() || self.pause_audition.is_some() {
            return;
        }
        host::on_ui(|studio, _, _| {
            if !matches!(studio.gesture, Gesture::None) || studio.confirm.is_some() {
                return;
            }
            studio.flush_commit();
            with_pane(|pane| {
                pane.invalidate_pause_review(studio);
                if !pane.can_submit() || pane.pause_audition.is_some() {
                    return;
                }
                let Some((scope, rows)) = pane.reviewed() else {
                    return;
                };
                let selected: Vec<_> = rows.iter().filter(|cut| cut.included).collect();
                if selected.is_empty() {
                    return;
                }
                let changes = selected.iter().flat_map(|cut| cut.changes.iter());
                if changes.clone().any(|change| {
                    studio
                        .clip(&change.clip_id)
                        .is_some_and(|clip| studio.locked(&clip.track_id))
                }) {
                    pane.error = t("chatUi.pauseTracksLocked");
                } else {
                    let command = scope.command(changes.cloned());
                    match studio.try_apply(command) {
                        Ok(_) => {
                            let count = selected.len();
                            pane.clear_pause_review();
                            pane.error.clear();
                            studio.notify(&tf("chatUi.pauseCutsApplied", &[&count]), false);
                        }
                        Err(_) => pane.error = t("chatUi.pauseCutsFailed"),
                    }
                }
                pane.publish();
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_round_inward_to_the_frame_grid() {
        assert_eq!(
            frame_bounds(0.501, 1.099, 24.0),
            Some([13.0 / 24.0, 26.0 / 24.0])
        );
        assert_eq!(frame_bounds(1.0, 1.05, 24.0), None);
        assert_eq!(frame_bounds(f64::NAN, 2.0, 24.0), None);
    }

    #[test]
    fn agent_can_only_pick_unique_ids_that_the_editor_supplied() {
        let candidate = Cut {
            id: 2,
            start: 1.0,
            end: 1.5,
            changes: Vec::new(),
            included: true,
        };
        let cuts = [candidate];
        assert_eq!(validate_selection(&cuts, &[2]).unwrap(), HashSet::from([2]));
        assert!(validate_selection(&cuts, &[2, 2]).is_err());
        assert!(validate_selection(&cuts, &[1]).is_err());
    }
}
