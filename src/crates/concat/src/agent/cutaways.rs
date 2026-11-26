// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

use std::collections::BTreeSet;
use std::path::Path;

use concat_project::Project;
use concat_project::model::{Clip, ClipKind, MediaKind};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CutawayError {
    SelectCaption,
    NoMedia,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CutawayBrief<'a> {
    timeline_id: &'a str,
    caption_id: &'a str,
    caption: String,
    caption_start: f64,
    caption_end: f64,
    candidates: Vec<Candidate<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Candidate<'a> {
    media_id: &'a str,
    name: &'a str,
    kind: MediaKind,
    source_duration: Option<f64>,
    source_in: f64,
    timeline_in: f64,
    duration: f64,
}

impl<'a> CutawayBrief<'a> {
    pub(super) fn capture(
        project: &'a Project,
        selection: &[String],
        folder: &Path,
    ) -> Result<Self, CutawayError> {
        let timeline = project.active();
        let (caption, caption_text) =
            selected_caption(project, selection).ok_or(CutawayError::SelectCaption)?;

        let start = caption.start;
        let end = start + caption.duration;
        let frame = 1.0 / timeline.video.rate();
        if !start.is_finite() || start < 0.0 || !end.is_finite() || end - start < frame {
            return Err(CutawayError::SelectCaption);
        }

        let words = keywords(caption_text);
        let on_screen: BTreeSet<&str> = timeline
            .clips
            .iter()
            .filter(|clip| matches!(clip.kind, ClipKind::Video | ClipKind::Image))
            .filter(|clip| clip.start < end && clip.start + clip.duration > start)
            .map(|clip| clip.media_id.as_str())
            .collect();
        let mut candidates = project
            .media
            .iter()
            .filter(|item| !item.placeholder && !item.path.is_empty())
            .filter(|item| !on_screen.contains(item.id.as_str()))
            .filter_map(|item| {
                let preferred = 3.0_f64.min(end - start);
                let (duration, available) = match item.kind {
                    MediaKind::Image => (preferred, None),
                    MediaKind::Video => {
                        let available = item.duration.filter(|d| d.is_finite() && *d >= frame)?;
                        (preferred.min(available), Some(available))
                    }
                    MediaKind::Audio => return None,
                };
                let filename = Path::new(&item.name);
                let stem = filename
                    .file_stem()
                    .and_then(|word| word.to_str())
                    .unwrap_or(&item.name);
                let score = words.intersection(&keywords(stem)).count();
                Some((
                    score,
                    Candidate {
                        media_id: &item.id,
                        name: &item.name,
                        kind: item.kind,
                        source_duration: available,
                        source_in: 0.0,
                        timeline_in: start + ((end - start) - duration) * 0.5,
                        duration,
                    },
                    item.path.as_str(),
                ))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|(left_score, left, _), (right_score, right, _)| {
            right_score
                .cmp(left_score)
                .then_with(|| right.duration.total_cmp(&left.duration))
                .then_with(|| left.name.cmp(right.name))
                .then_with(|| left.media_id.cmp(right.media_id))
        });
        let candidates = candidates
            .into_iter()
            .filter(|(_, _, path)| folder.join(path).is_file())
            .take(16)
            .map(|(_, candidate, _)| candidate)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(CutawayError::NoMedia);
        }

        Ok(Self {
            timeline_id: &timeline.id,
            caption_id: &caption.id,
            caption: caption_text.chars().take(2400).collect(),
            caption_start: start,
            caption_end: end,
            candidates,
        })
    }
}

pub(super) fn selected_caption<'a>(
    project: &'a Project,
    selection: &[String],
) -> Option<(&'a Clip, &'a str)> {
    let [selected] = selection else {
        return None;
    };
    let caption = project.active().clip(selected)?;
    let text = caption.text.as_ref()?.content.trim();
    (caption.kind == ClipKind::Text && !text.is_empty()).then_some((caption, text))
}

fn keywords(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.chars().count() >= 3)
        .map(|word| word.to_lowercase().replace('ё', "е"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use concat_project::model::{Clip, MediaItem, TextStyle};

    use super::*;

    fn fixture() -> Project {
        let mut project = concat_project::Editor::new().project().clone();
        let timeline = Arc::make_mut(&mut project.timelines[0]);
        timeline.clips.push(Arc::new(Clip {
            id: "caption".into(),
            kind: ClipKind::Text,
            start: 17.0,
            duration: 6.0,
            text: Some(TextStyle {
                content: "Утром мы увидели маяк.".into(),
                ..TextStyle::default()
            }),
            ..Clip::default()
        }));
        project
    }

    #[test]
    fn ranks_relevant_files_with_real_ranges_and_keeps_the_caption_intact() {
        let mut project = fixture();
        let path = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for (id, name, kind, duration) in [
            ("nearby", "Маяк_панорама.mp4", MediaKind::Video, Some(1.5)),
            ("image", "pier.jpg", MediaKind::Image, None),
            ("sound", "Маяк.wav", MediaKind::Audio, Some(50.0)),
            (
                "playing",
                "Маяк_на_экране.mp4",
                MediaKind::Video,
                Some(50.0),
            ),
            ("unknown", "without_duration.mp4", MediaKind::Video, None),
        ] {
            project.media.push(MediaItem {
                id: id.into(),
                name: name.into(),
                kind,
                duration,
                path: path.clone(),
                ..MediaItem::default()
            });
        }
        Arc::make_mut(&mut project.timelines[0])
            .clips
            .push(Arc::new(Clip {
                media_id: "playing".into(),
                kind: ClipKind::Video,
                start: 10.0,
                duration: 30.0,
                ..Clip::default()
            }));
        let selection = vec!["caption".into()];
        let brief = CutawayBrief::capture(&project, &selection, Path::new("/")).unwrap();
        assert_eq!(brief.caption, "Утром мы увидели маяк.");
        assert_eq!(brief.candidates.len(), 2);
        assert_eq!(brief.candidates[0].media_id, "nearby");
        assert_eq!(brief.candidates[0].duration, 1.5);
        assert_eq!(brief.candidates[0].timeline_in, 19.25);
        assert_eq!(brief.candidates[1].media_id, "image");
        assert_eq!(brief.candidates[1].duration, 3.0);
    }

    #[test]
    fn requires_one_nonempty_caption_and_an_existing_candidate() {
        let project = fixture();
        let folder = Path::new("/");
        assert_eq!(
            CutawayBrief::capture(&project, &[], folder).err(),
            Some(CutawayError::SelectCaption)
        );
        assert_eq!(
            CutawayBrief::capture(&project, &["caption".into()], folder).err(),
            Some(CutawayError::NoMedia)
        );
    }
}
