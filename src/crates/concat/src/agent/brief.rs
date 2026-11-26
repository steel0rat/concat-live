// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::storage::{read_json, write_json};

const MAX_FIELD_CHARS: usize = 4096;
static EMPTY_BRIEF: ProjectBrief = ProjectBrief {
    goal: String::new(),
    audience: String::new(),
    style: String::new(),
};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectBrief {
    pub(super) goal: String,
    pub(super) audience: String,
    pub(super) style: String,
}

impl ProjectBrief {
    fn validate(&self) -> Result<(), String> {
        for (label, value) in [
            ("Goal", &self.goal),
            ("Audience", &self.audience),
            ("Style", &self.style),
        ] {
            if value.chars().take(MAX_FIELD_CHARS + 1).count() > MAX_FIELD_CHARS {
                return Err(format!(
                    "{label} can contain up to {MAX_FIELD_CHARS} characters."
                ));
            }
        }
        Ok(())
    }
}

pub(super) struct BriefStore {
    file: PathBuf,
    stored: Option<ProjectBrief>,
}

impl BriefStore {
    pub(super) fn load(folder: &Path) -> Result<Self, String> {
        let file = folder.join("concat-brief.json");
        let stored = read_json::<ProjectBrief>(&file)?;
        if let Some(brief) = &stored {
            brief.validate()?;
        }
        Ok(Self { file, stored })
    }

    pub(super) fn brief(&self) -> &ProjectBrief {
        self.stored.as_ref().unwrap_or(&EMPTY_BRIEF)
    }

    pub(super) fn save(&mut self, goal: &str, audience: &str, style: &str) -> Result<(), String> {
        let next = ProjectBrief {
            goal: goal.trim().to_owned(),
            audience: audience.trim().to_owned(),
            style: style.trim().to_owned(),
        };
        next.validate()?;

        let _lock = self.lock()?;
        if read_json::<ProjectBrief>(&self.file)? != self.stored {
            return Err(
                "The project brief changed on disk. Reopen the project to reload it.".into(),
            );
        }
        if &next != self.brief() {
            write_json(&self.file, &next)?;
            self.stored = Some(next);
        }
        Ok(())
    }

    fn lock(&self) -> Result<File, String> {
        // Keep the lock on an inode that survives replacement of the brief.
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(self.file.with_file_name(".concat-brief.lock"))
            .map_err(|error| format!("Could not open the project brief lock. {error}"))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => {
                Err("Another window is saving the project brief. Try again.".into())
            }
            Err(TryLockError::Error(error)) => {
                Err(format!("Could not lock the project brief. {error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::storage::Scratch;
    use super::*;

    #[test]
    fn project_brief_round_trips_and_clears() {
        let folder = Scratch::new();
        let file = folder.path().join("concat-brief.json");
        let mut store = BriefStore::load(folder.path()).unwrap();
        assert_eq!(store.brief(), &ProjectBrief::default());
        store.save(" ", "", "").unwrap();
        assert!(!file.exists());

        store
            .save(
                "  Показать процесс\nза минуту ",
                "Новички",
                "Бумажная анимация",
            )
            .unwrap();
        let reloaded = BriefStore::load(folder.path()).unwrap();
        assert_eq!(reloaded.brief(), store.brief());
        assert_eq!(reloaded.brief().goal, "Показать процесс\nза минуту");
        assert_eq!(reloaded.brief().audience, "Новички");
        assert_eq!(reloaded.brief().style, "Бумажная анимация");

        store.save("", "", "").unwrap();
        let cleared = BriefStore::load(folder.path()).unwrap();
        assert_eq!(cleared.brief(), &ProjectBrief::default());
    }

    #[test]
    fn stale_and_corrupt_project_briefs_are_preserved() {
        let folder = Scratch::new();
        let file = folder.path().join("concat-brief.json");
        let mut first = BriefStore::load(folder.path()).unwrap();
        let mut second = BriefStore::load(folder.path()).unwrap();
        first.save("Current goal", "", "").unwrap();
        let current = fs::read(&file).unwrap();
        assert!(
            second
                .save("Stale goal", "", "")
                .unwrap_err()
                .contains("changed on disk")
        );
        assert_eq!(fs::read(&file).unwrap(), current);
        assert_eq!(second.brief(), &ProjectBrief::default());

        let corrupt = b"invalid json";
        fs::write(&file, corrupt).unwrap();
        assert!(BriefStore::load(folder.path()).is_err());
        assert!(first.save("Replacement goal", "", "").is_err());
        assert_eq!(fs::read(&file).unwrap(), corrupt);
        assert_eq!(first.brief().goal, "Current goal");
    }

    #[test]
    fn unsupported_project_brief_fields_are_preserved() {
        let folder = Scratch::new();
        let file = folder.path().join("concat-brief.json");
        let mut store = BriefStore::load(folder.path()).unwrap();
        let newer_brief = br#"{"goal":"","audience":"","style":"","other":"preserve"}"#;
        fs::write(&file, newer_brief).unwrap();

        assert!(BriefStore::load(folder.path()).is_err());
        assert!(store.save("Replacement goal", "", "").is_err());
        assert_eq!(fs::read(&file).unwrap(), newer_brief);
    }
}
