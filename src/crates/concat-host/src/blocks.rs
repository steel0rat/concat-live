// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Portable montage blocks. The native document carries the edit and the
//! manifest pins precisely the files and packages required to play it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use concat_effects::Catalogue;
use concat_project::{
    Project,
    commands::blocks::{self, Block},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const VERSION: u32 = 1;
const MANIFEST: &str = "block.json";
const ASSETS: &str = "assets";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// A saved block as the library presents it.
#[derive(Clone, Debug)]
pub struct Card {
    /// The ID of its managed bundle.
    pub id: String,
    /// The user-assigned name.
    pub name: String,
    /// The number of native clips this placement will add.
    pub clips: usize,
    /// The span of the selected clips, including gaps.
    pub seconds: f64,
}

/// Why a library operation could not be completed.
#[derive(Debug)]
pub enum Error {
    /// The library needs a name for this block.
    EmptyName,
    /// The bundle format or its asset references are invalid.
    InvalidBundle,
    /// A filesystem operation failed.
    File(String),
    /// An asset which the block needs is missing or was changed.
    DamagedAsset(String),
    /// The catalogue does not have a dependency of this block.
    MissingEffect(String),
    /// The installed package could render a different result.
    DifferentEffect {
        /// The catalogue ID.
        id: String,
        /// The version used when the block was saved.
        expected: u32,
        /// The version available now.
        installed: u32,
    },
    /// The native document or its placement is invalid.
    Model(blocks::Error),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: u32,
    name: String,
    document: Value,
    assets: BTreeMap<String, String>,
    effects: BTreeMap<String, u32>,
}

/// Files copied for a project edit stay owned by this value until the
/// editor accepts the whole block. Refusing the edit drops this copy.
pub struct PreparedBlock {
    block: Block,
    files: Option<NewDirectory>,
}

impl PreparedBlock {
    /// The native snapshot with the destination’s local file paths.
    pub fn block(&self) -> &Block {
        &self.block
    }

    /// Keep the files after a successful edit so Redo can reuse them.
    pub fn keep(mut self) {
        if let Some(files) = self.files.take() {
            files.keep();
        }
    }
}

/// Populate the library from fully published bundles.
pub fn list(config: &Path) -> Vec<Card> {
    let Ok(entries) = fs::read_dir(library(config)) else {
        return Vec::new();
    };
    let mut cards: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = entry.file_name().into_string().ok()?;
            let (manifest, block) = read(config, &id).ok()?;
            Some(card(id, manifest.name, &block))
        })
        .collect();
    cards.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    cards
}

/// Copy only the files of this fragment and publish its manifest last.
pub fn save(
    config: &Path,
    source_project: &Path,
    name: &str,
    mut block: Block,
    catalogue: &Catalogue,
) -> Result<Card, Error> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::EmptyName);
    }
    let effects = effect_versions(&block, catalogue)?;
    for path in block.files_mut() {
        let resolved = source_project.join(path.as_str());
        if !resolved.is_file() {
            return Err(Error::DamagedAsset(file_name(path)));
        }
        *path = resolved.to_str().ok_or(Error::InvalidBundle)?.to_owned();
    }
    let folder = NewDirectory::create(&library(config))?;
    let assets_dir = folder.path.join(ASSETS);
    fs::create_dir(&assets_dir).map_err(|error| file_error(&assets_dir, error))?;
    let mut paths = HashMap::<String, String>::new();
    let mut assets = BTreeMap::new();
    for (index, source) in block.files_mut().enumerate() {
        if let Some(relative) = paths.get(source) {
            source.clone_from(relative);
            continue;
        }
        let relative = crate::templates::bundle_file(&assets_dir, &format!("asset{index}"), source)
            .map_err(Error::File)?;
        assets.insert(relative.clone(), digest(&folder.path.join(&relative))?);
        paths.insert(source.clone(), relative.clone());
        *source = relative;
    }
    let manifest = Manifest {
        version: VERSION,
        name: name.to_owned(),
        document: block.document(name),
        assets,
        effects,
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| Error::InvalidBundle)?;
    let pending = folder.path.join(".block.json.new");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)
        .map_err(|error| file_error(&pending, error))?;
    output
        .write_all(&bytes)
        .and_then(|()| output.sync_all())
        .map_err(|error| file_error(&pending, error))?;
    drop(output);
    fs::rename(&pending, folder.path.join(MANIFEST))
        .map_err(|error| file_error(&pending, error))?;
    let result = card(folder.id.clone(), name.to_owned(), &block);
    folder.keep();
    Ok(result)
}

/// Check the saved dependencies and copy the assets into the destination project.
pub fn prepare(
    config: &Path,
    id: &str,
    destination: &Path,
    project: &Project,
    catalogue: &Catalogue,
) -> Result<PreparedBlock, Error> {
    let (manifest, mut block) = read(config, id)?;
    let installed = effect_versions(&block, catalogue)?;
    if installed.keys().ne(manifest.effects.keys()) {
        return Err(Error::InvalidBundle);
    }
    for (effect, expected) in &manifest.effects {
        let observed = installed[effect];
        if observed != *expected {
            return Err(Error::DifferentEffect {
                id: effect.clone(),
                expected: *expected,
                installed: observed,
            });
        }
    }

    let root = library(config).join(id);
    for (relative, expected) in &manifest.assets {
        let original = asset_path(&root, relative)?;
        if digest(&original)? != *expected {
            return Err(Error::DamagedAsset(file_name(relative)));
        }
    }
    let mut copies = existing_fonts(&block, &manifest.assets, project)?;

    let references: BTreeSet<_> = block.files_mut().map(|ref_path| ref_path.clone()).collect();
    let outstanding = references
        .iter()
        .any(|relative| !copies.contains_key(relative));
    let files = outstanding
        .then(|| NewDirectory::create(&destination.join(ASSETS).join("blocks")))
        .transpose()?;
    if let Some(folder) = &files {
        for relative in references {
            if copies.contains_key(&relative) {
                continue;
            }
            let original = asset_path(&root, &relative)?;
            let copy = folder
                .path
                .join(original.file_name().ok_or(Error::InvalidBundle)?);
            fs::copy(&original, &copy).map_err(|error| file_error(&copy, error))?;
            if digest(&copy)? != manifest.assets[&relative] {
                return Err(Error::DamagedAsset(file_name(&relative)));
            }
            copies.insert(
                relative,
                copy.to_str().ok_or(Error::InvalidBundle)?.to_owned(),
            );
        }
    }
    for path in block.files_mut() {
        path.clone_from(copies.get(path).ok_or(Error::InvalidBundle)?);
    }
    Ok(PreparedBlock { block, files })
}

fn existing_fonts(
    block: &Block,
    assets: &BTreeMap<String, String>,
    project: &Project,
) -> Result<HashMap<String, String>, Error> {
    let mut found = HashMap::new();
    for font in &project.fonts {
        let mut saved = block
            .project()
            .fonts
            .iter()
            .filter(|other| other.family == font.family)
            .peekable();
        if saved.peek().is_none() {
            continue;
        }
        let fingerprint = digest(Path::new(&font.path))?;
        let matching = saved
            .find(|other| assets[&other.path] == fingerprint)
            .ok_or_else(|| Error::Model(blocks::Error::FontConflict(font.family.clone())))?;
        if found
            .insert(matching.path.clone(), font.path.clone())
            .is_some()
        {
            return Err(Error::Model(blocks::Error::FontConflict(
                font.family.clone(),
            )));
        }
    }
    Ok(found)
}

fn read(config: &Path, id: &str) -> Result<(Manifest, Block), Error> {
    if !id.strip_prefix("block-").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
    }) {
        return Err(Error::InvalidBundle);
    }
    let root = library(config).join(id);
    if !fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Err(Error::InvalidBundle);
    }
    let path = root.join(MANIFEST);
    if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return Err(Error::InvalidBundle);
    }
    let bytes = fs::read(&path).map_err(|error| file_error(&path, error))?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidBundle)?;
    if manifest.version != VERSION || manifest.name.trim().is_empty() {
        return Err(Error::InvalidBundle);
    }
    let mut block = Block::from_document(&manifest.document).map_err(Error::Model)?;
    let references: BTreeSet<_> = block.files_mut().map(|path| path.clone()).collect();
    if references.iter().ne(manifest.assets.keys()) {
        return Err(Error::InvalidBundle);
    }
    Ok((manifest, block))
}

fn effect_versions(block: &Block, catalogue: &Catalogue) -> Result<BTreeMap<String, u32>, Error> {
    block
        .effects()
        .into_iter()
        .map(|id| {
            let version = catalogue
                .get(id)
                .ok_or_else(|| Error::MissingEffect(id.to_owned()))?
                .manifest
                .effect
                .version;
            Ok((id.to_owned(), version))
        })
        .collect()
}

fn asset_path(root: &Path, relative: &str) -> Result<PathBuf, Error> {
    let mut parts = Path::new(relative).components();
    let (Some(Component::Normal(folder)), Some(Component::Normal(file)), None) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(Error::InvalidBundle);
    };
    if folder != ASSETS {
        return Err(Error::InvalidBundle);
    }
    let assets = root.join(folder);
    let path = assets.join(file);
    if !fs::symlink_metadata(&assets).is_ok_and(|metadata| metadata.file_type().is_dir())
        || !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return Err(Error::DamagedAsset(file_name(relative)));
    }
    Ok(path)
}

fn card(id: String, name: String, block: &Block) -> Card {
    let timeline = block.project().active();
    Card {
        id,
        name,
        clips: timeline.clips.len(),
        seconds: timeline.duration(),
    }
}

fn library(config: &Path) -> PathBuf {
    config.join("montage-blocks")
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_error(path: &Path, error: io::Error) -> Error {
    Error::File(format!("{}: {error}", path.display()))
}

fn digest(path: &Path) -> Result<String, Error> {
    let mut file = File::open(path).map_err(|error| file_error(path, error))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| file_error(path, error))?;
        if count == 0 {
            return Ok(format!("{:x}", digest.finalize()));
        }
        digest.update(&buffer[..count]);
    }
}

struct NewDirectory {
    path: PathBuf,
    id: String,
    kept: bool,
}

impl NewDirectory {
    fn create(parent: &Path) -> Result<Self, Error> {
        fs::create_dir_all(parent).map_err(|error| file_error(parent, error))?;
        let process = std::process::id();
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let id = format!("block-{time}-{process}-{sequence}");
            let path = parent.join(&id);
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        id,
                        kept: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(file_error(&path, error)),
            }
        }
    }

    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for NewDirectory {
    fn drop(&mut self) {
        if !self.kept {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use concat_project::{
        Command, Editor,
        model::{
            AppliedFilter, Clip, ClipKey, ClipKind, KeyEase, KeyProperty, MediaItem, MediaKind,
        },
    };

    use super::*;

    fn source(directory: &Path) -> Project {
        fs::create_dir(directory).unwrap();
        fs::write(directory.join("take.mov"), b"material to preserve").unwrap();
        fs::write(
            directory.join("outside.wav"),
            b"material outside the selection",
        )
        .unwrap();

        let mut project = Project::new();
        project.media.extend([
            MediaItem {
                id: "m1".into(),
                path: "take.mov".into(),
                name: "Take".into(),
                kind: MediaKind::Video,
                ..Default::default()
            },
            MediaItem {
                id: "m2".into(),
                path: "outside.wav".into(),
                name: "Excluded".into(),
                kind: MediaKind::Audio,
                ..Default::default()
            },
        ]);
        let mut clip = Clip::blank("c3", "T2", ClipKind::Video, "Animated take", 9.0, 2.5);
        clip.media_id = "m1".into();
        clip.source_start = 1.25;
        clip.keys.push(ClipKey {
            property: KeyProperty::Scale,
            at: 0.25,
            value: 1.75,
            ease: KeyEase::LINEAR,
        });
        project.active_mut().clips.push(Arc::new(clip));
        project
    }

    #[test]
    fn file_survives_moved_source_and_one_undo_removes_the_block() {
        let workspace = tempfile::tempdir().unwrap();
        let source_dir = workspace.path().join("source");
        let project = source(&source_dir);
        let selection = ["c3".to_owned()];
        let block = Block::capture(&project, &selection).unwrap();
        let config = workspace.path().join("config");
        let catalogue = Catalogue::new();
        let saved = save(&config, &source_dir, "Clip block", block, &catalogue).unwrap();
        let bundle = library(&config).join(&saved.id);
        assert_eq!(fs::read_dir(bundle.join(ASSETS)).unwrap().count(), 1);
        assert_eq!(list(&config).len(), 1);
        fs::rename(&source_dir, workspace.path().join("relocated")).unwrap();

        let destination = workspace.path().join("destination");
        let mut editor = Editor::new();
        let before = editor.project().clone();
        let prepared = prepare(&config, &saved.id, &destination, &before, &catalogue).unwrap();
        let edit = editor
            .apply(Command::InsertBlock {
                block: Box::new(prepared.block().clone()),
                start: 3.5,
            })
            .unwrap();
        prepared.keep();
        assert!(edit.applied);
        assert_eq!(
            editor.project().active().tracks.len(),
            before.active().tracks.len() + 1
        );
        let inserted = editor
            .project()
            .active()
            .clip(&edit.created_id.unwrap())
            .unwrap();
        let original = project.active().clip("c3").unwrap();
        assert_eq!(inserted.start, 3.5);
        assert_eq!(inserted.duration, original.duration);
        assert_eq!(inserted.source_start, original.source_start);
        assert_eq!(inserted.keys, original.keys);
        let file = PathBuf::from(
            &editor
                .project()
                .media_by_id(&inserted.media_id)
                .unwrap()
                .path,
        );
        assert!(file.starts_with(&destination));
        assert_eq!(fs::read(&file).unwrap(), b"material to preserve");

        let after = editor.project().clone();
        editor.undo();
        assert_eq!(*editor.project(), before);
        assert!(!editor.can_undo());
        editor.redo();
        assert_eq!(*editor.project(), after);
        assert!(file.is_file());
    }

    #[test]
    fn missing_effect_and_modified_asset_are_refused_before_destination_writes() {
        let workspace = tempfile::tempdir().unwrap();
        let source_dir = workspace.path().join("source");
        let mut project = source(&source_dir);
        let selection = ["c3".to_owned()];
        let config = workspace.path().join("config");
        let catalogue = Catalogue::new();
        project
            .active_mut()
            .clip_mut("c3")
            .unwrap()
            .video_effects
            .push(AppliedFilter::new("example.uninstalled"));
        let block = Block::capture(&project, &selection).unwrap();
        assert!(
            matches!(save(&config, &source_dir, "Needs effect", block, &catalogue), Err(Error::MissingEffect(id)) if id == "example.uninstalled")
        );
        assert!(!config.exists());

        project
            .active_mut()
            .clip_mut("c3")
            .unwrap()
            .video_effects
            .clear();
        let block = Block::capture(&project, &selection).unwrap();
        let saved = save(&config, &source_dir, "Clip block", block, &catalogue).unwrap();
        let assets = library(&config).join(&saved.id).join(ASSETS);
        let stored = fs::read_dir(&assets)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(&stored, b"changed bytes").unwrap();
        let destination = workspace.path().join("destination");
        assert!(matches!(
            prepare(
                &config,
                &saved.id,
                &destination,
                &Project::new(),
                &catalogue
            ),
            Err(Error::DamagedAsset(_))
        ));
        assert!(!destination.exists());
    }
}
