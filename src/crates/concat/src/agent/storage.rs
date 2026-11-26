// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Private records owned by the embedded chat.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Serialize, de::DeserializeOwned};

pub(super) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            format!(
                "Invalid chat data in {} at line {}, column {}.",
                path.display(),
                error.line(),
                error.column()
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

pub(super) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| format!("Could not encode chat data: {error}"))?;
    replace_private(path, &encoded)
        .map_err(|error| format!("Could not save {}: {error}", path.display()))
}

fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("the chat data path has no directory"))?;
    fs::create_dir_all(parent)?;

    // Create a fresh sibling so two windows can save without sharing a staging file.
    let (stage, mut file) = loop {
        let nonce = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let stage = parent.join(format!(".concat-chat-{}-{nonce}.part", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&stage) {
            Ok(file) => break (stage, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&stage, path));
    if result.is_err() {
        let _ = fs::remove_file(stage);
    }
    result
}

#[cfg(test)]
pub(super) struct Scratch(std::path::PathBuf);

#[cfg(test)]
impl Scratch {
    pub(super) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("concat-chat-{}-{nonce}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create the test directory: {error}"),
            }
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_preserves_private_readable_data_and_consumes_staging_files() {
        let root = Scratch::new();
        let file = root.path().join("agent.json");
        assert_eq!(read_json::<String>(&file).unwrap(), None);

        write_json(&file, &"draft").unwrap();
        write_json(&file, &"new draft").unwrap();
        assert_eq!(
            read_json::<String>(&file).unwrap().as_deref(),
            Some("new draft")
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn a_malformed_record_is_reported_instead_of_treated_as_missing() {
        let root = Scratch::new();
        let file = root.path().join("agent.json");
        fs::write(&file, b"unfinished").unwrap();
        assert!(read_json::<String>(&file).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"unfinished");
    }
}
