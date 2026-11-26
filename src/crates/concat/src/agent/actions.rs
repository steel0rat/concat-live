// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Timeline requests and the user's saved chat prompts.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::storage::{read_json, write_json};
use crate::ui::AgentQuickActionData;

const BUILT_INS: &[(&str, &str, &str)] = &[
    (
        "captions",
        "Captions",
        "Make subtitles for the spoken audio in the supplied timeline selection. Add editable, time-aligned captions in the language spoken in the video. Keep the original audio and clips in place, and keep caption timing inside the supplied scope.",
    ),
    (
        "trim-pauses",
        "Trim pauses",
        "Shorten the long pauses in speech within the supplied timeline selection. Preserve spoken words, transitions and a natural cadence. Limit cuts and timing adjustments to that scope, keeping media outside it in place.",
    ),
    (
        "fit-9-16",
        "Fit 9:16",
        "Set the active timeline's frame to a vertical 9:16 aspect ratio and reframe the clips within the supplied timeline selection for that canvas. Preserve the timeline frame rate and duration. Apply clip transform changes only within the supplied scope.",
    ),
];

#[derive(Default, Deserialize, Serialize)]
struct PinnedActions {
    last_id: u64,
    entries: Vec<PinnedAction>,
}

#[derive(Deserialize, Serialize)]
struct PinnedAction {
    id: u64,
    prompt: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    template: bool,
}

struct PromptTemplate<'a> {
    fields: Vec<&'a str>,
    substitutions: Vec<(&'a str, usize)>,
    tail: &'a str,
}

impl<'a> PromptTemplate<'a> {
    fn parse(prompt: &'a str) -> Result<Self, String> {
        let mut fields = Vec::new();
        let mut field_indices = HashMap::new();
        let mut substitutions = Vec::new();
        let mut tail = prompt;
        // Only an opening delimiter starts a field. Closing braces in
        // ordinary prompt text can belong to nested JSON.
        while let Some((literal, rest)) = tail.split_once("{{") {
            let (label, remaining) = rest
                .split_once("}}")
                .ok_or("Close each template parameter with }}.")?;
            let name = label.trim();
            if name.is_empty()
                || label.contains(['{', '}', '\u{2028}', '\u{2029}'])
                || label.chars().any(char::is_control)
            {
                return Err("Use a non-empty, single-line name inside each {{parameter}}.".into());
            }
            let index = *field_indices.entry(name).or_insert_with(|| {
                let index = fields.len();
                fields.push(name);
                index
            });
            substitutions.push((literal, index));
            tail = remaining;
        }
        if fields.is_empty() {
            return Err("Add at least one parameter, such as {{Name}}.".into());
        }
        Ok(Self {
            fields,
            substitutions,
            tail,
        })
    }

    fn render(&self, values: &[String]) -> Result<String, String> {
        if values.len() != self.fields.len() {
            return Err("Supply one value for each template parameter.".into());
        }
        if values.iter().any(|value| value.trim().is_empty()) {
            return Err("Fill in all template parameters.".into());
        }

        let mut prompt = String::new();
        for &(literal, index) in &self.substitutions {
            prompt.push_str(literal);
            prompt.push_str(&values[index]);
        }
        prompt.push_str(self.tail);
        Ok(prompt)
    }
}

pub(super) struct QuickActions {
    path: PathBuf,
    pins: PinnedActions,
}

impl QuickActions {
    pub(super) fn new(config: &Path) -> Self {
        let path = config.join("chat-quick-actions.json");
        let pins = Self::read_pins(&path).unwrap_or_else(|error| {
            log::warn!("{error}");
            PinnedActions::default()
        });
        Self { path, pins }
    }

    pub(super) fn prompt(&self, id: &str) -> Option<&str> {
        if let Some((_, _, prompt)) = BUILT_INS.iter().find(|action| action.0 == id) {
            return Some(prompt);
        }
        self.pinned_action(id)
            .filter(|action| !action.template)
            .map(|action| action.prompt.as_str())
    }

    pub(super) fn template_fields(&self, id: &str) -> Option<Vec<String>> {
        let template = self.template(id).ok()?;
        Some(template.fields.into_iter().map(str::to_owned).collect())
    }

    pub(super) fn render_template(&self, id: &str, values: &[String]) -> Result<String, String> {
        self.template(id)?.render(values)
    }

    fn template(&self, id: &str) -> Result<PromptTemplate<'_>, String> {
        let action = self
            .pinned_action(id)
            .filter(|action| action.template)
            .ok_or_else(|| crate::i18n::t("chatUi.selectPinnedPrompt"))?;
        PromptTemplate::parse(&action.prompt)
    }

    fn pinned_action(&self, id: &str) -> Option<&PinnedAction> {
        let id = Self::pinned_id(id)?;
        self.pins.entries.iter().find(|action| action.id == id)
    }

    pub(super) fn rows(&self) -> Vec<AgentQuickActionData> {
        BUILT_INS
            .iter()
            .map(|(id, label, _)| AgentQuickActionData {
                id: (*id).into(),
                label: match *id {
                    "captions" => crate::i18n::t("chatUi.titleCaptions"),
                    "trim-pauses" => crate::i18n::t("chatUi.titleTrimPauses"),
                    "fit-9-16" => crate::i18n::t("chatUi.titleVertical"),
                    _ => (*label).to_owned(),
                }
                .into(),
                pinned: false,
            })
            .chain(self.pins.entries.iter().map(|action| {
                AgentQuickActionData {
                    id: format!("pin-{}", action.id).into(),
                    label: action
                        .prompt
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .into(),
                    pinned: true,
                }
            }))
            .collect()
    }

    pub(super) fn pin(&mut self, prompt: &str) -> Result<(), String> {
        self.pin_prompt(prompt, false)
    }

    pub(super) fn pin_template(&mut self, prompt: &str) -> Result<(), String> {
        self.pin_prompt(prompt, true)
    }

    fn pin_prompt(&mut self, prompt: &str, template: bool) -> Result<(), String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("Write a draft before pinning it.".into());
        }

        if template {
            PromptTemplate::parse(prompt)?;
        }

        self.update_pins(|pins| {
            if pins
                .entries
                .iter()
                .any(|action| action.prompt == prompt && action.template == template)
            {
                return Ok(false);
            }

            pins.last_id = pins
                .last_id
                .checked_add(1)
                .ok_or_else(|| "No more pinned action IDs are available.".to_owned())?;
            pins.entries.push(PinnedAction {
                id: pins.last_id,
                prompt: prompt.to_owned(),
                template,
            });
            Ok(true)
        })
    }

    pub(super) fn unpin(&mut self, id: &str) -> Result<(), String> {
        let id =
            Self::pinned_id(id).ok_or_else(|| "Choose a pinned action to remove.".to_owned())?;
        self.update_pins(|pins| {
            let before = pins.entries.len();
            pins.entries.retain(|action| action.id != id);
            Ok(before != pins.entries.len())
        })
    }

    fn pinned_id(id: &str) -> Option<u64> {
        let id: u64 = id.strip_prefix("pin-")?.parse().ok()?;
        (id != 0).then_some(id)
    }

    fn read_pins(path: &Path) -> Result<PinnedActions, String> {
        let pins: PinnedActions = read_json(path)
            .map_err(|_| format!("Could not read pinned actions in {}.", path.display()))?
            .unwrap_or_default();
        if pins.entries.iter().any(|action| {
            action.id == 0
                || action.id > pins.last_id
                || action.prompt.trim().is_empty()
                || (action.template && PromptTemplate::parse(&action.prompt).is_err())
        }) || pins.entries.windows(2).any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(format!("Invalid pinned actions in {}.", path.display()));
        }
        Ok(pins)
    }

    fn update_pins(
        &mut self,
        edit: impl FnOnce(&mut PinnedActions) -> Result<bool, String>,
    ) -> Result<(), String> {
        // Lock a sibling of the JSON file because atomic replacement changes the JSON inode.
        let _lock = self.lock_pins()?;
        let mut pins = Self::read_pins(&self.path)?;
        if edit(&mut pins)? {
            write_json(&self.path, &pins)?;
        }
        self.pins = pins;
        Ok(())
    }

    fn lock_pins(&self) -> Result<File, String> {
        let path = self.path.with_extension("lock");
        let parent = path
            .parent()
            .ok_or_else(|| "No directory for pinned actions.".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not prepare pinned actions: {error}"))?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(path)
            .map_err(|error| format!("Could not open the pinned actions lock: {error}"))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => {
                Err("Another window is changing pinned actions. Try again.".into())
            }
            Err(fs::TryLockError::Error(error)) => {
                Err(format!("Could not lock pinned actions: {error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::storage::Scratch;

    #[test]
    fn pinned_drafts_preserve_text_and_survive_restart_and_removal() {
        let scratch = Scratch::new();
        let mut actions = QuickActions::new(scratch.path());
        assert_eq!(
            actions
                .rows()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["captions", "trim-pauses", "fit-9-16"],
        );
        assert!(
            actions
                .prompt("fit-9-16")
                .unwrap()
                .contains("active timeline")
        );
        assert!(actions.pin("   \n\t").is_err());
        assert!(actions.unpin("captions").is_err());

        actions
            .pin("\nСубтитры крупным шрифтом\nНа русском языке\n")
            .unwrap();
        actions
            .pin("Субтитры крупным шрифтом\nНа русском языке")
            .unwrap();
        let mut reopened = QuickActions::new(scratch.path());
        let rows = reopened.rows();
        assert_eq!(rows.len(), 4);
        assert!(rows[3].pinned);
        assert_eq!(rows[3].label.as_str(), "Субтитры крупным шрифтом");
        assert_eq!(
            reopened.prompt(rows[3].id.as_str()),
            Some("Субтитры крупным шрифтом\nНа русском языке"),
        );

        reopened.unpin(rows[3].id.as_str()).unwrap();
        reopened.pin("New prompt").unwrap();
        let reopened = QuickActions::new(scratch.path());
        assert!(reopened.prompt(rows[3].id.as_str()).is_none());
        assert_eq!(reopened.prompt("pin-2"), Some("New prompt"));
    }

    #[test]
    fn legacy_pins_and_new_literal_pins_keep_template_syntax_as_text() {
        let scratch = Scratch::new();
        let path = scratch.path().join("chat-quick-actions.json");
        let legacy = br#"{"last_id":1,"entries":[{"id":1,"prompt":"Example {{name}}"}]}"#;
        fs::write(path, legacy).unwrap();
        let mut actions = QuickActions::new(scratch.path());

        assert_eq!(actions.prompt("pin-1"), Some("Example {{name}}"));
        assert!(actions.template_fields("pin-1").is_none());
        actions.pin("Use {{braces}} as written").unwrap();
        actions.pin_template("Example {{name}}").unwrap();

        let reopened = QuickActions::new(scratch.path());
        assert_eq!(reopened.prompt("pin-1"), Some("Example {{name}}"));
        assert_eq!(reopened.prompt("pin-2"), Some("Use {{braces}} as written"));
        assert_eq!(reopened.template_fields("pin-3"), Some(vec!["name".into()]));
    }

    #[test]
    fn template_render_uses_ordered_unique_fields_and_literal_values() {
        let scratch = Scratch::new();
        let mut actions = QuickActions::new(scratch.path());
        let prompt =
            r#"Плашка {{Имя}} цвета {{ Цвет }}. Повтори {{ Имя }} и сохрани {"spec":{"on":true}}."#;
        actions.pin_template(prompt).unwrap();
        actions.pin_template(prompt).unwrap();
        let reopened = QuickActions::new(scratch.path());

        assert_eq!(reopened.rows().len(), 4);
        assert!(reopened.prompt("pin-1").is_none());
        assert_eq!(
            reopened.template_fields("pin-1"),
            Some(vec!["Имя".into(), "Цвет".into()]),
        );
        assert!(reopened.render_template("pin-1", &["Саша".into()]).is_err());
        assert!(
            reopened
                .render_template("pin-1", &["Саша".into(), "  ".into()])
                .is_err()
        );
        assert!(
            reopened
                .render_template("captions", &["Саша".into(), "синий".into()])
                .is_err()
        );
        assert_eq!(
            reopened
                .render_template("pin-1", &["{{Цвет}}".into(), "синий".into()])
                .unwrap(),
            r#"Плашка {{Цвет}} цвета синий. Повтори {{Цвет}} и сохрани {"spec":{"on":true}}."#,
        );
    }

    #[test]
    fn invalid_template_does_not_change_existing_pins() {
        let scratch = Scratch::new();
        let mut actions = QuickActions::new(scratch.path());
        actions.pin("Existing prompt").unwrap();
        let before = fs::read(&actions.path).unwrap();

        for prompt in [
            "No fields",
            "Open {{field",
            "Empty {{ }}",
            "Multiline {{field\nname}}",
            "Nested {{outer {{inner}}}}",
        ] {
            assert!(actions.pin_template(prompt).is_err());
        }
        assert_eq!(fs::read(&actions.path).unwrap(), before);
        assert_eq!(actions.rows().len(), 4);
    }

    #[test]
    fn stale_windows_preserve_other_pins_and_the_lock_guards_edits() {
        let scratch = Scratch::new();
        let mut first = QuickActions::new(scratch.path());
        let mut second = QuickActions::new(scratch.path());
        first.pin("First window").unwrap();
        let lock = first.lock_pins().unwrap();
        assert!(second.pin("While locked").is_err());
        drop(lock);
        second.pin("Second window").unwrap();
        first.unpin("pin-1").unwrap();
        assert_eq!(first.prompt("pin-2"), Some("Second window"));
        assert_eq!(first.rows().len(), 4);
    }

    #[test]
    fn malformed_settings_are_not_overwritten() {
        let scratch = Scratch::new();
        let mut actions = QuickActions::new(scratch.path());
        let invalid = br#"{"last_id":0,"entries":[{"id":1,"prompt":"Saved prompt"}]}"#;
        fs::write(&actions.path, invalid).unwrap();
        assert!(actions.pin("New prompt").is_err());
        assert_eq!(fs::read(&actions.path).unwrap(), invalid);
        assert_eq!(actions.rows().len(), 3);
    }
}
