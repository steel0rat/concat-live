// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat contributors

//! Immutable skill releases and atomic activation in Codex's user skill folder.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::{Package, Version};
use crate::dirs::AppDirs;

/// The skill bound to the editor's native chat.
pub const NAME: &str = "steel0rat-video-maker-skill";
/// The skill's independent release channel.
pub const REPO: &str = "steel0rat/steel0rat-video-maker-skill";

const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

/// A validated skill release manifest.
#[derive(Clone, Debug)]
pub struct Manifest {
    /// The independent skill version.
    pub version: Version,
    /// The oldest compatible editor version.
    pub minimum_app: Version,
    /// The immutable archive and its expected digest.
    pub archive: Package,
}

/// An unpacked release, ready for activation or already active.
#[derive(Clone, Debug)]
pub struct InstalledSkill {
    /// The independent skill version.
    pub version: Version,
    /// Absolute path to the release's SKILL.md.
    pub path: PathBuf,
    archive_sha256: String,
    tree_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    schema: u32,
    version: String,
    archive_sha256: String,
    tree_sha256: String,
}

impl InstalledSkill {
    fn record(&self) -> Record {
        Record {
            schema: 1,
            version: self.version.to_string(),
            archive_sha256: self.archive_sha256.clone(),
            tree_sha256: self.tree_sha256.clone(),
        }
    }
}

fn root(dirs: &AppDirs) -> PathBuf {
    dirs.data.join("skills").join(NAME)
}

fn release_path(dirs: &AppDirs, version: Version, digest: &str) -> PathBuf {
    root(dirs)
        .join("versions")
        .join(format!("{version}-{digest}"))
        .join(NAME)
}

/// Reads the release manifest and refuses another repository, skill or tag.
pub fn manifest_from_json(text: &str) -> Result<Manifest, String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("the skill manifest does not read as JSON: {error}"))?;
    let string = |key: &str| {
        value[key]
            .as_str()
            .ok_or_else(|| format!("the skill manifest has no {key}"))
    };
    let version = Version::parse(string("version")?).ok_or("the skill version is not stable")?;
    if value["schema"] != 1
        || value["kind"] != "concat-live-skill"
        || value["name"] != NAME
        || string("version")? != version.to_string()
        || string("tag")? != version.tag()
    {
        return Err("the manifest is not for the requested Concat Live skill".into());
    }
    let minimum_text = string("minAppVersion")?;
    let minimum_app = Version::parse(minimum_text)
        .filter(|v| v.to_string() == minimum_text)
        .ok_or("the minimum editor version is invalid")?;
    let item = &value["archive"];
    let field = |key: &str| {
        item[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("the skill archive has no {key}"))
    };
    let archive = Package {
        file: field("file")?,
        url: field("url")?,
        bytes: item["bytes"]
            .as_u64()
            .ok_or("the skill archive has no byte size")?,
        sha256: field("sha256")?,
    };
    super::validate_package(&archive, REPO, Some(&version.tag()))?;
    if archive.file != format!("{NAME}-{version}.zip") || archive.bytes > MAX_ARCHIVE_BYTES {
        return Err("the skill archive's name or size is invalid".into());
    }
    Ok(Manifest {
        version,
        minimum_app,
        archive,
    })
}

/// Finds the latest published stable skill compatible with this editor.
/// No published release is a normal result for a new repository.
pub fn fetch_latest(app_version: Version) -> Result<Option<Manifest>, String> {
    let json = super::get_text(&format!(
        "https://api.github.com/repos/{REPO}/releases?per_page=100"
    ))?;
    let releases = super::releases_from_json_min(&json, Version::new(0, 0, 0))?;
    let mut incompatible = None;
    for release in releases {
        let text = super::get_text(&format!(
            "https://github.com/{REPO}/releases/download/{}/manifest.json",
            release.tag
        ))?;
        let manifest = manifest_from_json(&text)?;
        if manifest.version != release.version {
            return Err("the skill manifest and GitHub release report different versions".into());
        }
        if manifest.minimum_app > app_version {
            incompatible.get_or_insert(manifest.minimum_app);
            continue;
        }
        return Ok(Some(manifest));
    }
    match incompatible {
        Some(version) => Err(format!(
            "the new skill requires Concat Live {version} or newer"
        )),
        None => Ok(None),
    }
}

/// The active managed release. A local source installation without a
/// published release has no managed record and is left untouched.
pub fn current(dirs: &AppDirs) -> Result<Option<InstalledSkill>, String> {
    let path = root(dirs).join("current.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read the active skill record: {error}")),
    };
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the active skill record is invalid: {error}"))?;
    let version = Version::parse(&record.version)
        .filter(|v| v.to_string() == record.version)
        .ok_or("the active skill version is invalid")?;
    if record.schema != 1
        || !super::valid_digest(&record.archive_sha256)
        || !super::valid_digest(&record.tree_sha256)
    {
        return Err("the active skill record has an invalid schema or digest".into());
    }
    let skill = InstalledSkill {
        version,
        path: release_path(dirs, version, &record.archive_sha256).join("SKILL.md"),
        archive_sha256: record.archive_sha256,
        tree_sha256: record.tree_sha256,
    };
    validate_prepared(dirs, &skill)?;
    Ok(Some(skill))
}

/// Downloads and unpacks a release without changing the active skill.
/// Archive structure is audited before any entry is extracted.
pub fn prepare(
    manifest: &Manifest,
    dirs: &AppDirs,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<InstalledSkill, String> {
    if !cfg!(target_os = "macos") {
        return Err("automatic skill installation currently supports macOS".into());
    }
    super::validate_package(&manifest.archive, REPO, Some(&manifest.version.tag()))?;
    if manifest.archive.bytes > MAX_ARCHIVE_BYTES || manifest.minimum_app > Version::current() {
        return Err("the skill release exceeds its size limit or needs a newer editor".into());
    }
    let archive = super::download_verified(
        &manifest.archive,
        REPO,
        &root(dirs).join("downloads"),
        cancel,
        progress,
    )?;
    let bytes = std::fs::read(&archive)
        .map_err(|error| format!("could not read the skill archive: {error}"))?;
    let entries = audit_archive(&bytes)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(super::CANCELLED.into());
    }
    let destination = release_path(dirs, manifest.version, &manifest.archive.sha256);
    let final_parent = destination
        .parent()
        .ok_or("the skill release has no parent")?;
    if final_parent.exists() {
        let saved: Record = serde_json::from_slice(
            &std::fs::read(final_parent.join("prepared.json"))
                .map_err(|error| format!("could not read the prepared release: {error}"))?,
        )
        .map_err(|error| format!("the prepared release record is invalid: {error}"))?;
        let skill = InstalledSkill {
            version: manifest.version,
            path: destination.join("SKILL.md"),
            archive_sha256: manifest.archive.sha256.clone(),
            tree_sha256: saved.tree_sha256,
        };
        validate_prepared(dirs, &skill)?;
        return Ok(skill);
    }
    let versions = root(dirs).join("versions");
    std::fs::create_dir_all(&versions)
        .map_err(|error| format!("could not create the skill release folder: {error}"))?;
    let unpacked = versions.join(format!(".unpack-{}", super::nonce()));
    std::fs::create_dir(&unpacked)
        .map_err(|error| format!("could not create the skill staging folder: {error}"))?;
    super::run(
        Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&archive)
            .arg(&unpacked),
    )?;
    if cancel.load(Ordering::Relaxed) {
        return Err(super::CANCELLED.into());
    }
    let tree = unpacked.join(NAME);
    validate_tree(&tree, &entries)?;
    validate_skill_document(&tree.join("SKILL.md"))?;
    let tree_sha256 = tree_digest(&tree)?;
    let skill = InstalledSkill {
        version: manifest.version,
        path: destination.join("SKILL.md"),
        archive_sha256: manifest.archive.sha256.clone(),
        tree_sha256,
    };
    write_new(
        &unpacked.join("prepared.json"),
        &serde_json::to_vec_pretty(&skill.record()).map_err(|e| e.to_string())?,
    )?;
    if final_parent.exists() {
        return Err("another updater prepared the same skill release".into());
    }
    std::fs::rename(&unpacked, final_parent)
        .map_err(|error| format!("could not publish the prepared skill: {error}"))?;
    validate_prepared(dirs, &skill)?;
    Ok(skill)
}

fn validate_prepared(dirs: &AppDirs, skill: &InstalledSkill) -> Result<(), String> {
    if !super::valid_digest(&skill.archive_sha256) || !super::valid_digest(&skill.tree_sha256) {
        return Err("the prepared skill has an invalid digest".into());
    }
    let expected = release_path(dirs, skill.version, &skill.archive_sha256).join("SKILL.md");
    if expected != skill.path || !skill.path.is_absolute() {
        return Err("the prepared skill path is outside the managed release folder".into());
    }
    let tree = skill
        .path
        .parent()
        .ok_or("the prepared skill has no folder")?;
    let canonical_root = root(dirs)
        .canonicalize()
        .map_err(|error| format!("could not read the managed skill root: {error}"))?;
    if !tree
        .canonicalize()
        .map_err(|error| format!("could not read the prepared skill: {error}"))?
        .starts_with(&canonical_root)
    {
        return Err("the prepared skill folder resolves outside its managed root".into());
    }
    validate_skill_document(&skill.path)?;
    if tree_digest(tree)? != skill.tree_sha256 {
        return Err("the prepared skill's files have changed".into());
    }
    Ok(())
}

fn validate_skill_document(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| format!("the release has no SKILL.md: {error}"))?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return Err("SKILL.md is not an ordinary bounded text file".into());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("SKILL.md is not readable UTF-8: {error}"))?;
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return Err("SKILL.md has no YAML frontmatter".into());
    }
    let mut front = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line == "---" {
            closed = true;
            break;
        }
        front.push(line);
    }
    let name = front
        .iter()
        .filter_map(|line| line.strip_prefix("name:"))
        .map(str::trim)
        .collect::<Vec<_>>();
    let description = front.iter().any(|line| {
        line.strip_prefix("description:")
            .is_some_and(|v| !v.trim().is_empty())
    });
    if !closed || name != [NAME] || !description || lines.all(|line| line.trim().is_empty()) {
        return Err("SKILL.md does not describe the expected video-maker skill".into());
    }
    Ok(())
}

fn tree_files(tree: &Path) -> Result<BTreeMap<String, (u64, String)>, String> {
    fn visit(
        base: &Path,
        dir: &Path,
        out: &mut BTreeMap<String, (u64, String)>,
        bytes: &mut u64,
    ) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink() {
                return Err("a skill release must not contain symbolic links".into());
            }
            if meta.is_dir() {
                visit(base, &path, out, bytes)?;
            } else if meta.is_file() {
                *bytes += meta.len();
                if *bytes > MAX_UNPACKED_BYTES || out.len() >= MAX_ENTRIES {
                    return Err("the extracted skill exceeds its size or entry limit".into());
                }
                let rel = path
                    .strip_prefix(base)
                    .map_err(|e| e.to_string())?
                    .to_str()
                    .ok_or("the skill path is not UTF-8")?
                    .replace('\\', "/");
                out.insert(rel, (meta.len(), super::file_digest(&path)?));
            } else {
                return Err("a skill release contains a special file".into());
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(tree, tree, &mut out, &mut 0)?;
    Ok(out)
}

fn tree_digest(tree: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for (path, (bytes, hash)) in tree_files(tree)? {
        digest.update(path.as_bytes());
        digest.update([0]);
        digest.update(bytes.to_le_bytes());
        digest.update(hash.as_bytes());
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Atomically activates a managed release and preserves the previous
/// installed link or directory without touching its source target.
pub fn activate(dirs: &AppDirs, skill: &InstalledSkill) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("automatic skill activation currently supports macOS".into());
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or("HOME does not name an absolute user folder")?;
    activate_at(dirs, skill, &home.join(".agents/skills"))
}

fn activate_at(dirs: &AppDirs, skill: &InstalledSkill, agents: &Path) -> Result<(), String> {
    activate_with(dirs, skill, agents, |source, target| {
        std::fs::rename(source, target)
    })
}

fn activate_with(
    dirs: &AppDirs,
    skill: &InstalledSkill,
    agents: &Path,
    commit: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
    validate_prepared(dirs, skill)?;
    if !agents.is_absolute() {
        return Err("the user skill folder is not absolute".into());
    }
    std::fs::create_dir_all(agents)
        .map_err(|error| format!("could not create the user skill folder: {error}"))?;
    let managed = root(dirs);
    let state = managed.join("current.json");
    let old_state = match std::fs::read(&state) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("could not preserve the active skill state: {e}")),
    };
    let pending = managed.join(format!("current.{}.pending", super::nonce()));
    let encoded = serde_json::to_vec_pretty(&skill.record()).map_err(|e| e.to_string())?;
    write_new(&pending, &encoded)?;
    let installed = agents.join(NAME);
    let staged = agents.join(format!(".{NAME}.{}", super::nonce()));
    create_link(
        skill.path.parent().ok_or("the skill has no folder")?,
        &staged,
    )?;
    let previous_exists = std::fs::symlink_metadata(&installed).is_ok();
    if previous_exists {
        let meta = std::fs::symlink_metadata(&installed).map_err(|e| e.to_string())?;
        if !meta.file_type().is_symlink() && !meta.is_dir() {
            return Err(
                "the installed skill path is neither a directory nor a symbolic link".into(),
            );
        }
    }
    let backup = managed.join("backups").join(super::nonce());
    std::fs::create_dir_all(&backup)
        .map_err(|e| format!("could not create the skill backup folder: {e}"))?;
    if let Some(bytes) = &old_state {
        write_new(&backup.join("current.json"), bytes)?;
    }
    let previous = backup.join(NAME);
    if previous_exists {
        swap_entries(&staged, &installed)?;
        if let Err(error) = std::fs::rename(&staged, &previous) {
            swap_entries(&staged, &installed)?;
            return Err(format!(
                "could not preserve the previous installation; activation rolled back: {error}"
            ));
        }
    } else {
        move_new(&staged, &installed)?;
    }
    let committed = commit(&pending, &state)
        .map_err(|error| format!("could not publish the active skill state: {error}"))
        .and_then(|()| {
            if std::fs::read(&state).map_err(|e| e.to_string())? != encoded
                || installed.canonicalize().map_err(|e| e.to_string())?
                    != skill
                        .path
                        .parent()
                        .ok_or("the skill has no folder")?
                        .canonicalize()
                        .map_err(|e| e.to_string())?
            {
                return Err("the active skill failed its post-install check".into());
            }
            Ok(())
        });
    if let Err(error) = committed {
        if previous_exists {
            std::fs::rename(&previous, &staged).map_err(|rollback| {
                format!(
                    "{error}; could not stage rollback: {rollback}; previous skill: {}",
                    previous.display()
                )
            })?;
            swap_entries(&staged, &installed)?;
            std::fs::rename(&staged, backup.join("failed-install-link"))
                .map_err(|e| e.to_string())?;
        } else {
            std::fs::rename(&installed, backup.join("failed-install-link"))
                .map_err(|e| e.to_string())?;
        }
        if let Some(bytes) = old_state {
            let restore = managed.join(format!("current.{}.restore", super::nonce()));
            write_new(&restore, &bytes)?;
            std::fs::rename(&restore, &state).map_err(|e| e.to_string())?;
        } else if state.exists() {
            std::fs::rename(&state, backup.join("failed-current.json"))
                .map_err(|e| e.to_string())?;
        }
        return Err(format!("{error}; previous skill installation restored"));
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("could not create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    if std::fs::read(path).map_err(|e| e.to_string())? != bytes {
        return Err("the skill state write did not verify".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn create_link(target: &Path, path: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, path)
        .map_err(|e| format!("could not create the skill link: {e}"))
}
#[cfg(not(target_os = "macos"))]
fn create_link(_: &Path, _: &Path) -> Result<(), String> {
    Err("automatic skill activation currently supports macOS".into())
}

#[cfg(target_os = "macos")]
fn rename_flags(left: &Path, right: &Path, flags: u32) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let left = std::ffi::CString::new(left.as_os_str().as_bytes())
        .map_err(|_| "the skill path contains NUL")?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes())
        .map_err(|_| "the skill path contains NUL")?;
    // Both C strings live across this call; rename exchanges only directory entries.
    let code = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            flags,
        )
    };
    if code != 0 {
        return Err(format!(
            "could not atomically switch the skill installation: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}
fn swap_entries(left: &Path, right: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        rename_flags(left, right, libc::RENAME_SWAP)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (left, right);
        Err("atomic skill activation requires macOS".into())
    }
}
fn move_new(left: &Path, right: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        rename_flags(left, right, libc::RENAME_EXCL)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (left, right);
        Err("atomic skill activation requires macOS".into())
    }
}

#[derive(Debug)]
struct ZipEntry {
    path: String,
    bytes: u64,
    directory: bool,
}

fn le16(data: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .ok_or("the ZIP header is truncated")?
            .try_into()
            .map_err(|_| "invalid ZIP u16")?,
    ))
}
fn le32(data: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or("the ZIP header is truncated")?
            .try_into()
            .map_err(|_| "invalid ZIP u32")?,
    ))
}

fn audit_archive(data: &[u8]) -> Result<Vec<ZipEntry>, String> {
    if data.len() < 22 || data.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err("the skill ZIP has an invalid size".into());
    }
    let first = data.len().saturating_sub(22 + 65535);
    let end = (first..=data.len() - 22)
        .rev()
        .find(|&i| {
            data[i..i + 4] == [0x50, 0x4b, 0x05, 0x06]
                && le16(data, i + 20).is_ok_and(|n| i + 22 + n as usize == data.len())
        })
        .ok_or("the skill ZIP has no complete central directory")?;
    let count = le16(data, end + 10)? as usize;
    let central = le32(data, end + 16)? as usize;
    let size = le32(data, end + 12)? as usize;
    if le16(data, end + 4)? != 0
        || le16(data, end + 6)? != 0
        || le16(data, end + 8)? as usize != count
        || count == 0
        || count > MAX_ENTRIES
        || central.checked_add(size) != Some(end)
    {
        return Err("multipart, ZIP64 or oversized skill archives are not supported".into());
    }
    let mut offset = central;
    let mut names = BTreeSet::new();
    let mut ranges = Vec::new();
    let mut total = 0u64;
    let mut entries = Vec::new();
    for _ in 0..count {
        if le32(data, offset)? != 0x02014b50 || offset + 46 > end {
            return Err("the skill ZIP central directory is invalid".into());
        }
        let flags = le16(data, offset + 8)?;
        let method = le16(data, offset + 10)?;
        let compressed = le32(data, offset + 20)? as usize;
        let unpacked = le32(data, offset + 24)? as u64;
        let name_len = le16(data, offset + 28)? as usize;
        let extra = le16(data, offset + 30)? as usize;
        let comment = le16(data, offset + 32)? as usize;
        let attrs = le32(data, offset + 38)?;
        let local = le32(data, offset + 42)? as usize;
        let finish = offset
            .checked_add(46 + name_len + extra + comment)
            .ok_or("the ZIP entry overflows")?;
        if finish > end
            || extra != 0
            || le16(data, offset + 34)? != 0
            || flags & !0x0800 != 0
            || ![0, 8].contains(&method)
        {
            return Err("the skill ZIP uses unsupported flags, methods or metadata".into());
        }
        let raw = &data[offset + 46..offset + 46 + name_len];
        if flags & 0x800 == 0 && !raw.is_ascii() {
            return Err("the ZIP filename has an ambiguous encoding".into());
        }
        let name = std::str::from_utf8(raw).map_err(|_| "the ZIP filename is not UTF-8")?;
        let directory = name.ends_with('/');
        let clean = name.trim_end_matches('/');
        let relative = clean.strip_prefix(&format!("{NAME}/"));
        if (clean != NAME && relative.is_none())
            || (clean == NAME && !directory)
            || clean.len() > 1024
            || clean.split('/').count() > 32
            || clean.contains(['\\', ':'])
            || clean.chars().any(char::is_control)
            || clean
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !names.insert(clean.to_lowercase())
        {
            return Err("the skill ZIP contains an unsafe or duplicate path".into());
        }
        let kind = (attrs >> 16) & 0xf000;
        if ![0, 0x4000, 0x8000].contains(&kind)
            || (kind == 0x4000 && !directory)
            || (kind == 0x8000 && directory)
            || (directory && unpacked != 0)
            || unpacked > MAX_ARCHIVE_BYTES
        {
            return Err("the skill ZIP contains a link, special file or oversized entry".into());
        }
        total = total
            .checked_add(unpacked)
            .ok_or("the ZIP size overflows")?;
        if total > MAX_UNPACKED_BYTES {
            return Err("the unpacked skill exceeds its size limit".into());
        }
        if le32(data, local)? != 0x04034b50
            || local + 30 > central
            || le16(data, local + 6)? != flags
            || le16(data, local + 8)? != method
            || le32(data, local + 14)? != le32(data, offset + 16)?
            || le32(data, local + 18)? as usize != compressed
            || le32(data, local + 22)? as u64 != unpacked
            || le16(data, local + 26)? as usize != name_len
            || le16(data, local + 28)? != 0
        {
            return Err("the ZIP local and central headers disagree".into());
        }
        let content = local
            .checked_add(30 + name_len)
            .ok_or("the ZIP local header overflows")?;
        let content_end = content
            .checked_add(compressed)
            .ok_or("the ZIP data overflows")?;
        if content_end > central
            || data.get(local + 30..content) != Some(raw)
            || (method == 0 && compressed as u64 != unpacked)
        {
            return Err("the ZIP entry's data range is invalid".into());
        }
        ranges.push((local, content_end));
        entries.push(ZipEntry {
            path: relative.unwrap_or("").to_owned(),
            bytes: unpacked,
            directory,
        });
        offset = finish;
    }
    ranges.sort_unstable();
    let mut prior = 0;
    for (start, end) in ranges {
        if start != prior {
            return Err("the ZIP has overlapping or hidden local entries".into());
        }
        prior = end;
    }
    if prior != central
        || offset != end
        || !entries.iter().any(|e| e.path == "SKILL.md" && !e.directory)
    {
        return Err("the skill ZIP is incomplete or has no SKILL.md".into());
    }
    Ok(entries)
}

fn validate_tree(tree: &Path, entries: &[ZipEntry]) -> Result<(), String> {
    let actual = tree_files(tree)?;
    let expected: BTreeMap<_, _> = entries
        .iter()
        .filter(|e| !e.directory)
        .map(|e| (e.path.clone(), e.bytes))
        .collect();
    let sizes: BTreeMap<_, _> = actual.into_iter().map(|(p, (size, _))| (p, size)).collect();
    if sizes != expected {
        return Err("the extracted skill differs from its ZIP inventory".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &str = "---\nname: steel0rat-video-maker-skill\ndescription: Native video editing\n---\nCreate an editable video.\n";

    fn manifest_value() -> serde_json::Value {
        serde_json::json!({"schema":1,"kind":"concat-live-skill","name":NAME,
            "version":"0.1.0","tag":"v0.1.0","minAppVersion":"0.3.0",
            "archive":{"file":format!("{NAME}-0.1.0.zip"),
                "url":format!("https://github.com/{REPO}/releases/download/v0.1.0/{NAME}-0.1.0.zip"),
                "bytes":128,"sha256":"a".repeat(64)}})
    }

    #[test]
    fn manifests_are_bound_to_our_skill_repository_and_stable_tag() {
        let valid = manifest_value();
        let release = manifest_from_json(&valid.to_string()).expect("valid release");
        assert_eq!(release.version, Version::new(0, 1, 0));
        assert_eq!(release.minimum_app, Version::new(0, 3, 0));
        for (pointer, value) in [
            ("/name", serde_json::json!("other-skill")),
            ("/tag", serde_json::json!("v0.2.0")),
            ("/version", serde_json::json!("0.1.0-beta.1")),
            ("/archive/sha256", serde_json::json!("short")),
            ("/archive/bytes", serde_json::json!(MAX_ARCHIVE_BYTES + 1)),
            (
                "/archive/url",
                serde_json::json!(
                    "https://github.com/jub0t/Concat/releases/download/v0.1.0/evil.zip"
                ),
            ),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).expect("field") = value;
            assert!(
                manifest_from_json(&invalid.to_string()).is_err(),
                "{pointer}"
            );
        }
    }

    fn zip(name: &str, contents: &[u8], mode: u32) -> Vec<u8> {
        fn u16(data: &mut Vec<u8>, value: u16) {
            data.extend(value.to_le_bytes());
        }
        fn u32(data: &mut Vec<u8>, value: u32) {
            data.extend(value.to_le_bytes());
        }
        let mut crc = !0u32;
        for byte in contents {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320u32 & 0u32.wrapping_sub(crc & 1));
            }
        }
        let crc = !crc;
        let mut data = Vec::new();
        u32(&mut data, 0x04034b50);
        u16(&mut data, 20);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u16(&mut data, 0x21);
        u32(&mut data, crc);
        u32(&mut data, contents.len() as u32);
        u32(&mut data, contents.len() as u32);
        u16(&mut data, name.len() as u16);
        u16(&mut data, 0);
        data.extend(name.as_bytes());
        data.extend(contents);
        let central = data.len() as u32;
        u32(&mut data, 0x02014b50);
        u16(&mut data, 0x0314);
        u16(&mut data, 20);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u16(&mut data, 0x21);
        u32(&mut data, crc);
        u32(&mut data, contents.len() as u32);
        u32(&mut data, contents.len() as u32);
        u16(&mut data, name.len() as u16);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u32(&mut data, mode << 16);
        u32(&mut data, 0);
        data.extend(name.as_bytes());
        let central_bytes = data.len() as u32 - central;
        u32(&mut data, 0x06054b50);
        u16(&mut data, 0);
        u16(&mut data, 0);
        u16(&mut data, 1);
        u16(&mut data, 1);
        u32(&mut data, central_bytes);
        u32(&mut data, central);
        u16(&mut data, 0);
        data
    }

    #[test]
    fn zip_inventory_rejects_traversal_symlinks_and_header_mismatches() {
        let valid = zip(&format!("{NAME}/SKILL.md"), DOCUMENT.as_bytes(), 0o100644);
        let entries = audit_archive(&valid).expect("safe ZIP");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "SKILL.md");
        assert_eq!(entries[0].bytes, DOCUMENT.len() as u64);
        for name in [
            format!("{NAME}/../escape"),
            "/tmp/escape".into(),
            format!("{NAME}/C:escape"),
            format!("{NAME}/x\\escape"),
            "other/SKILL.md".into(),
        ] {
            assert!(
                audit_archive(&zip(&name, b"x", 0o100644)).is_err(),
                "{name}"
            );
        }
        assert!(audit_archive(&zip(&format!("{NAME}/SKILL.md"), b"target", 0o120777)).is_err());
        let mut mismatch = valid.clone();
        mismatch[30] = b'X';
        assert!(audit_archive(&mismatch).is_err());
        let mut encrypted = valid.clone();
        encrypted[6] = 1;
        assert!(audit_archive(&encrypted).is_err());
        assert!(audit_archive(&valid[..valid.len() - 1]).is_err());
    }

    #[test]
    fn malformed_or_wrong_skill_documents_are_refused_without_panics() {
        let directory = tempfile::tempdir().expect("temporary document");
        let path = directory.path().join("SKILL.md");
        for text in [
            "---",
            "---\n",
            "---\nname: other\ndescription: edit\n---\nBody",
            "---\nname: steel0rat-video-maker-skill\ndescription: edit\n",
        ] {
            if path.exists() {
                let _ = std::fs::read(&path).expect("read prior test document");
            }
            std::fs::write(&path, text).expect("test document");
            assert!(validate_skill_document(&path).is_err());
        }
        let _ = std::fs::read(&path).expect("read prior test document");
        std::fs::write(&path, DOCUMENT).expect("valid document");
        validate_skill_document(&path).expect("our document");
    }

    #[cfg(target_os = "macos")]
    fn prepared(dirs: &AppDirs, version: Version) -> InstalledSkill {
        let sha = "a".repeat(64);
        let tree = release_path(dirs, version, &sha);
        std::fs::create_dir_all(&tree).expect("versioned skill");
        std::fs::write(tree.join("SKILL.md"), DOCUMENT).expect("skill document");
        InstalledSkill {
            version,
            path: tree.join("SKILL.md"),
            archive_sha256: sha,
            tree_sha256: tree_digest(&tree).expect("tree digest"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn activation_preserves_the_previous_source_link_and_its_target() {
        let temp = tempfile::tempdir().expect("temporary skill installation");
        let dirs = AppDirs::under(&temp.path().join("data"));
        let agents = temp.path().join("agents");
        let source = temp.path().join("source-repository");
        std::fs::create_dir_all(&agents).expect("user skills");
        std::fs::create_dir(&source).expect("source repository");
        std::fs::write(source.join("unchanged"), b"local edits").expect("local edits");
        create_link(&source, &agents.join(NAME)).expect("source link");
        let release = prepared(&dirs, Version::new(0, 1, 0));
        activate_at(&dirs, &release, &agents).expect("activate release");
        assert_eq!(
            agents.join(NAME).canonicalize().expect("active skill"),
            release
                .path
                .parent()
                .expect("release folder")
                .canonicalize()
                .expect("release")
        );
        assert_eq!(
            std::fs::read(source.join("unchanged")).expect("source remains"),
            b"local edits"
        );
        let backups: Vec<_> = std::fs::read_dir(root(&dirs).join("backups"))
            .expect("backups")
            .map(|e| e.expect("backup").path().join(NAME))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_link(&backups[0]).expect("preserved symbolic link"),
            source
        );
        let active = current(&dirs)
            .expect("active record")
            .expect("active release");
        assert_eq!(active.version, release.version);
        assert_eq!(active.path, release.path);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn activation_preserves_an_installed_directory_too() {
        let temp = tempfile::tempdir().expect("temporary skill installation");
        let dirs = AppDirs::under(&temp.path().join("data"));
        let agents = temp.path().join("agents");
        std::fs::create_dir_all(agents.join(NAME)).expect("old installed directory");
        std::fs::write(agents.join(NAME).join("keep"), b"old version").expect("old file");
        let release = prepared(&dirs, Version::new(0, 1, 0));
        activate_at(&dirs, &release, &agents).expect("atomically replace directory with link");
        let previous = std::fs::read_dir(root(&dirs).join("backups"))
            .expect("backups")
            .next()
            .expect("backup")
            .expect("entry")
            .path()
            .join(NAME);
        assert_eq!(
            std::fs::read(previous.join("keep")).expect("old directory"),
            b"old version"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_failed_state_commit_restores_the_previous_skill_and_record() {
        let temp = tempfile::tempdir().expect("temporary skill installation");
        let dirs = AppDirs::under(&temp.path().join("data"));
        let agents = temp.path().join("agents");
        let first = prepared(&dirs, Version::new(0, 1, 0));
        activate_at(&dirs, &first, &agents).expect("first release");
        let before = std::fs::read(root(&dirs).join("current.json")).expect("first record");
        let second = prepared(&dirs, Version::new(0, 1, 1));
        let error = activate_with(&dirs, &second, &agents, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "test state failure",
            ))
        })
        .expect_err("rollback");
        assert!(error.contains("previous skill installation restored"));
        assert_eq!(
            std::fs::read(root(&dirs).join("current.json")).expect("restored record"),
            before
        );
        assert_eq!(
            agents
                .join(NAME)
                .canonicalize()
                .expect("active first release"),
            first
                .path
                .parent()
                .expect("first folder")
                .canonicalize()
                .expect("first release")
        );
        assert!(
            second.path.is_file(),
            "the prepared new release is preserved"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn changed_release_files_never_replace_an_existing_installation() {
        let temp = tempfile::tempdir().expect("temporary skill installation");
        let dirs = AppDirs::under(&temp.path().join("data"));
        let agents = temp.path().join("agents");
        std::fs::create_dir_all(agents.join(NAME)).expect("existing skill");
        std::fs::write(agents.join(NAME).join("keep"), b"source").expect("source");
        let release = prepared(&dirs, Version::new(0, 1, 0));
        let _ = std::fs::read(&release.path).expect("read before editing test fixture");
        std::fs::write(&release.path, DOCUMENT.to_owned() + "changed").expect("changed fixture");
        assert!(activate_at(&dirs, &release, &agents).is_err());
        assert_eq!(
            std::fs::read(agents.join(NAME).join("keep")).expect("original installation"),
            b"source"
        );
        assert!(current(&dirs).expect("no record").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_verified_local_archive_prepares_without_changing_user_skills() {
        let temp = tempfile::tempdir().expect("temporary release cache");
        let dirs = AppDirs::under(&temp.path().join("data"));
        let archive = zip(&format!("{NAME}/SKILL.md"), DOCUMENT.as_bytes(), 0o100644);
        let file = temp.path().join("fixture.zip");
        std::fs::write(&file, &archive).expect("archive");
        let digest = super::super::file_digest(&file).expect("digest");
        let mut value = manifest_value();
        value["archive"]["bytes"] = serde_json::json!(archive.len());
        value["archive"]["sha256"] = serde_json::json!(digest);
        let manifest = manifest_from_json(&value.to_string()).expect("manifest");
        let cached = root(&dirs).join("downloads").join(&manifest.archive.sha256);
        std::fs::create_dir_all(&cached).expect("cache");
        std::fs::write(cached.join(&manifest.archive.file), &archive)
            .expect("verified cached download");
        let ready = prepare(&manifest, &dirs, &AtomicBool::new(false), &mut |_, _| {})
            .expect("prepare local release");
        assert_eq!(
            std::fs::read_to_string(&ready.path).expect("prepared skill"),
            DOCUMENT
        );
        assert!(current(&dirs).expect("inactive").is_none());
        assert!(
            !temp.path().join("agents").exists(),
            "preparing a release must not activate it"
        );
    }
}
