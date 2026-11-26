// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Publish a complete set of files together under a fresh directory name.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Temporary folder for a single audio or review export with several files.
pub struct OutputSet {
    parent: PathBuf,
    staged: PathBuf,
    members: Vec<String>,
}

impl OutputSet {
    /// Creates a fresh scratch folder beside the proposed output.
    pub fn new(parent: &Path, members: &[&str]) -> io::Result<Self> {
        if !parent.is_dir() || members.is_empty() || members.iter().any(|name| !single_name(name)) {
            return Err(invalid("Invalid export directory or file name."));
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..64 {
            let next = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let staged = parent.join(format!(
                ".concat-export-{}-{stamp}-{next}",
                std::process::id()
            ));
            match fs::create_dir(&staged) {
                Ok(()) => {
                    return Ok(Self {
                        parent: parent.to_path_buf(),
                        staged,
                        members: members.iter().map(|name| (*name).to_owned()).collect(),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(invalid("Could not claim a temporary export directory."))
    }

    /// Where the caller writes all of the files in the set.
    pub fn path(&self) -> &Path {
        &self.staged
    }

    /// Moves all declared files into a new, visible folder. Existing folders
    /// keep their name; a later export gets a numeric suffix.
    pub fn publish(self, name: &str) -> io::Result<PathBuf> {
        if !single_name(name)
            || self
                .members
                .iter()
                .any(|file| !self.staged.join(file).is_file())
        {
            return Err(invalid(
                "The export file set is incomplete or its name is invalid.",
            ));
        }
        for index in 1..=1000 {
            let destination = if index == 1 {
                self.parent.join(name)
            } else {
                self.parent.join(format!("{name}-{index}"))
            };
            #[cfg(unix)]
            {
                // Reserve the name across processes. Renaming over the
                // empty reservation publishes the whole set at once.
                match fs::create_dir(&destination) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
                match fs::rename(&self.staged, &destination) {
                    Ok(()) => return Ok(destination),
                    Err(error) => {
                        let _ = fs::remove_dir(&destination);
                        return Err(error);
                    }
                }
            }
            #[cfg(not(unix))]
            match fs::rename(&self.staged, &destination) {
                Ok(()) => return Ok(destination),
                Err(error) if destination.exists() || error.kind() == ErrorKind::AlreadyExists => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(invalid(
            "No unused folder name is available for this export.",
        ))
    }
}

impl Drop for OutputSet {
    fn drop(&mut self) {
        if self.staged.is_dir() {
            for file in &self.members {
                let _ = fs::remove_file(self.staged.join(file));
            }
            let _ = fs::remove_dir(&self.staged);
        }
    }
}

fn single_name(name: &str) -> bool {
    let path = Path::new(name);
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(first)) if first == path.as_os_str())
        && components.next().is_none()
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message)
}
