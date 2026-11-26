// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Checks the effects of an agent edit against its selected tracks and time range.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use concat_project::Project;
use concat_project::model::{Background, Clip, Timeline};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct EditBounds {
    pub(super) timeline: String,
    pub(super) tracks: BTreeSet<String>,
    pub(super) range: Option<[f64; 2]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ScopeViolation {
    Timeline,
    SharedAssets,
    Track(String),
    Range(String),
}

struct LocatedClip<'a> {
    clip: &'a Arc<Clip>,
    position: usize,
}

type ClipIndex<'a> = HashMap<&'a str, LocatedClip<'a>>;

impl EditBounds {
    pub(super) fn check(&self, before: &Project, after: &Project) -> Result<(), ScopeViolation> {
        let Project {
            media,
            fonts,
            timelines,
            active_timeline_id,
            extra,
            unread,
        } = before;
        if active_timeline_id != &self.timeline
            || after.active_timeline_id != self.timeline
            || timelines.len() != after.timelines.len()
        {
            return Err(ScopeViolation::Timeline);
        }
        if fonts != &after.fonts
            || extra != &after.extra
            || unread != &after.unread
            || !after.media.starts_with(media)
        {
            return Err(ScopeViolation::SharedAssets);
        }

        let imported: HashSet<_> = after.media[media.len()..]
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        if !imported.is_empty()
            && (media.iter().any(|item| imported.contains(item.id.as_str()))
                || timelines
                    .iter()
                    .any(|timeline| uses_media(timeline, &imported)))
        {
            // A newly imported id must not turn on a background or a clip
            // that used to reference a missing source.
            return Err(ScopeViolation::SharedAssets);
        }

        let mut found = false;
        for (old, new) in timelines.iter().zip(&after.timelines) {
            if old.id != new.id {
                return Err(ScopeViolation::Timeline);
            }
            if old.id == self.timeline {
                found = true;
                if !Arc::ptr_eq(old, new) {
                    self.check_timeline(old, new)?;
                }
            } else if !Arc::ptr_eq(old, new) && old != new {
                return Err(ScopeViolation::Timeline);
            }
        }
        if found {
            Ok(())
        } else {
            Err(ScopeViolation::Timeline)
        }
    }

    fn check_timeline(&self, old: &Timeline, new: &Timeline) -> Result<(), ScopeViolation> {
        let Timeline {
            id: _,
            name,
            video,
            background,
            tracks,
            clips,
            linkage,
            sections,
            caption_translations,
            extra,
        } = old;
        if name != &new.name
            || video != &new.video
            || background != &new.background
            || tracks != &new.tracks
            || linkage != &new.linkage
            || sections != &new.sections
            || caption_translations != &new.caption_translations
            || extra != &new.extra
        {
            return Err(ScopeViolation::Timeline);
        }

        let previous = index_clips(clips)?;
        let next = index_clips(&new.clips)?;
        let mut unchanged = Vec::new();
        for old_clip in clips {
            let new_clip = next.get(old_clip.id.as_str());
            if let Some(entry) = new_clip
                && (Arc::ptr_eq(old_clip, entry.clip) || old_clip == entry.clip)
            {
                unchanged.push((entry.position, old_clip.as_ref()));
            } else {
                self.check_clip(old_clip)?;
                if let Some(entry) = new_clip {
                    self.check_clip(entry.clip)?;
                }
            }

            let old_parent = parent_of(old_clip, &previous);
            let new_parent = new_clip.and_then(|entry| parent_of(entry.clip, &next));
            if old_parent.map(|clip| &clip.id) != new_parent.map(|clip| &clip.id) {
                self.check_clip(old_clip)?;
                for parent in old_parent.into_iter().chain(new_parent) {
                    self.check_clip(parent)?;
                }
            }
        }
        for clip in &new.clips {
            if !previous.contains_key(clip.id.as_str()) {
                self.check_clip(clip)?;
                if let Some(parent) = parent_of(clip, &next) {
                    self.check_clip(parent)?;
                }
            }
        }
        self.check_order(&unchanged)
    }

    fn check_order(&self, unchanged: &[(usize, &Clip)]) -> Result<(), ScopeViolation> {
        if unchanged.is_sorted_by_key(|(index, _)| *index) {
            return Ok(());
        }
        // The exporter uses clip order to resolve ties. An in-scope edit
        // may insert or delete clips, so compare only surviving, equal clips.
        let mut furthest = 0;
        let mut protected = None;
        for &(index, clip) in unchanged {
            if let Some((previous, violation)) = &protected
                && index < *previous
            {
                return Err(ScopeViolation::clone(violation));
            }
            if let Err(violation) = self.check_clip(clip) {
                if index < furthest {
                    return Err(violation);
                }
                protected = Some((index, violation));
            }
            furthest = furthest.max(index);
        }
        Ok(())
    }

    fn check_clip(&self, clip: &Clip) -> Result<(), ScopeViolation> {
        if !self.tracks.contains(&clip.track_id) {
            return Err(ScopeViolation::Track(clip.track_id.clone()));
        }
        if let Some([start, end]) = self.range {
            // A legacy transition may start before the clip it belongs to.
            let lead = clip.transition_in.as_ref().map_or(0.0, |transition| {
                transition.duration.min(clip.duration) / 2.0
            });
            let first = (clip.start - lead).max(0.0);
            let last = clip.start + clip.duration;
            // The playhead stores In/Out as f32. Compare at that precision so
            // a clip on the marked edge is not refused by a rounding error.
            if !start.is_finite()
                || !end.is_finite()
                || start >= end
                || !clip.start.is_finite()
                || !last.is_finite()
                || (first as f32) < start as f32
                || (last as f32) > end as f32
            {
                let name = if clip.name.is_empty() {
                    &clip.id
                } else {
                    &clip.name
                };
                return Err(ScopeViolation::Range(name.clone()));
            }
        }
        Ok(())
    }
}

fn index_clips(clips: &[Arc<Clip>]) -> Result<ClipIndex<'_>, ScopeViolation> {
    let items: ClipIndex<'_> = clips
        .iter()
        .enumerate()
        .map(|(position, clip)| (clip.id.as_str(), LocatedClip { clip, position }))
        .collect();
    if items.len() == clips.len() {
        Ok(items)
    } else {
        Err(ScopeViolation::Timeline)
    }
}

fn parent_of<'a>(clip: &Clip, clips: &ClipIndex<'a>) -> Option<&'a Clip> {
    let id = clip.detached_from.as_ref()?;
    clips.get(id.as_str()).map(|entry| entry.clip.as_ref())
}

fn uses_media(timeline: &Timeline, imports: &HashSet<&str>) -> bool {
    let backdrop = |background: &Background| matches!(background, Background::Image { media_id } if imports.contains(media_id.as_str()));
    backdrop(&timeline.background)
        || timeline.clips.iter().any(|clip| {
            imports.contains(clip.media_id.as_str())
                || clip.background.as_ref().is_some_and(backdrop)
                || clip.enhanced.as_ref().is_some_and(|copy| {
                    imports.contains(copy.original.as_str()) || imports.contains(copy.copy.as_str())
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_project::commands::NewMedia;
    use concat_project::model::{ClipKind, MediaItem, MediaKind};
    use concat_project::{Command, Editor};

    fn scope(range: Option<[f64; 2]>) -> EditBounds {
        EditBounds {
            timeline: "TL1".into(),
            tracks: BTreeSet::from(["T1".into()]),
            range,
        }
    }

    fn add_clip(project: &mut Project, id: &str, track: &str, start: f64, duration: f64) {
        project.active_mut().clips.push(Arc::new(Clip::blank(
            id,
            track,
            ClipKind::Video,
            id,
            start,
            duration,
        )));
    }

    #[test]
    fn a_move_checks_both_tracks_and_the_full_span_on_both_sides() {
        let mut before = Project::new();
        add_clip(&mut before, "clip", "T1", 2.0, 2.0);
        let bounds = scope(Some([1.0, 5.0]));
        let mut after = before.clone();
        after.active_mut().clip_mut("clip").unwrap().start = 3.0;
        assert_eq!(bounds.check(&before, &after), Ok(()));

        after.active_mut().clip_mut("clip").unwrap().start = 3.001;
        let outside = Err(ScopeViolation::Range("clip".into()));
        assert_eq!(bounds.check(&before, &after), outside);
        assert_eq!(bounds.check(&after, &before), outside);

        let changed = after.active_mut().clip_mut("clip").unwrap();
        changed.start = 2.0;
        changed.track_id = "T2".into();
        let protected = Err(ScopeViolation::Track("T2".into()));
        assert_eq!(bounds.check(&before, &after), protected);
        assert_eq!(bounds.check(&after, &before), protected);
    }

    #[test]
    fn a_clip_at_the_marked_edge_uses_the_playheads_precision() {
        let mut before = Project::new();
        add_clip(&mut before, "clip", "T1", 4.1, 0.1);
        let mut after = before.clone();
        after.active_mut().clip_mut("clip").unwrap().volume = 0.5;
        let bounds = scope(Some([f64::from(4.1_f32), f64::from(4.2_f32)]));
        assert_eq!(bounds.check(&before, &after), Ok(()));
    }

    #[test]
    fn a_batch_with_a_ripple_delete_cannot_move_a_clip_on_a_protected_track() {
        let mut editor = Editor::new();
        let item = NewMedia {
            path: "/footage.mov".into(),
            name: "Footage".into(),
            duration: Some(2.0),
            kind: MediaKind::Video,
            width: None,
            height: None,
            frame_rate: None,
            frame_rate_fraction: None,
            video_codec: None,
            audio_codec: None,
            has_audio: false,
            audio_tracks: Vec::new(),
            origin: None,
            color_space: Default::default(),
        };
        let media_id = editor
            .apply(Command::AddMedia { item })
            .unwrap()
            .created_id
            .unwrap();
        let place = |editor: &mut Editor, lane: &str, start| {
            editor
                .apply(Command::AddClip {
                    media_id: media_id.clone(),
                    track_id: lane.into(),
                    start,
                    ripple: false,
                })
                .unwrap()
                .created_id
                .unwrap()
        };
        let selected = place(&mut editor, "T1", 0.0);
        let protected = place(&mut editor, "T2", 3.0);
        let before = editor.project().clone();
        editor
            .apply(Command::Batch {
                commands: vec![Command::RemoveClips {
                    clip_ids: vec![selected],
                    ripple: true,
                }],
            })
            .unwrap();
        assert_eq!(
            editor.project().active().clip(&protected).unwrap().start,
            1.0
        );
        assert_eq!(
            scope(None).check(&before, editor.project()),
            Err(ScopeViolation::Track("T2".into()))
        );
    }

    #[test]
    fn a_protected_clip_cannot_be_reordered_among_surviving_clips() {
        let mut before = Project::new();
        add_clip(&mut before, "first", "T2", 0.0, 2.0);
        add_clip(&mut before, "next", "T2", 0.0, 2.0);
        let mut after = before.clone();
        after.active_mut().clips.swap(0, 1);
        assert_eq!(
            scope(None).check(&before, &after),
            Err(ScopeViolation::Track("T2".into()))
        );
    }

    #[test]
    fn removing_a_parent_cannot_change_a_link_from_a_protected_clip() {
        let mut before = Project::new();
        add_clip(&mut before, "picture", "T1", 0.0, 2.0);
        add_clip(&mut before, "sound", "T2", 0.0, 2.0);
        let sound = before.active_mut().clip_mut("sound").unwrap();
        sound.kind = ClipKind::Audio;
        sound.detached_from = Some("picture".into());

        let mut after = before.clone();
        after.active_mut().clips.retain(|clip| clip.id != "picture");
        assert_eq!(
            scope(None).check(&before, &after),
            Err(ScopeViolation::Track("T2".into()))
        );
    }

    #[test]
    fn a_caption_version_is_protected_timeline_metadata() {
        let mut before = Project::new();
        add_clip(&mut before, "caption", "T1", 1.0, 1.0);
        let caption = before.active_mut().clip_mut("caption").unwrap();
        caption.kind = ClipKind::Text;
        caption.text = Some(concat_project::model::TextStyle {
            content: "Original".into(),
            ..Default::default()
        });
        let mut translation = before.active().caption_translations.clone();
        translation
            .set_texts(
                before.active(),
                "T1",
                "fr",
                vec![("caption".into(), "Version française".into())],
            )
            .unwrap();
        let mut after = before.clone();
        after.active_mut().caption_translations = translation;
        assert_eq!(
            scope(None).check(&before, &after),
            Err(ScopeViolation::Timeline)
        );
    }

    #[test]
    fn imports_append_without_modifying_existing_media_or_project_settings() {
        let mut before = Project::new();
        before.media.push(MediaItem {
            id: "original".into(),
            ..Default::default()
        });
        let mut after = before.clone();
        after.media.push(MediaItem {
            id: "imported".into(),
            ..Default::default()
        });
        let bounds = scope(None);
        assert_eq!(bounds.check(&before, &after), Ok(()));

        after.media[0].path = "/different.mov".into();
        assert_eq!(
            bounds.check(&before, &after),
            Err(ScopeViolation::SharedAssets)
        );

        after.media[0] = before.media[0].clone();
        after.active_mut().linkage = false;
        assert_eq!(bounds.check(&before, &after), Err(ScopeViolation::Timeline));

        before.active_mut().background = Background::Image {
            media_id: "imported".into(),
        };
        after.active_mut().background = before.active().background.clone();
        after.active_mut().linkage = before.active().linkage;
        assert_eq!(
            bounds.check(&before, &after),
            Err(ScopeViolation::SharedAssets)
        );
    }
}
