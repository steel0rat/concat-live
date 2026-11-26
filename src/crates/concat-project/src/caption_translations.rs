// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Caption translations retain the clip identity and the original words
//! the translator worked from. Times always come from the live timeline.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::{
    model::{Clip, ClipKind, Timeline, ranges::MAX_TIME},
    srt::{self, Cue},
};
use serde::{Deserialize, Serialize};

/// Text versions for this timeline keyed by language tag and stable clip ID.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CaptionTranslations(BTreeMap<String, BTreeMap<String, Translation>>);

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct Translation {
    source: String,
    text: String,
}

/// The translation status for the active caption lane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// The cue has a translation of its current original.
    pub ready: usize,
    /// The clip has never had a version in this language.
    pub missing: usize,
    /// The original has changed since the translation was entered.
    pub stale: usize,
}

/// The two times that identify a cue in a SubRip file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    /// First millisecond on the source timeline.
    pub start_ms: u64,
    /// End time from the beginning of the source timeline.
    pub end_ms: u64,
}

/// A subtitle version could not be imported or exported yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A language tag does not match the format used for caption versions.
    InvalidLanguage,
    /// The requested language has no saved version on this timeline.
    MissingLanguage(String),
    /// There are no text clips on the chosen lane.
    EmptyTrack,
    /// The file or the edit contains no translated cues.
    EmptyImport,
    /// The file does not follow SubRip syntax.
    InvalidSrt {
        /// One-based line number.
        line: usize,
        /// Parser diagnostic for the log.
        reason: String,
    },
    /// A cue is outside the timeline's valid time range.
    InvalidTimes,
    /// More than one original cue uses these two times.
    AmbiguousSource(Span),
    /// The translated file supplies more than one cue for these times.
    RepeatedCue(Span),
    /// None of the original cues on the chosen track uses these times.
    UnmatchedCue(Span),
    /// A caption no longer exists on the chosen track.
    UnknownClip(String),
    /// The named clip has no translated words in the submission.
    EmptyTranslation(String),
    /// The submission names the same clip more than once.
    RepeatedClip(String),
    /// The translation has cues that still need an update.
    Incomplete(Coverage),
}

impl CaptionTranslations {
    /// Whether the timeline has no recorded translations.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Keeps only captions included in a captured montage fragment.
    pub(crate) fn retain_clips(&mut self, ids: &BTreeSet<&str>) {
        self.0.retain(|_, entries| {
            entries.retain(|id, _| ids.contains(id.as_str()));
            !entries.is_empty()
        });
    }

    /// Carries the fragment’s translated captions to their new clip IDs.
    pub(crate) fn apply_to_clips(&mut self, source: &Self, ids: &HashMap<&str, String>) {
        for (language, entries) in &source.0 {
            for (id, text) in entries {
                if let Some(new_id) = ids.get(id.as_str()) {
                    self.0
                        .entry(language.clone())
                        .or_default()
                        .insert(new_id.clone(), text.clone());
                }
            }
        }
    }

    /// Applies only the named IDs. Each update records the current
    /// original so later source edits can be reviewed before export.
    pub fn set_texts(
        &mut self,
        timeline: &Timeline,
        track_id: &str,
        language: &str,
        texts: Vec<(String, String)>,
    ) -> Result<(), Error> {
        let language = language_code(language)?;
        if texts.is_empty() {
            return Err(Error::EmptyImport);
        }
        let clips: BTreeMap<_, _> = cue_clips(timeline, track_id)
            .map(|(clip, source)| (clip.id.as_str(), source))
            .collect();
        if clips.is_empty() {
            return Err(Error::EmptyTrack);
        }
        let mut staged = BTreeMap::new();
        for (id, text) in texts {
            let source = clips
                .get(id.as_str())
                .ok_or_else(|| Error::UnknownClip(id.clone()))?;
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let text = text.trim();
            if text.is_empty() {
                return Err(Error::EmptyTranslation(id));
            }
            let entry = Translation {
                source: (*source).to_owned(),
                text: text.to_owned(),
            };
            if staged.insert(id.clone(), entry).is_some() {
                return Err(Error::RepeatedClip(id));
            }
        }
        self.0.entry(language).or_default().extend(staged);
        Ok(())
    }

    /// Associates translated cues with the original lane by exact
    /// millisecond boundaries. This is the same timing precision as the
    /// existing original SRT export, including sub-frame clip edges.
    pub fn import_srt(
        &mut self,
        timeline: &Timeline,
        track_id: &str,
        language: &str,
        text: &str,
    ) -> Result<(), Error> {
        let cues = srt::parse(text).map_err(|error| Error::InvalidSrt {
            line: error.line,
            reason: error.reason,
        })?;
        if cues.is_empty() {
            return Err(Error::EmptyImport);
        }
        let mut targets = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        for (clip, _) in cue_clips(timeline, track_id) {
            let key = span(clip.start, clip.start + clip.duration)?;
            if targets.insert(key, clip.id.as_str()).is_some() {
                ambiguous.insert(key);
            }
        }
        if targets.is_empty() {
            return Err(Error::EmptyTrack);
        }
        let mut seen = BTreeSet::new();
        let mut mapped = Vec::with_capacity(cues.len());
        for cue in cues {
            let key = span(cue.start, cue.end)?;
            if !seen.insert(key) {
                return Err(Error::RepeatedCue(key));
            }
            if ambiguous.contains(&key) {
                return Err(Error::AmbiguousSource(key));
            }
            let id = targets.get(&key).ok_or(Error::UnmatchedCue(key))?;
            mapped.push(((*id).to_owned(), cue.text));
        }
        self.set_texts(timeline, track_id, language, mapped)
    }

    /// Which versions address at least one current caption on this lane.
    pub fn languages(&self, timeline: &Timeline, track_id: &str) -> Vec<(&str, Coverage)> {
        let clips: Vec<_> = cue_clips(timeline, track_id).collect();
        self.0
            .iter()
            .filter_map(|(language, entries)| {
                let presence = coverage(clips.iter().copied(), entries);
                (presence.ready + presence.stale > 0).then_some((language.as_str(), presence))
            })
            .collect()
    }

    /// None uses the normal original-language SubRip export. A translated
    /// export is complete only when every original cue has current words.
    pub fn export(
        &self,
        timeline: &Timeline,
        track_id: &str,
        language: Option<&str>,
    ) -> Result<Vec<Cue>, Error> {
        let Some(language) = language else {
            let original = srt::title_cues(timeline, Some(track_id));
            return if original.is_empty() {
                Err(Error::EmptyTrack)
            } else {
                Ok(original)
            };
        };
        let language = language_code(language)?;
        let entries = self
            .0
            .get(&language)
            .ok_or_else(|| Error::MissingLanguage(language.clone()))?;
        let clips: Vec<_> = cue_clips(timeline, track_id).collect();
        if clips.is_empty() {
            return Err(Error::EmptyTrack);
        }
        let status = coverage(clips.iter().copied(), entries);
        if status.missing + status.stale > 0 {
            return Err(Error::Incomplete(status));
        }
        let mut cues = clips
            .into_iter()
            .map(|(clip, _)| Cue {
                start: clip.start,
                end: clip.start + clip.duration,
                text: entries[&clip.id].text.clone(),
            })
            .collect::<Vec<_>>();
        cues.sort_by(|a, b| a.start.total_cmp(&b.start));
        Ok(cues)
    }
}

fn cue_clips<'a>(
    timeline: &'a Timeline,
    track_id: &'a str,
) -> impl Iterator<Item = (&'a Clip, &'a str)> {
    timeline
        .clips
        .iter()
        .filter(move |clip| clip.kind == ClipKind::Text && clip.track_id == track_id)
        .filter_map(|clip| Some((clip.as_ref(), clip.text.as_ref()?.content.as_str())))
}

fn coverage<'a>(
    clips: impl Iterator<Item = (&'a Clip, &'a str)>,
    entries: &BTreeMap<String, Translation>,
) -> Coverage {
    let mut result = Coverage::default();
    for (clip, source) in clips {
        match entries.get(&clip.id) {
            Some(entry) if entry.source == source && !entry.text.trim().is_empty() => {
                result.ready += 1
            }
            Some(_) => result.stale += 1,
            None => result.missing += 1,
        }
    }
    result
}

fn span(start: f64, end: f64) -> Result<Span, Error> {
    if !(0.0..=MAX_TIME).contains(&start) || !(start..=MAX_TIME).contains(&end) {
        return Err(Error::InvalidTimes);
    }
    Ok(Span {
        start_ms: (start * 1_000.0).round() as u64,
        end_ms: (end * 1_000.0).round() as u64,
    })
}

/// Common language tags such as ru, en-GB, zh-Hant and es-419.
pub fn language_code(value: &str) -> Result<String, Error> {
    let mut parts = value.trim().split('-');
    let alpha = |value: &str| value.bytes().all(|byte| byte.is_ascii_alphabetic());
    let primary = parts.next().unwrap_or_default();
    if !(2..=3).contains(&primary.len()) || !alpha(primary) {
        return Err(Error::InvalidLanguage);
    }
    let mut normalized = primary.to_ascii_lowercase();
    let mut script = false;
    let mut region = false;
    let mut variants = BTreeSet::new();
    for part in parts {
        let fixed = if alpha(part) && part.len() == 4 && !script && !region && variants.is_empty() {
            script = true;
            let mut fixed = part.to_ascii_lowercase();
            fixed[0..1].make_ascii_uppercase();
            fixed
        } else if ((alpha(part) && part.len() == 2)
            || (part.len() == 3 && part.bytes().all(|byte| byte.is_ascii_digit())))
            && !region
            && variants.is_empty()
        {
            region = true;
            part.to_ascii_uppercase()
        } else if ((5..=8).contains(&part.len())
            || part.len() == 4 && part.as_bytes()[0].is_ascii_digit())
            && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            let fixed = part.to_ascii_lowercase();
            if !variants.insert(fixed.clone()) {
                return Err(Error::InvalidLanguage);
            }
            fixed
        } else {
            return Err(Error::InvalidLanguage);
        };
        normalized.push('-');
        normalized.push_str(&fixed);
    }
    Ok(normalized)
}

/// Reads known language entries without dropping an older timeline when
/// this field has an unsupported shape.
pub(crate) fn read<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<CaptionTranslations, D::Error> {
    let stored = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(stored).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use crate::{
        Command, DocumentSettings, Editor, Project,
        commands::blocks::Block,
        model::TextStyle,
        sections::{Section, SectionAnchor},
    };

    use super::*;

    fn add_title(editor: &mut Editor, at: f64, words: &str) -> String {
        editor
            .apply(Command::AddTextClip {
                above: false,
                track_id: Some("T2".into()),
                start: at,
                style: Some(TextStyle {
                    content: words.into(),
                    ..TextStyle::default()
                }),
                duration: Some(1.0),
                offset_y: None,
            })
            .unwrap()
            .created_id
            .unwrap()
    }

    fn add_version(editor: &mut Editor, language: &str, words: [&str; 2]) {
        let timeline = editor.project().active();
        let mut cues = srt::title_cues(timeline, Some("T2"));
        assert_eq!(cues.len(), words.len());
        for (cue, text) in cues.iter_mut().zip(words) {
            cue.text = text.into();
        }
        editor
            .apply(Command::ImportCaptionTranslation {
                timeline_id: timeline.id.clone(),
                track_id: "T2".into(),
                language: language.into(),
                srt: srt::write(&cues),
            })
            .unwrap();
    }

    #[test]
    fn versions_survive_native_save_and_are_undoable() {
        let mut editor = Editor::new();
        let first = add_title(&mut editor, 2.0, "Первый");
        add_title(&mut editor, 3.5, "Второй");
        let before = editor.project().clone();
        let video = before.active().video;
        let settings = DocumentSettings {
            name: "Caption translation fixture".into(),
            width: video.width,
            height: video.height,
            rate_num: video.rate_num,
            rate_den: video.rate_den,
            canvas: video.canvas,
        };
        assert!(
            editor.to_document(&settings)["timelines"][0]
                .get("captionTranslations")
                .is_none()
        );
        add_version(&mut editor, "en", ["First", "Second"]);
        add_version(&mut editor, "fr", ["Premier", "Deuxième"]);
        let timeline = editor.project().active();
        assert_eq!(
            timeline
                .clip(&first)
                .unwrap()
                .text
                .as_ref()
                .unwrap()
                .content,
            "Первый"
        );
        assert_eq!(
            timeline
                .caption_translations
                .languages(timeline, "T2")
                .len(),
            2
        );
        editor.undo();
        let timeline = editor.project().active();
        assert_eq!(
            timeline
                .caption_translations
                .languages(timeline, "T2")
                .len(),
            1
        );
        editor.undo();
        assert_eq!(*editor.project(), before);
        editor.redo();
        editor.redo();

        let document = editor.to_document(&settings);
        let restored = Editor::from_document(&document).unwrap();
        let timeline = restored.project().active();
        assert_eq!(
            timeline
                .caption_translations
                .export(timeline, "T2", Some("fr"))
                .unwrap()[1]
                .text,
            "Deuxième"
        );
        assert_eq!(
            timeline
                .caption_translations
                .export(timeline, "T2", None)
                .unwrap()[0]
                .text,
            "Первый"
        );
    }

    #[test]
    fn a_montage_block_restores_only_selected_caption_versions() {
        let mut source = Editor::new();
        let first = add_title(&mut source, 2.0, "Блок");
        let second = add_title(&mut source, 6.0, "Другая часть сцены");
        add_version(&mut source, "en", ["Reusable", "Elsewhere"]);
        let mut project = source.project().clone();
        let active = project.active_mut();
        active.sections.push(Section {
            id: 1,
            name: "Unrelated editorial section".into(),
            planned: Some(2.5),
            anchor: SectionAnchor::Time { at: 5.0 },
        });
        active.extra.insert("sourceOnly".into(), true.into());
        let block = Block::capture(&project, std::slice::from_ref(&first)).unwrap();
        let snapshot = block.project().active();
        assert_eq!(snapshot.clips.len(), 1);
        assert!(snapshot.sections.is_empty() && snapshot.extra.is_empty());
        let document = block.document("Reusable");
        let translations = &document["timelines"][0]["captionTranslations"]["en"];
        assert!(translations.get(&first).is_some() && translations.get(&second).is_none());

        let mut destination = Editor::new();
        let before = Project::new();
        let created = destination
            .apply(Command::InsertBlock {
                block: Box::new(block),
                start: 9.5,
            })
            .unwrap()
            .created_id
            .unwrap();
        let timeline = destination.project().active();
        let copied = timeline.clip(&created).unwrap();
        let cues = timeline
            .caption_translations
            .export(timeline, &copied.track_id, Some("en"))
            .unwrap();
        assert_eq!(
            cues,
            vec![Cue {
                start: 9.5,
                end: 10.5,
                text: "Reusable".into()
            }]
        );
        destination.undo();
        assert_eq!(*destination.project(), before);
    }
}
