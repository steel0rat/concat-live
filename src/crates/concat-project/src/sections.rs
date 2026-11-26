// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Editorial sections that supply both timing comparisons and publishing chapters.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::{Timeline, ranges::MAX_TIME};

const MAX_SECTIONS: usize = 1024;
const MAX_NAME: usize = 120;

/// The position a section follows as a timeline is edited.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SectionAnchor {
    /// An absolute position in timeline seconds.
    Time {
        /// The chosen position.
        at: f64,
    },
    /// A position inside a clip, as far into the clip's current length as possible.
    Clip {
        /// The clip whose movement carries the section.
        clip_id: String,
        /// The distance from the front of the clip in timeline seconds.
        offset: f64,
        /// Where the section remains if that clip is removed.
        fallback: f64,
    },
}

impl SectionAnchor {
    fn is_valid(&self) -> bool {
        let valid_at = |at: f64| (0.0..=2.0 * MAX_TIME).contains(&at);
        match self {
            Self::Time { at } => valid_at(*at),
            Self::Clip {
                clip_id,
                offset,
                fallback,
            } => !clip_id.is_empty() && valid_at(*fallback) && (0.0..=MAX_TIME).contains(offset),
        }
    }

    fn resolve(&self, timeline: &Timeline) -> (f64, bool) {
        match self {
            Self::Time { at } => (*at, true),
            Self::Clip {
                clip_id,
                offset,
                fallback,
            } => match timeline.clip(clip_id) {
                Some(clip) => (clip.start + offset.min(clip.duration), true),
                None => (*fallback, false),
            },
        }
    }
}

/// A named edit section and its optional target duration.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    /// Stable within this timeline.
    pub id: u32,
    /// A single line for both the timing panel and publishing lists.
    pub name: String,
    /// Position of the first frame of the section.
    pub anchor: SectionAnchor,
    /// Expected duration in seconds.
    pub planned: Option<f64>,
}

impl Section {
    /// Trims names and validates a complete replacement before saving it.
    pub fn normalized(mut sections: Vec<Self>) -> Option<Vec<Self>> {
        if sections.len() > MAX_SECTIONS {
            return None;
        }
        let mut ids = BTreeSet::new();
        for section in &mut sections {
            section.name = section.name.trim().to_owned();
            if !(1..=(i32::MAX as u32)).contains(&section.id)
                || !ids.insert(section.id)
                || section.name.is_empty()
                || section.name.chars().count() > MAX_NAME
                || section.name.chars().any(char::is_control)
                || !section.anchor.is_valid()
                || section
                    .planned
                    .is_some_and(|time| !(0.0..=2.0 * MAX_TIME).contains(&time))
            {
                return None;
            }
        }
        Some(sections)
    }
}

/// Current interval of a named section after applying real clip positions.
pub struct SectionTiming<'a> {
    /// The saved title, plan and anchor.
    pub section: &'a Section,
    /// Current beginning in seconds.
    pub start: f64,
    /// Current end at the following section or at the end of the edit.
    pub end: f64,
    /// True if the original anchor is still present.
    pub attached: bool,
}

impl SectionTiming<'_> {
    /// Current duration on the edit.
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

impl<'a> SectionTiming<'a> {
    /// Resolves section anchors in timeline order on each update.
    pub fn on_timeline(sections: &'a [Section], timeline: &Timeline) -> Vec<Self> {
        let length = timeline.duration();
        let mut rows = sections
            .iter()
            .filter(|section| section.anchor.is_valid())
            .map(|section| {
                let (start, attached) = section.anchor.resolve(timeline);
                Self {
                    section,
                    start: start.clamp(0.0, length),
                    end: length,
                    attached,
                }
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            left.start
                .total_cmp(&right.start)
                .then_with(|| left.section.id.cmp(&right.section.id))
        });
        for index in 1..rows.len() {
            rows[index - 1].end = rows[index].start;
        }
        rows
    }
}

/// Why the current timeline cannot be represented as a publishing list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChapterError {
    /// A list cannot be made with no named sections.
    Empty,
    /// There is no chapter covering the beginning of the edit.
    MissingBeginning,
    /// A clip-linked marker lost its clip.
    MissingAnchor,
    /// Starts must identify different seconds and leave footage to play.
    AmbiguousTime,
}

/// Returns exactly the current chapter starts and titles with whole-second timestamps.
pub fn publication_chapters<'a>(
    rows: &[SectionTiming<'a>],
) -> Result<Vec<(u64, &'a str)>, ChapterError> {
    let first = rows.first().ok_or(ChapterError::Empty)?;
    if first.start.floor() != 0.0 {
        return Err(ChapterError::MissingBeginning);
    }
    let mut chapters = Vec::with_capacity(rows.len());
    for row in rows {
        if !row.attached {
            return Err(ChapterError::MissingAnchor);
        }
        let time = row.start.floor() as u64;
        if row.end <= row.start
            || chapters
                .last()
                .is_some_and(|(previous, _)| *previous >= time)
        {
            return Err(ChapterError::AmbiguousTime);
        }
        chapters.push((time, row.section.name.as_str()));
    }
    Ok(chapters)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::model::Clip;

    use super::*;

    #[test]
    fn moving_a_clip_changes_timing_and_publishing_together() {
        let mut timeline = Timeline::default();
        timeline.clips = vec![
            Arc::new(Clip {
                id: "first".into(),
                duration: 14.0,
                ..Clip::default()
            }),
            Arc::new(Clip {
                id: "second".into(),
                start: 14.0,
                duration: 14.0,
                ..Clip::default()
            }),
        ];
        let sections = Section::normalized(vec![
            Section {
                id: 2,
                name: "Разбор".into(),
                anchor: SectionAnchor::Clip {
                    clip_id: "second".into(),
                    offset: 0.0,
                    fallback: 14.0,
                },
                planned: Some(12.0),
            },
            Section {
                id: 1,
                name: " Вступление ".into(),
                anchor: SectionAnchor::Time { at: 0.0 },
                planned: Some(12.0),
            },
        ])
        .unwrap();

        let initial = SectionTiming::on_timeline(&sections, &timeline);
        assert_eq!(initial[0].section.name, "Вступление");
        assert_eq!(
            initial[0].duration() - initial[0].section.planned.unwrap(),
            2.0
        );
        assert_eq!(
            publication_chapters(&initial).unwrap(),
            [(0, "Вступление"), (14, "Разбор")]
        );

        Arc::make_mut(&mut timeline.clips[1]).start = 17.2;
        let edited = SectionTiming::on_timeline(&sections, &timeline);
        assert_eq!(edited[0].duration(), 17.2);
        assert_eq!(
            publication_chapters(&edited).unwrap(),
            [(0, "Вступление"), (17, "Разбор")]
        );

        timeline.clips.remove(1);
        let shortened = SectionTiming::on_timeline(&sections, &timeline);
        assert_eq!(shortened[1].duration(), 0.0);
        assert_eq!(
            publication_chapters(&shortened),
            Err(ChapterError::MissingAnchor)
        );
    }

    #[test]
    fn timeline_sections_round_trip_and_undo() {
        use crate::{Command, DocumentSettings, Editor, from_document, to_document};

        let mut editor = Editor::new();
        let previous = editor.project().clone();
        let sections = vec![Section {
            id: 7,
            name: "Первый раздел".into(),
            anchor: SectionAnchor::Time { at: 0.0 },
            planned: Some(12.345),
        }];
        editor
            .apply(Command::SetTimelineSections {
                timeline_id: previous.active_timeline_id.clone(),
                sections: sections.clone(),
            })
            .unwrap();

        let settings = DocumentSettings {
            name: "Сравнение хронометража".into(),
            width: 320,
            height: 180,
            rate_num: 25,
            rate_den: 1,
            canvas: [0, 0, 0, 255],
        };
        let document = to_document(&settings, editor.project());
        let restored = from_document(&document).unwrap();
        assert_eq!(restored.active().sections, sections);
        assert!(editor.undo());
        assert_eq!(*editor.project(), previous);
        assert!(editor.redo());
        assert_eq!(editor.project().active().sections, sections);
    }

    #[test]
    fn malformed_or_overlapping_chapters_are_rejected() {
        let valid = Section {
            id: 1,
            name: "Начало".into(),
            anchor: SectionAnchor::Time { at: 0.0 },
            planned: Some(30.0),
        };
        assert!(Section::normalized(vec![valid.clone(), valid.clone()]).is_none());
        assert!(
            Section::normalized(vec![Section {
                planned: Some(f64::NAN),
                ..valid.clone()
            }])
            .is_none()
        );
        let mut timeline = Timeline::default();
        timeline.clips.push(Arc::new(Clip {
            duration: 30.0,
            ..Clip::default()
        }));
        let sections = [
            valid,
            Section {
                id: 2,
                name: "Ещё начало".into(),
                anchor: SectionAnchor::Time { at: 0.5 },
                planned: None,
            },
        ];
        assert_eq!(
            publication_chapters(&SectionTiming::on_timeline(&sections, &timeline)),
            Err(ChapterError::AmbiguousTime)
        );
    }
}
