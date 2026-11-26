// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Native clip snapshots and atomic placement on newly minted tracks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::{
    DocumentSettings, Project,
    commands::IdMint,
    model::{Background, Clip, ClipKind, Timeline, ranges::MAX_TIME},
};
use serde_json::Value;

/// Why a clip outside the selection must be included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// The sound track was detached from its associated picture.
    DetachedAudio,
    /// The two clips are required by the same cut or transition layer.
    Transition,
}

/// One additional clip displayed in the capture preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequiredClip {
    /// The stable ID to add once the user approves the preview.
    pub id: String,
    /// The missing clip’s display name.
    pub name: String,
    /// The display name of the clip that needs it.
    pub linked_to: String,
    /// The relationship that would be lost.
    pub link: Link,
}

/// Why the requested block could not be saved or placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A block requires at least one selected clip.
    EmptySelection,
    /// The clip ID no longer names a clip of the active timeline.
    MissingClip(String),
    /// A selected clip's track has gone.
    MissingTrack(String),
    /// A selected clip needs a bin item that is gone.
    MissingMedia(String),
    /// The named transition clip has lost one side of its cut.
    MissingTransitionSource(String),
    /// The selection needs more clips that the user can include.
    RequiredClips(Vec<RequiredClip>),
    /// A clip has an invalid span, or the block would exceed the timeline.
    InvalidTime,
    /// The saved document cannot be represented as a complete block.
    InvalidDocument,
    /// A distinct font already uses this family in the destination.
    FontConflict(String),
    /// A destination item uses the same path with different settings.
    MediaConflict(String),
}

/// The selected part of a project with its own editable native records.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    project: Project,
}

#[derive(Clone, Copy)]
struct Neighbor<'a> {
    clip: &'a Clip,
    link: Link,
}

impl Block {
    /// Retains native clip and track records in their original stacking order.
    pub fn capture(source: &Project, selected: &[String]) -> Result<Self, Error> {
        if selected.is_empty() {
            return Err(Error::EmptySelection);
        }
        let timeline = source.active();
        verify_links(timeline, selected)?;

        let ids: BTreeSet<&str> = selected.iter().map(String::as_str).collect();
        let mut snapshot = timeline.clone();
        snapshot.clips.retain(|clip| ids.contains(clip.id.as_str()));
        snapshot.sections.clear();
        snapshot.extra.clear();
        snapshot.caption_translations.retain_clips(&ids);
        if snapshot.clips.iter().any(|clip| !valid_span(clip)) {
            return Err(Error::InvalidTime);
        }
        let start = snapshot
            .clips
            .iter()
            .map(|clip| clip.start)
            .fold(f64::INFINITY, f64::min);
        let mut track_ids = BTreeSet::new();
        let mut media_ids = BTreeSet::new();
        let mut font_families = BTreeSet::new();
        let source_background_lane = timeline.background_lane();
        for clip in &mut snapshot.clips {
            let clip = Arc::make_mut(clip);
            if timeline.track(&clip.track_id).is_none() {
                return Err(Error::MissingTrack(clip.track_id.clone()));
            }
            if clip.background.is_none() {
                clip.background = timeline
                    .background_on(clip, source_background_lane)
                    .cloned();
            }
            clip.start -= start;
            track_ids.insert(clip.track_id.clone());
            if matches!(
                clip.kind,
                ClipKind::Video | ClipKind::Audio | ClipKind::Image
            ) || !clip.media_id.is_empty()
            {
                media_ids.insert(clip.media_id.clone());
            }
            if let Some(enhanced) = &clip.enhanced {
                media_ids.extend([enhanced.original.clone(), enhanced.copy.clone()]);
            }
            if let Some(Background::Image { media_id }) = &clip.background {
                media_ids.insert(media_id.clone());
            }
            if let Some(text) = &clip.text {
                font_families.insert(font_family(&text.font_family).to_owned());
            }
        }
        snapshot
            .tracks
            .retain(|track| track_ids.contains(&track.id));
        // Inherited backgrounds belong to the affected clips in the fragment.
        snapshot.background = Background::None;
        let media: Vec<_> = source
            .media
            .iter()
            .filter(|item| media_ids.remove(&item.id))
            .cloned()
            .collect();
        if let Some(id) = media_ids.into_iter().next() {
            return Err(Error::MissingMedia(id));
        }
        let fonts = source
            .fonts
            .iter()
            .filter(|font| font_families.contains(font_family(&font.family)))
            .cloned()
            .collect();
        Ok(Self {
            project: Project {
                media,
                fonts,
                active_timeline_id: snapshot.id.clone(),
                timelines: vec![Arc::new(snapshot)],
                extra: Default::default(),
                unread: Default::default(),
            },
        })
    }

    /// Reads a complete block in the same format as an ordinary project.
    pub fn from_document(document: &Value) -> Result<Self, Error> {
        let mut project = crate::from_document(document).ok_or(Error::InvalidDocument)?;
        if project.timelines.len() != 1 {
            return Err(Error::InvalidDocument);
        }
        let timeline = project.active_mut();
        timeline.sections.clear();
        timeline.extra.clear();
        let selected: Vec<_> = timeline.clips.iter().map(|clip| clip.id.clone()).collect();
        let block = Self::capture(&project, &selected)?;
        if block.project != project {
            return Err(Error::InvalidDocument);
        }
        Ok(block)
    }

    /// Writes the selected records as a self-contained native document.
    pub fn document(&self, name: &str) -> Value {
        let video = self.project.active().video;
        crate::to_document(
            &DocumentSettings {
                name: name.to_owned(),
                width: video.width,
                height: video.height,
                rate_num: video.rate_num,
                rate_den: video.rate_den,
                canvas: video.canvas,
            },
            &self.project,
        )
    }

    /// The single timeline and its owned media and fonts.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Rebase the references when packaging or materializing the block.
    pub fn files_mut(&mut self) -> impl Iterator<Item = &mut String> {
        self.project
            .media
            .iter_mut()
            .map(|item| &mut item.path)
            .chain(self.project.fonts.iter_mut().map(|font| &mut font.path))
    }

    /// Catalogue packages the block needs to render exactly as authored.
    pub fn effects(&self) -> BTreeSet<&str> {
        self.project
            .active()
            .clips
            .iter()
            .flat_map(|clip| {
                clip.video_effects
                    .iter()
                    .chain(&clip.filters)
                    .map(|link| link.id.as_str())
                    .chain(
                        clip.transition_in
                            .iter()
                            .map(|transition| transition.id.as_str()),
                    )
            })
            .collect()
    }

    /// A command can use this as its sole mutation. Failed placement leaves
    /// both the destination and its ID counter untouched.
    pub fn apply(
        &self,
        destination: &mut Project,
        mint: &mut IdMint,
        start: f64,
    ) -> Result<Vec<String>, Error> {
        if !start.is_finite() || start < 0.0 || start + self.project.active().duration() > MAX_TIME
        {
            return Err(Error::InvalidTime);
        }
        let mut next = destination.clone();
        let mut ids = mint.clone();
        ids.adopt_project(destination);

        for font in &self.project.fonts {
            let family = font_family(&font.family);
            let matching = destination
                .fonts
                .iter()
                .filter(|current| font_family(&current.family) == family);
            if matching
                .clone()
                .any(|current| !self.project.fonts.contains(current))
            {
                return Err(Error::FontConflict(font.family.clone()));
            }
            if !next.fonts.contains(font) {
                next.fonts.push(font.clone());
            }
        }

        let mut media_ids = HashMap::new();
        for item in &self.project.media {
            let mut imported = item.clone();
            if let Some(existing) = next.media.iter().find(|current| current.path == item.path) {
                imported.id = existing.id.clone();
                if imported != *existing {
                    return Err(Error::MediaConflict(item.path.clone()));
                }
            } else {
                imported.id = ids.next("m");
                next.media.push(imported.clone());
            }
            media_ids.insert(item.id.as_str(), imported.id);
        }

        let fragment = self.project.active();
        let track_ids: HashMap<_, _> = fragment
            .tracks
            .iter()
            .map(|track| (track.id.as_str(), ids.next("t")))
            .collect();
        let clip_ids: HashMap<_, _> = fragment
            .clips
            .iter()
            .map(|clip| (clip.id.as_str(), ids.next("c")))
            .collect();
        let timeline = next.active_mut();
        timeline
            .caption_translations
            .apply_to_clips(&fragment.caption_translations, &clip_ids);
        timeline
            .tracks
            .extend(fragment.tracks.iter().cloned().map(|mut track| {
                track.id.clone_from(&track_ids[track.id.as_str()]);
                track
            }));
        let added = fragment
            .clips
            .iter()
            .map(|original| {
                let mut clip = original.as_ref().clone();
                clip.id.clone_from(&clip_ids[original.id.as_str()]);
                clip.track_id
                    .clone_from(&track_ids[original.track_id.as_str()]);
                clip.start += start;
                if !clip.media_id.is_empty() {
                    clip.media_id
                        .clone_from(&media_ids[original.media_id.as_str()]);
                }
                if let Some(enhanced) = &mut clip.enhanced {
                    enhanced.original = media_ids[enhanced.original.as_str()].clone();
                    enhanced.copy = media_ids[enhanced.copy.as_str()].clone();
                }
                if let Some(Background::Image { media_id }) = &mut clip.background {
                    *media_id = media_ids[media_id.as_str()].clone();
                }
                if let Some(video) = &mut clip.detached_from {
                    *video = clip_ids[video.as_str()].clone();
                }
                Arc::new(clip)
            })
            .collect::<Vec<_>>();
        let placed = added.iter().map(|clip| clip.id.clone()).collect();
        timeline.clips.extend(added);
        *destination = next;
        *mint = ids;
        Ok(placed)
    }
}

fn font_family(family: &str) -> &str {
    family.trim().trim_matches('"').trim_matches('\'')
}

fn valid_span(clip: &Clip) -> bool {
    clip.start.is_finite()
        && clip.start >= 0.0
        && clip.duration > 0.0
        && clip.start + clip.duration <= MAX_TIME
}

fn verify_links(timeline: &Timeline, selected: &[String]) -> Result<(), Error> {
    let clips: HashMap<_, _> = timeline
        .clips
        .iter()
        .map(|clip| (clip.id.as_str(), clip.as_ref()))
        .collect();
    let lanes: HashMap<_, _> = timeline
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id.as_str(), (index, track.visible)))
        .collect();
    let pictures: Vec<_> = timeline
        .clips
        .iter()
        .filter_map(|clip| {
            let (row, visible) = *lanes.get(clip.track_id.as_str())?;
            (visible && clip.kind.is_visual()).then_some((row, clip.as_ref()))
        })
        .collect();
    let half_frame = timeline.video.rate_den as f64 / (2.0 * timeline.video.rate_num as f64);
    let has_placed_transition = timeline
        .clips
        .iter()
        .any(|clip| clip.kind == ClipKind::Transition);
    let previous: HashMap<_, _> = pictures
        .iter()
        .filter(|(_, clip)| has_placed_transition || clip.transition_in.is_some())
        .filter_map(|(row, incoming)| {
            let (_, outgoing) = pictures.iter().find(|(other_row, outgoing)| {
                row == other_row
                    && outgoing.id != incoming.id
                    && (outgoing.start + outgoing.duration - incoming.start).abs() < half_frame
            })?;
            Some((incoming.id.as_str(), *outgoing))
        })
        .collect();

    let mut neighbors: HashMap<&str, Vec<Neighbor>> = HashMap::new();
    let mut unresolved = HashMap::new();
    let mut connect = |one: &Clip, other: &Clip, link| {
        // The owned references live in the active timeline for the entire check.
        let one = clips[one.id.as_str()];
        let other = clips[other.id.as_str()];
        neighbors
            .entry(one.id.as_str())
            .or_default()
            .push(Neighbor { clip: other, link });
        neighbors
            .entry(other.id.as_str())
            .or_default()
            .push(Neighbor { clip: one, link });
    };
    for clip in timeline.clips.iter().map(Arc::as_ref) {
        if let Some(video) = &clip.detached_from {
            if let Some(source) = clips.get(video.as_str()) {
                connect(clip, source, Link::DetachedAudio);
            } else {
                unresolved.insert(clip.id.as_str(), Error::MissingClip(video.clone()));
            }
        }
        let (_, visible) = lanes
            .get(clip.track_id.as_str())
            .copied()
            .unwrap_or_default();
        if visible && clip.kind.is_visual() && clip.transition_in.is_some() {
            if let Some(outgoing) = previous.get(clip.id.as_str()) {
                connect(clip, outgoing, Link::Transition);
            } else {
                unresolved.insert(
                    clip.id.as_str(),
                    Error::MissingTransitionSource(clip.id.clone()),
                );
            }
        }
    }
    for clip in timeline
        .clips
        .iter()
        .filter(|clip| clip.kind == ClipKind::Transition)
    {
        let Some(&(row, true)) = lanes.get(clip.track_id.as_str()) else {
            continue;
        };
        let end = clip.start + clip.duration;
        let middle = (clip.start + end) / 2.0;
        let cut = pictures
            .iter()
            .filter(|(below, picture)| {
                *below < row
                    && picture.start >= clip.start - half_frame
                    && picture.start <= end + half_frame
                    && previous.contains_key(picture.id.as_str())
            })
            .min_by(|(row_a, a), (row_b, b)| {
                (a.start - middle)
                    .abs()
                    .total_cmp(&(b.start - middle).abs())
                    .then(row_b.cmp(row_a))
            });
        if let Some((_, incoming)) = cut {
            connect(clip, incoming, Link::Transition);
            connect(clip, previous[incoming.id.as_str()], Link::Transition);
        } else {
            for (_, source) in pictures.iter().filter(|(below, picture)| {
                *below < row && picture.start < end && picture.start + picture.duration > clip.start
            }) {
                connect(clip, source, Link::Transition);
            }
        }
    }

    let mut stack: Vec<_> = selected
        .iter()
        .map(|id| {
            clips
                .get(id.as_str())
                .copied()
                .ok_or_else(|| Error::MissingClip(id.clone()))
        })
        .collect::<Result<_, _>>()?;
    let mut known: BTreeSet<_> = stack.iter().map(|clip| clip.id.as_str()).collect();
    let mut missing = BTreeMap::new();
    while let Some(clip) = stack.pop() {
        if let Some(error) = unresolved.remove(clip.id.as_str()) {
            return Err(error);
        }
        for neighbor in neighbors.get(clip.id.as_str()).into_iter().flatten() {
            let required = neighbor.clip;
            if known.insert(required.id.as_str()) {
                missing.insert(
                    required.id.as_str(),
                    RequiredClip {
                        id: required.id.clone(),
                        name: required.name.clone(),
                        linked_to: clip.name.clone(),
                        link: neighbor.link,
                    },
                );
                stack.push(required);
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::RequiredClips(missing.into_values().collect()))
    }
}
