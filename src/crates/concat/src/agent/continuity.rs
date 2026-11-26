// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Local continuity for the chat attached to a project folder.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::storage::{read_json, write_json};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct ChatState {
    pub(super) thread: Option<String>,
    pub(super) title: String,
    pub(super) draft: String,
    pub(super) files: Vec<PathBuf>,
}

impl ChatState {
    pub(super) fn prepend_pending(&mut self, text: &str, files: &[PathBuf]) {
        if !text.is_empty() {
            self.draft = if self.draft.is_empty() {
                text.to_owned()
            } else {
                format!("{text}\n\n{}", self.draft)
            };
        }
        for path in files {
            if !self.files.contains(path) {
                self.files.push(path.clone());
            }
        }
    }
}

pub(super) fn project_key(folder: &Path) -> Result<PathBuf, String> {
    folder
        .canonicalize()
        .map_err(|error| format!("Could not identify the project folder: {error}"))
}

pub(super) struct ProjectChats {
    file: PathBuf,
    pending: HashMap<PathBuf, ChatState>,
}

impl ProjectChats {
    pub(super) fn new(config: &Path) -> Self {
        Self {
            file: config.join("project-chats.json"),
            pending: HashMap::new(),
        }
    }

    pub(super) fn load(&self, project: &Path) -> Result<ChatState, String> {
        if let Some(state) = self.pending.get(project) {
            return Ok(state.clone());
        }
        let records = read_json::<HashMap<PathBuf, ChatState>>(&self.file)?;
        Ok(records
            .and_then(|mut records| records.remove(project))
            .unwrap_or_default())
    }

    pub(super) fn queue(&mut self, project: PathBuf, state: ChatState) {
        self.pending.insert(project, state);
    }

    pub(super) fn flush(&mut self) -> Result<(), String> {
        if self.pending.is_empty() {
            return Ok(());
        }
        fs::create_dir_all(self.file.parent().expect("a config directory was provided"))
            .map_err(|error| format!("Could not open the chat settings directory: {error}"))?;

        // Keep the lock on a sibling inode while the JSON file is replaced.
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let lock = options
            .open(self.file.with_extension("lock"))
            .map_err(|error| format!("Could not open the project chat lock: {error}"))?;
        lock.try_lock()
            .map_err(|error| format!("Could not lock project chats: {error}"))?;

        let mut records = read_json::<HashMap<PathBuf, ChatState>>(&self.file)?.unwrap_or_default();
        records.extend(
            self.pending
                .iter()
                .map(|(key, state)| (key.clone(), state.clone())),
        );
        write_json(&self.file, &records)?;
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::storage::Scratch;
    use super::*;

    #[test]
    fn separate_writers_keep_the_last_chat_and_draft_for_each_project() {
        let root = Scratch::new();
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let first = project_key(&first).unwrap();
        let second = project_key(&second).unwrap();
        let mut one = ProjectChats::new(root.path());
        let mut two = ProjectChats::new(root.path());

        let expected_first = ChatState {
            thread: Some("conversation".into()),
            title: "Cut the intro".into(),
            draft: "Adjust the lighting".into(),
            files: vec![first.join("source.mp4")],
        };
        let expected_second = ChatState {
            draft: "Prepare the end credits".into(),
            ..Default::default()
        };

        one.queue(first.clone(), expected_first.clone());
        two.queue(second.clone(), expected_second.clone());
        one.flush().unwrap();
        two.flush().unwrap();

        let reopened = ProjectChats::new(root.path());
        assert_eq!(reopened.load(&first).unwrap(), expected_first);
        assert_eq!(reopened.load(&second).unwrap(), expected_second);
    }

    #[test]
    fn unreadable_state_is_not_overwritten_and_pending_drafts_stay_available() {
        let root = Scratch::new();
        let mut chats = ProjectChats::new(root.path());
        let project = root.path().join("project");
        let original = b"damaged";
        fs::write(&chats.file, original).unwrap();
        assert!(chats.load(&project).is_err());

        let pending = ChatState {
            draft: "Continue here".into(),
            ..Default::default()
        };
        chats.queue(project.clone(), pending.clone());

        assert!(chats.flush().is_err());
        assert_eq!(fs::read(&chats.file).unwrap(), original);
        assert_eq!(chats.load(&project).unwrap(), pending);
    }

    #[test]
    fn restoring_an_unacknowledged_send_keeps_both_sets_of_text_and_files() {
        let mut state = ChatState {
            draft: "Follow-up".into(),
            files: vec!["cover.png".into()],
            ..Default::default()
        };
        state.prepend_pending("Earlier", &["cover.png".into(), "voice.wav".into()]);
        assert_eq!(state.draft, "Earlier\n\nFollow-up");
        assert_eq!(
            state.files,
            [PathBuf::from("cover.png"), PathBuf::from("voice.wav")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_paths_share_the_same_project_record() {
        let root = Scratch::new();
        let folder = root.path().join("project");
        let alias = root.path().join("alias");
        fs::create_dir(&folder).unwrap();
        std::os::unix::fs::symlink(&folder, &alias).unwrap();
        assert_eq!(project_key(&folder).unwrap(), project_key(&alias).unwrap());
    }
}
