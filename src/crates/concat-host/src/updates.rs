// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Which release this is, which releases are out, and putting one of them
//! in this one's place.
//!
//! Every release on GitHub carries a `manifest.json` naming each
//! platform's package with its URL, its size and its SHA-256 - the release
//! workflow writes it. That is what a switch reads: the list of releases
//! comes from GitHub's API, the package for this machine from the chosen
//! release's manifest, and the bytes are checked against the manifest's
//! digest before they are put anywhere, the road the models take (see
//! [`crate::models`]).
//!
//! Only releases from [`FIRST_SWITCHABLE`] on are offered. A release before
//! it has no page to come back up by, so a person who went there would be
//! stranded.
//!
//! How a package goes in place is the platform's affair. Windows runs the
//! installer, which replaces the files once the app has closed. macOS
//! copies the app out of the disk image over the bundle this is running
//! from. An AppImage is written over the file it was started from. A .deb,
//! an .rpm or a pacman package is handed to the desktop's installer. The
//! caller closes the app where that is needed and [`relaunch`] starts the
//! new one.

#[path = "updates/skill.rs"]
pub mod skill;

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(unix)]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::dirs::AppDirs;
use crate::models;

/// The repository the releases are read from.
pub const REPO: &str = "steel0rat/concat-live";

/// The first release with a Version page: the earliest one a person can
/// come back from, so the earliest one offered.
pub const FIRST_SWITCHABLE: Version = Version::new(0, 3, 0);

/// GitHub's list of the repository's releases, newest first. A hundred
/// covers years of releases at this pace.
const RELEASES_URL: &str =
    "https://api.github.com/repos/steel0rat/concat-live/releases?per_page=100";

/// What a download that was stopped says.
pub const CANCELLED: &str = "the download was cancelled";

/// A release's number: `0.2.5`. A tag carries a `v` in front of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    /// The first number.
    pub major: u64,
    /// The second.
    pub minor: u64,
    /// The third.
    pub patch: u64,
}

impl Version {
    /// The version with these three numbers.
    pub const fn new(major: u64, minor: u64, patch: u64) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    /// `0.2.5` or `v0.2.5`. Anything else - a pre-release such as
    /// `v0.3.0-beta.1`, the `nightly` tag, two numbers - is `None`: not a
    /// release a person is offered.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version::new(major, minor, patch))
    }

    /// The version this binary was built as: the workspace's.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version::new(0, 0, 0))
    }

    /// The release's tag: `v0.2.5`.
    pub fn tag(self) -> String {
        format!("v{self}")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One release a person can switch to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Its number.
    pub version: Version,
    /// Its tag on GitHub, `v0.2.5`, which names its download folder.
    pub tag: String,
    /// The day it was published, `2026-09-24`; empty when GitHub did not
    /// say.
    pub published: String,
}

/// Reads GitHub's answer to the releases request: the stable releases from
/// [`FIRST_SWITCHABLE`] on, newest first. Drafts and pre-releases are left
/// out, as is anything whose tag is not a plain version.
pub fn releases_from_json(json: &str) -> Result<Vec<Release>, String> {
    releases_from_json_min(json, FIRST_SWITCHABLE)
}

fn releases_from_json_min(json: &str, minimum: Version) -> Result<Vec<Release>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("the release list does not read as JSON: {error}"))?;
    let Some(entries) = value.as_array() else {
        // GitHub answers an object with a message when it refuses: the
        // rate limit, for the most part.
        return Err(format!("GitHub answered: {}", github_message(&value)));
    };
    let mut releases: Vec<Release> = entries
        .iter()
        .filter_map(|entry| {
            let flag = |name: &str| {
                entry
                    .get(name)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            };
            if flag("draft") || flag("prerelease") {
                return None;
            }
            let tag = entry.get("tag_name")?.as_str()?;
            let version = Version::parse(tag)?;
            if version < minimum {
                return None;
            }
            let published = entry
                .get("published_at")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(10)
                .collect();
            Some(Release {
                version,
                tag: tag.to_owned(),
                published,
            })
        })
        .collect();
    releases.sort_by(|a, b| b.version.cmp(&a.version));
    releases.dedup_by(|a, b| a.version == b.version);
    Ok(releases)
}

/// The `message` of a GitHub error body, or the body's first line.
fn github_message(value: &serde_json::Value) -> String {
    value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || value.to_string().chars().take(200).collect(),
            str::to_owned,
        )
}

/// Asks GitHub for the releases; see [`releases_from_json`].
pub fn fetch_releases() -> Result<Vec<Release>, String> {
    releases_from_json(&get_text(RELEASES_URL)?)
}

/// Where this build sits among the releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// This build is the newest release.
    Latest,
    /// A newer release is out: this one.
    Behind(Version),
    /// This build is newer than any release: a build from the tree.
    Ahead,
    /// No release from [`FIRST_SWITCHABLE`] on is out.
    NoReleases,
}

/// Where `current` sits among `releases`.
pub fn standing(current: Version, releases: &[Release]) -> Standing {
    match releases.iter().map(|release| release.version).max() {
        None => Standing::NoReleases,
        Some(newest) if newest > current => Standing::Behind(newest),
        Some(newest) if newest < current => Standing::Ahead,
        Some(_) => Standing::Latest,
    }
}

/// The kind of package this build was installed from, which is the kind
/// that goes in its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageKind {
    /// A macOS disk image with the app in it.
    Dmg,
    /// The Windows installer, `Concat-<version>-windows-<arch>-setup.exe`.
    Setup,
    /// The Windows Installer package, `Concat-<version>-windows-<arch>.msi`:
    /// the per-machine install an administrator deploys.
    Msi,
    /// A Linux AppImage: one file, started where it lies.
    AppImage,
    /// A Debian package.
    Deb,
    /// An RPM.
    Rpm,
    /// An Arch package.
    Pacman,
}

impl PackageKind {
    /// The key the release manifest files the package under.
    pub fn key(self) -> &'static str {
        match self {
            PackageKind::Dmg => "dmg",
            PackageKind::Setup => "setup",
            PackageKind::Msi => "msi",
            PackageKind::AppImage => "appimage",
            PackageKind::Deb => "deb",
            PackageKind::Rpm => "rpm",
            PackageKind::Pacman => "pacman",
        }
    }
}

/// Why this build cannot be replaced from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixed {
    /// A Flatpak: Flatpak keeps it up to date.
    Flatpak,
    /// A phone: its app store keeps it up to date.
    Store,
    /// Not installed from any package this knows - a build from the tree,
    /// a distribution's own package.
    NotFromPackage,
}

/// How this build was installed, or why it cannot be switched from here.
/// Worked out once: on Linux it reads the package databases.
pub fn installed_as() -> Result<PackageKind, Fixed> {
    static KNOWN: OnceLock<Result<PackageKind, Fixed>> = OnceLock::new();
    *KNOWN.get_or_init(|| {
        if cfg!(target_os = "macos") {
            own_bundle()
                .map(|_| PackageKind::Dmg)
                .ok_or(Fixed::NotFromPackage)
        } else if cfg!(target_os = "windows") {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf));
            windows_kind(
                dir.as_deref()
                    .is_some_and(|dir| dir.join("unins000.exe").is_file()),
                dir.as_deref().is_some_and(in_program_files),
            )
        } else if cfg!(target_os = "linux") {
            linux_kind(
                std::env::var_os("APPIMAGE").is_some(),
                std::env::var_os("FLATPAK_ID").is_some(),
                Path::new("/var/lib/dpkg/info/concat.list").exists(),
                pacman_has_concat(),
                rpm_has_concat(),
            )
        } else {
            Err(Fixed::Store)
        }
    })
}

/// A Windows install, from where the executable is and what sits beside
/// it. The setup leaves its
/// uninstaller beside the program; the .msi leaves none and always
/// installs under Program Files. Anything else - the portable zip, a build
/// from the tree - came from no installer, and an update from here would
/// put a second copy beside it rather than replace it.
fn windows_kind(uninstaller: bool, program_files: bool) -> Result<PackageKind, Fixed> {
    if uninstaller {
        Ok(PackageKind::Setup)
    } else if program_files {
        Ok(PackageKind::Msi)
    } else {
        Err(Fixed::NotFromPackage)
    }
}

/// Whether `dir` is under one of Windows' Program Files folders.
fn in_program_files(dir: &Path) -> bool {
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .any(|root| dir.starts_with(root))
}

/// A Linux install, from what is on the machine: an AppImage says so in
/// the environment, and a package is in its manager's database.
fn linux_kind(
    appimage: bool,
    flatpak: bool,
    dpkg: bool,
    pacman: bool,
    rpm: bool,
) -> Result<PackageKind, Fixed> {
    if flatpak {
        Err(Fixed::Flatpak)
    } else if appimage {
        Ok(PackageKind::AppImage)
    } else if dpkg {
        Ok(PackageKind::Deb)
    } else if pacman {
        Ok(PackageKind::Pacman)
    } else if rpm {
        Ok(PackageKind::Rpm)
    } else {
        Err(Fixed::NotFromPackage)
    }
}

/// Whether pacman's database lists the `concat` package: a folder named
/// `concat-<version>-<release>` under its local store.
fn pacman_has_concat() -> bool {
    std::fs::read_dir("/var/lib/pacman/local").is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix("concat-")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
    })
}

/// Whether the RPM database lists `concat`. Asked of `rpm` itself, and only
/// where there is a database to ask about.
fn rpm_has_concat() -> bool {
    Path::new("/var/lib/rpm").exists()
        && Command::new("rpm")
            .args(["-q", "concat"])
            .output()
            .is_ok_and(|output| output.status.success())
}

/// One platform's package in a release: what to fetch and what it must
/// hash to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    /// Its file name, `Concat-0.2.5-macos-arm64.dmg`.
    pub file: String,
    /// Where to fetch it.
    pub url: String,
    /// Its size, for the progress bar before the server has said.
    pub bytes: u64,
    /// Its SHA-256 as lowercase hex.
    pub sha256: String,
}

/// The name the manifest gives this machine's architecture: the builds
/// say `arm64` for a Mac and `aarch64` everywhere else.
pub fn manifest_arch(os: &str, arch: &str) -> String {
    match (os, arch) {
        ("macos", "aarch64") => "arm64".to_owned(),
        _ => arch.to_owned(),
    }
}

/// The package of `kind` for `os` on `arch` in a release's manifest.
pub fn package_from_manifest(
    json: &str,
    os: &str,
    arch: &str,
    kind: PackageKind,
) -> Result<Package, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("the release manifest does not read as JSON: {error}"))?;
    let entry = value
        .get("binaries")
        .and_then(|binaries| binaries.get(os))
        .and_then(|platform| platform.get(manifest_arch(os, arch)))
        .and_then(|machine| machine.get(kind.key()))
        .ok_or_else(|| format!("the release has no {} package for {os} {arch}", kind.key()))?;
    let text = |name: &str| {
        entry
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("the manifest's package has no {name}"))
    };
    let file = text("file")?;
    // The name goes into a path under the app's own folder, and comes off
    // the network: a name with a folder in it is refused, not honoured.
    if file.is_empty() || file.starts_with('.') || file.contains(['/', '\\']) {
        return Err(format!(
            "the manifest names the package {file:?}, which is not a file name"
        ));
    }
    Ok(Package {
        file,
        url: text("url")?,
        bytes: entry
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        sha256: text("sha256")?,
    })
}

/// Where a release's manifest is.
pub fn manifest_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/manifest.json")
}

/// The package of `kind` for this machine in `release`, from its manifest.
pub fn fetch_package(release: &Release, kind: PackageKind) -> Result<Package, String> {
    let text = get_text(&manifest_url(&release.tag))?;
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("the release manifest does not read as JSON: {error}"))?;
    if manifest["schema"] != 2
        || manifest["product"] != "Concat Live"
        || manifest["bundleIdentifier"] != crate::dirs::IDENTIFIER
        || manifest["version"] != release.version.to_string()
        || manifest["tag"] != release.tag
        || release.tag != release.version.tag()
    {
        return Err("the release manifest is not for the requested Concat Live version".into());
    }
    let package = package_from_manifest(&text, std::env::consts::OS, std::env::consts::ARCH, kind)?;
    validate_package(&package, REPO, Some(&release.tag))?;
    Ok(package)
}

/// Where packages are downloaded to.
pub fn folder(dirs: &AppDirs) -> PathBuf {
    dirs.data.join("updates")
}

/// Fetches and verifies an immutable release package. Existing packages
/// and failed partials are preserved without changing an earlier download.
pub fn download_package(
    package: &Package,
    folder: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, String> {
    download_verified(package, REPO, folder, cancel, progress)
}

const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn valid_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_package(package: &Package, repo: &str, tag: Option<&str>) -> Result<(), String> {
    if package.file.is_empty()
        || package.file.starts_with('.')
        || !package
            .file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        || !valid_digest(&package.sha256)
        || package.bytes == 0
        || package.bytes > MAX_DOWNLOAD_BYTES
    {
        return Err("the release package has an invalid file name, size or SHA-256".into());
    }
    let prefix = format!("https://github.com/{repo}/releases/download/");
    let (actual_tag, actual_file) = package
        .url
        .strip_prefix(&prefix)
        .and_then(|tail| tail.split_once('/'))
        .ok_or("the release package URL is outside the expected GitHub repository")?;
    let version = Version::parse(actual_tag).ok_or("the package URL has no stable release tag")?;
    if actual_tag != version.tag()
        || actual_file != package.file
        || tag.is_some_and(|expected| expected != actual_tag)
    {
        return Err("the package URL does not match its release tag and file name".into());
    }
    Ok(())
}

fn download_verified(
    package: &Package,
    repo: &str,
    folder: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, String> {
    validate_package(package, repo, None)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    let cache = folder.join(&package.sha256);
    std::fs::create_dir_all(&cache)
        .map_err(|error| format!("could not create {}: {error}", cache.display()))?;
    let done = cache.join(&package.file);
    if done.exists() || done.is_symlink() {
        if done.is_symlink()
            || std::fs::metadata(&done)
                .map_err(|error| error.to_string())?
                .len()
                != package.bytes
        {
            return Err(format!("the cached package {} has changed", done.display()));
        }
        models::verify(&done, &package.sha256)?;
        progress(package.bytes, package.bytes);
        return Ok(done);
    }
    let partial = cache.join(format!("{}.{}.part", package.file, nonce()));
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(|error| format!("could not create {}: {error}", partial.display()))?;
    let response = crate::net::get(
        &package.url,
        Duration::from_secs(20),
        Duration::from_secs(30),
        crate::net::Trust::Shipped,
        &|request| request.set("User-Agent", &format!("Concat-Live/{}", Version::current())),
    )
    .map_err(|_| "the release package could not be downloaded".to_owned())?;
    if response
        .header("Content-Length")
        .and_then(|text| text.parse::<u64>().ok())
        .is_some_and(|length| length != package.bytes)
    {
        return Err("the download size differs from the release manifest".into());
    }
    let mut reader = response.into_reader().take(package.bytes + 1);
    let mut received = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    progress(0, package.bytes);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("the release download stopped: {error}"))?;
        if read == 0 {
            break;
        }
        received += read as u64;
        if received > package.bytes {
            return Err("the download exceeds the release manifest's size".into());
        }
        target
            .write_all(&buffer[..read])
            .map_err(|error| format!("could not write {}: {error}", partial.display()))?;
        progress(received, package.bytes);
    }
    target
        .sync_all()
        .map_err(|error| format!("could not flush {}: {error}", partial.display()))?;
    drop(target);
    if received != package.bytes {
        return Err("the release download ended before its declared size".into());
    }
    models::verify(&partial, &package.sha256)?;
    // A hard link refuses a concurrently published destination.
    if let Err(error) = std::fs::hard_link(&partial, &done) {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("could not publish {}: {error}", done.display()));
        }
        models::verify(&done, &package.sha256)?;
    }
    progress(received, package.bytes);
    Ok(done)
}

fn nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{time}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// What [`install`] did, which decides what the app does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The installer is running: the app closes so it can finish.
    InstallerRunning,
    /// The new build is in this one's place: the app closes and
    /// [`relaunch`] starts it.
    Replaced,
    /// The package is open in the desktop's installer: the person finishes
    /// there and starts the app again.
    Opened,
}

/// Puts `package`, a file of `kind` already checked against its digest, in
/// this build's place.
pub fn install(package: &Path, kind: PackageKind) -> Result<Installed, String> {
    match kind {
        PackageKind::Setup => {
            Command::new(package)
                .spawn()
                .map_err(|error| format!("could not start {}: {error}", package.display()))?;
            Ok(Installed::InstallerRunning)
        }
        // msiexec asks for the elevation a per-machine package needs, and
        // the package's MajorUpgrade takes the old version out.
        PackageKind::Msi => {
            Command::new("msiexec")
                .arg("/i")
                .arg(package)
                .spawn()
                .map_err(|error| format!("could not start msiexec: {error}"))?;
            Ok(Installed::InstallerRunning)
        }
        PackageKind::Dmg => replace_from_dmg(package),
        PackageKind::AppImage => replace_appimage(package),
        PackageKind::Deb | PackageKind::Rpm | PackageKind::Pacman => {
            run(Command::new("xdg-open").arg(package))?;
            Ok(Installed::Opened)
        }
    }
}

/// The `.app` this process runs from, or `None` for a bare binary.
fn own_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .map(Path::to_path_buf)
}

/// A verified application staged beside the installed macOS bundle.
#[derive(Clone, Debug)]
pub struct PreparedApp {
    staged: PathBuf,
    target: PathBuf,
    version: Version,
    original_digest: String,
    prepared_digest: String,
    work: PathBuf,
}

/// Verifies and stages a downloaded DMG without replacing the running app.
/// Its caller saves the visible project before committing the installation.
pub fn prepare_app(package: &Path, version: Version) -> Result<PreparedApp, String> {
    prepare_app_version(package, Some(version))
}

fn prepare_app_version(package: &Path, expected: Option<Version>) -> Result<PreparedApp, String> {
    if !cfg!(target_os = "macos") {
        return Err("automatic application installation currently supports macOS".into());
    }
    let target =
        own_bundle().ok_or("automatic updates require an installed Concat Live.app bundle")?;
    require_bundle_identifier(&target)?;
    let original_digest = file_digest(&target.join("Contents/MacOS/concat"))?;
    let parent = target
        .parent()
        .ok_or("the installed app has no parent folder")?;
    let work = parent.join(".concat-live-updates").join(nonce());
    std::fs::create_dir_all(&work)
        .map_err(|error| format!("could not create {}: {error}", work.display()))?;
    let mount = work.join("mount");
    run(Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(package))?;
    let result = (|| {
        let fresh = mount.join("Concat Live.app");
        let version = validate_bundle(&fresh, expected)?;
        let staged = work.join("Concat Live.app");
        run(Command::new("/usr/bin/ditto").arg(&fresh).arg(&staged))?;
        validate_bundle(&staged, Some(version))?;
        let prepared_digest = file_digest(&staged.join("Contents/MacOS/concat"))?;
        Ok(PreparedApp {
            staged,
            target,
            version,
            original_digest,
            prepared_digest,
            work,
        })
    })();
    let detached = run(Command::new("/usr/bin/hdiutil")
        .arg("detach")
        .arg(&mount)
        .arg("-quiet"));
    match (result, detached) {
        (Ok(prepared), Ok(())) => Ok(prepared),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
    }
}

/// Replaces the staged bundle and preserves the previous application.
/// An unsuccessful swap or post-install check restores the previous bundle.
pub fn install_prepared_app(prepared: &PreparedApp) -> Result<PathBuf, String> {
    require_bundle_identifier(&prepared.target)?;
    if file_digest(&prepared.target.join("Contents/MacOS/concat"))? != prepared.original_digest
        || file_digest(&prepared.staged.join("Contents/MacOS/concat"))? != prepared.prepared_digest
    {
        return Err("an application bundle changed after update preparation".into());
    }
    validate_bundle(&prepared.staged, Some(prepared.version))?;
    swap_app(&prepared.staged, &prepared.target, &prepared.work, || {
        validate_bundle(&prepared.target, Some(prepared.version)).map(|_| ())
    })
}

fn swap_app(
    staged: &Path,
    target: &Path,
    work: &Path,
    verify: impl FnOnce() -> Result<(), String>,
) -> Result<PathBuf, String> {
    let previous_folder = work.join("previous");
    std::fs::create_dir(&previous_folder)
        .map_err(|error| format!("could not create {}: {error}", previous_folder.display()))?;
    let previous = previous_folder.join("Concat Live.app");
    std::fs::rename(target, &previous)
        .map_err(|error| format!("could not preserve the previous app: {error}"))?;
    if let Err(error) = std::fs::rename(staged, target) {
        return match std::fs::rename(&previous, target) {
            Ok(()) => Err(format!(
                "could not install the new app; previous app restored: {error}"
            )),
            Err(rollback) => Err(format!(
                "could not install the new app: {error}; restore {} manually: {rollback}",
                previous.display()
            )),
        };
    }
    if let Err(error) = verify() {
        let failed = work.join("failed-app");
        std::fs::rename(target, &failed)
            .map_err(|rollback| format!("installed app failed verification ({error}); rollback could not preserve it: {rollback}; previous app: {}", previous.display()))?;
        std::fs::rename(&previous, target).map_err(|rollback| {
            format!(
                "installed app failed verification ({error}); restore {} manually: {rollback}",
                previous.display()
            )
        })?;
        return Err(format!(
            "new app failed verification; previous app restored: {error}"
        ));
    }
    Ok(previous)
}

fn require_bundle_identifier(bundle: &Path) -> Result<(), String> {
    let identifier = capture(
        Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print:CFBundleIdentifier"])
            .arg(bundle.join("Contents/Info.plist")),
    )?;
    if identifier.trim() != crate::dirs::IDENTIFIER {
        return Err("the update is not a ru.steel0rat.concat-live application bundle".into());
    }
    Ok(())
}

fn validate_bundle(bundle: &Path, expected: Option<Version>) -> Result<Version, String> {
    require_bundle_identifier(bundle)?;
    let canonical = bundle
        .canonicalize()
        .map_err(|error| format!("could not read the app bundle: {error}"))?;
    let binary = bundle.join("Contents/MacOS/concat");
    if !binary
        .canonicalize()
        .map_err(|error| format!("could not read the app executable: {error}"))?
        .starts_with(&canonical)
    {
        return Err("the app executable is outside its bundle".into());
    }
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(bundle))?;
    let reported = capture(Command::new(&binary).arg("--version"))?;
    let version = reported
        .trim()
        .strip_prefix("concat ")
        .and_then(Version::parse)
        .ok_or("the prepared application did not report a Concat version")?;
    if expected.is_some_and(|expected| expected != version) {
        return Err("the prepared application version differs from its release".into());
    }
    let declared = capture(
        Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print:CFBundleShortVersionString"])
            .arg(bundle.join("Contents/Info.plist")),
    )?;
    if Version::parse(declared.trim()) != Some(version) {
        return Err("the application bundle and executable report different versions".into());
    }
    run(Command::new(&binary).arg("--smoke"))?;
    Ok(version)
}

fn file_digest(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn replace_from_dmg(dmg: &Path) -> Result<Installed, String> {
    if own_bundle().is_none() {
        run(Command::new("/usr/bin/open").arg(dmg))?;
        return Ok(Installed::Opened);
    }
    let prepared = prepare_app_version(dmg, None)?;
    install_prepared_app(&prepared)?;
    Ok(Installed::Replaced)
}

/// Writes the new AppImage over the file this one was started from, which
/// the environment names; the launcher entries that point at it keep
/// working.
fn replace_appimage(package: &Path) -> Result<Installed, String> {
    let target = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .ok_or_else(|| "this is not running as an AppImage".to_owned())?;
    let mut staged = target.clone().into_os_string();
    staged.push(".new");
    let staged = PathBuf::from(staged);
    std::fs::copy(package, &staged)
        .map_err(|error| format!("could not write {}: {error}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("could not mark {} executable: {error}", staged.display()))?;
    }
    std::fs::rename(&staged, &target)
        .map_err(|error| format!("could not replace {}: {error}", target.display()))?;
    Ok(Installed::Replaced)
}

/// Starts the build now in place a moment from now, so the caller can quit
/// first. Nothing on Windows, where the installer offers to start the app
/// itself.
pub fn relaunch() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let bundle = own_bundle().ok_or_else(|| "not running from an app bundle".to_owned())?;
        detach(
            Command::new("sh")
                .args(["-c", "sleep 1; open -n \"$0\""])
                .arg(bundle),
        )
    } else if cfg!(target_os = "linux") {
        let target = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .ok_or_else(|| "could not tell what to start".to_owned())?;
        detach(
            Command::new("sh")
                .args(["-c", "sleep 1; exec \"$0\""])
                .arg(target),
        )
    } else {
        Ok(())
    }
}

/// Starts this build again, on `project` when one was open, so a setting
/// read only at launch - drawing on the GPU or in software - takes hold.
/// The caller saves, closes the project and quits; the new process opens
/// it once this one has let go of its lock. A Mac's app is started from
/// its bundle, so the Dock shows the one Concat; a build from the tree,
/// Linux and Windows start the executable itself. Nothing on a phone.
pub fn restart(project: Option<&Path>) -> Result<(), String> {
    restart_with_roots(project, &[])
}

/// Restarts while retaining the current launch's working folders.
/// The roots are passed to the child without changing this process's environment.
pub fn restart_with_roots(project: Option<&Path>, roots: &[PathBuf]) -> Result<(), String> {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        return Err("a phone's app is started from its launcher".into());
    }
    let target = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or("could not tell what to start")?;
    let bundle = cfg!(target_os = "macos").then(own_bundle).flatten();
    let mut command = restart_command(project, roots, bundle.as_deref(), &target)?;
    detach(&mut command)
}

fn restart_command(
    project: Option<&Path>,
    roots: &[PathBuf],
    bundle: Option<&Path>,
    target: &Path,
) -> Result<Command, String> {
    use std::ffi::OsString;
    if roots
        .iter()
        .any(|root| !root.is_absolute() || !root.is_dir())
    {
        return Err("the restart working folders must be existing absolute directories".into());
    }
    let encoded =
        if roots.is_empty() {
            None
        } else {
            Some(std::env::join_paths(roots).map_err(|error| {
                format!("the restart working folders cannot be encoded: {error}")
            })?)
        };
    let mut args: Vec<OsString> = Vec::new();
    if let Some(project) = project {
        args.push("--project".into());
        args.push(project.into());
    }
    if let Some(bundle) = bundle {
        let mut open_args: Vec<OsString> = vec!["-n".into(), bundle.into()];
        if let Some(roots) = encoded {
            let mut assignment = OsString::from("CONCAT_MCP_ROOTS=");
            assignment.push(roots);
            open_args.push("--env".into());
            open_args.push(assignment);
        }
        open_args.push("--args".into());
        open_args.extend(args);
        return Ok(delayed_command(Path::new("/usr/bin/open"), &open_args));
    }
    let mut command = if cfg!(windows) {
        let mut command = Command::new(target);
        command.args(&args);
        command
    } else {
        delayed_command(target, &args)
    };
    if let Some(encoded) = encoded {
        command.env("CONCAT_MCP_ROOTS", encoded);
    }
    Ok(command)
}

fn delayed_command(program: &Path, args: &[std::ffi::OsString]) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "sleep 1; exec \"$@\"", "concat-restart"])
        .arg(program)
        .args(args);
    command
}

/// Runs an update utility with bounded output and a finite deadline.
fn run(command: &mut Command) -> Result<(), String> {
    capture(command).map(|_| ())
}

fn capture(command: &mut Command) -> Result<String, String> {
    capture_with_timeout(command, Duration::from_secs(60))
}

#[cfg(unix)]
fn capture_with_timeout(command: &mut Command, timeout: Duration) -> Result<String, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    let mut output = match child.stdout.take() {
        Some(output) => output,
        None => {
            finish_update_child(child)?;
            return Err("the update utility did not open stdout".into());
        }
    };
    let fd = output.as_raw_fd();
    // The pipe belongs to this ChildStdout and stays open through both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    let nonblocking =
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0;
    let result = if nonblocking {
        monitor_update_child(&mut child, &mut output, Instant::now() + timeout, &program)
    } else {
        Err(format!(
            "could not make the update output nonblocking: {}",
            std::io::Error::last_os_error()
        ))
    };
    drop(output);
    let status = finish_update_child(child);
    let bytes = result?;
    let status = status?;
    if !status.success() {
        return Err(format!("{program} failed ({status})"));
    }
    String::from_utf8(bytes).map_err(|_| format!("{program} returned non-UTF-8 output"))
}

#[cfg(not(unix))]
fn capture_with_timeout(_command: &mut Command, _timeout: Duration) -> Result<String, String> {
    Err("verified update utilities currently require a Unix desktop".into())
}

#[cfg(unix)]
fn monitor_update_child(
    child: &mut std::process::Child,
    output: &mut std::process::ChildStdout,
    deadline: Instant,
    program: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    loop {
        drain_update_output(output, &mut bytes)?;
        // WNOWAIT keeps our child's PID reserved until its owned process
        // group has been terminated, so a recycled PID cannot be signalled.
        let mut observed = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let code = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                observed.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if code != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("could not observe {program}: {error}"));
            }
        } else {
            // The zero-initialized siginfo is valid even when WNOHANG
            // reports no event; waitid sets si_signo only for an observed event.
            let observed = unsafe { observed.assume_init() };
            if observed.si_signo != 0 {
                drain_update_output(output, &mut bytes)?;
                return Ok(bytes);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("{program} exceeded its update deadline"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn drain_update_output(
    output: &mut std::process::ChildStdout,
    bytes: &mut Vec<u8>,
) -> Result<(), String> {
    let mut buffer = [0u8; 4096];
    for _ in 0..32 {
        match output.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if bytes.len() + count > 64 * 1024 {
                    return Err("the update utility exceeded the output limit".into());
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
            Err(error) => return Err(format!("could not read the update utility output: {error}")),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn finish_update_child(mut child: std::process::Child) -> Result<std::process::ExitStatus, String> {
    let pid = child.id() as libc::pid_t;
    // process_group(0) created this group for our child; the unreaped
    // child's PID remains reserved and the editor's group is excluded.
    unsafe {
        if pid > 1 && libc::getpgrp() != pid {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted && Instant::now() < deadline =>
            {
                continue;
            }
            Err(error) => return Err(format!("could not reap the update utility: {error}")),
            Ok(None) => break,
        }
    }
    // Kernel termination can be delayed by uninterruptible IO. This
    // reaper owns no stdout pipe and never blocks the update worker.
    std::thread::Builder::new()
        .name("concat-update-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| format!("could not arrange update-child cleanup: {error}"))?;
    Err("the update utility is still terminating after its cleanup deadline".into())
}

/// Starts a command and lets it go, with no pipes back to this process.
fn detach(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start {program}: {error}"))
}

/// One GET, read whole as text. GitHub wants a user agent and says so with
/// a 403 otherwise.
fn get_text(url: &str) -> Result<String, String> {
    let agent = format!("Concat/{}", Version::current());
    let response = crate::net::get(
        url,
        std::time::Duration::from_secs(20),
        std::time::Duration::from_secs(30),
        crate::net::Trust::Shipped,
        &|request| {
            request
                .set("User-Agent", &agent)
                .set("Accept", "application/vnd.github+json")
        },
    )
    .map_err(|failure| {
        let crate::net::Failure {
            error,
            cause,
            proxy,
        } = *failure;
        match error {
            ureq::Error::Status(code, response) => {
                let body = response.into_string().unwrap_or_default();
                let said = serde_json::from_str::<serde_json::Value>(&body)
                    .map(|value| github_message(&value))
                    .unwrap_or_else(|_| body.chars().take(200).collect());
                format!("{url} answered {code}: {said}")
            }
            other => {
                let through = proxy
                    .map(|proxy| format!(" (through the proxy {proxy})"))
                    .unwrap_or_default();
                format!(
                    "{url} did not answer: {}{through}: {other}",
                    cause.describe()
                )
            }
        }
    })?;
    let mut text = String::new();
    response
        .into_reader()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{url} was cut short: {error}"))?;
    if text.len() > 1024 * 1024 {
        return Err("the release metadata exceeds one MiB".into());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_reads_a_tag_and_orders_by_its_numbers() {
        assert_eq!(Version::parse("v0.2.5"), Some(Version::new(0, 2, 5)));
        assert_eq!(Version::parse("0.2.5"), Some(Version::new(0, 2, 5)));
        assert_eq!(Version::parse(" v1.0.0 "), Some(Version::new(1, 0, 0)));
        for not_a_release in ["v0.3.0-beta.1", "nightly", "0.2", "0.2.5.1", "", "v"] {
            assert_eq!(Version::parse(not_a_release), None, "{not_a_release:?}");
        }
        // By number, not by text: ten comes after nine.
        assert!(Version::new(0, 2, 10) > Version::new(0, 2, 9));
        assert!(Version::new(0, 3, 0) > Version::new(0, 2, 99));
        assert!(Version::new(1, 0, 0) > Version::new(0, 9, 9));
        assert_eq!(Version::new(0, 2, 5).to_string(), "0.2.5");
        assert_eq!(Version::new(0, 2, 5).tag(), "v0.2.5");
        assert!(Version::current() >= Version::new(0, 2, 4));
    }

    const RELEASES: &str = r#"[
        {"tag_name": "nightly", "draft": false, "prerelease": true, "published_at": "2026-10-03T01:00:00Z"},
        {"tag_name": "v0.3.2", "draft": true, "prerelease": false, "published_at": null},
        {"tag_name": "v0.3.0-beta.1", "draft": false, "prerelease": true, "published_at": "2026-10-02T12:00:00Z"},
        {"tag_name": "v0.3.0", "draft": false, "prerelease": false, "published_at": "2026-09-28T09:30:00Z"},
        {"tag_name": "v0.3.1", "draft": false, "prerelease": false, "published_at": "2026-10-01T17:34:02Z"},
        {"tag_name": "v0.2.10", "draft": false, "prerelease": false, "published_at": "2026-09-24T17:34:02Z"},
        {"tag_name": "v0.3.1", "draft": false, "prerelease": false, "published_at": "2026-10-01T17:34:02Z"}
    ]"#;

    #[test]
    fn the_list_keeps_stable_releases_from_the_first_switchable_newest_first() {
        let releases = releases_from_json(RELEASES).expect("reads");
        let versions: Vec<String> = releases
            .iter()
            .map(|release| release.version.to_string())
            .collect();
        assert_eq!(versions, ["0.3.1", "0.3.0"]);
        assert_eq!(releases[0].tag, "v0.3.1");
        assert_eq!(releases[0].published, "2026-10-01");
        assert_eq!(releases[1].published, "2026-09-28");
        assert!(releases_from_json("[]").expect("reads").is_empty());
    }

    #[test]
    fn a_refusal_from_github_is_read_out_in_its_own_words() {
        let error = releases_from_json(r#"{"message": "API rate limit exceeded for 1.2.3.4."}"#)
            .expect_err("refused");
        assert!(error.contains("rate limit"), "{error}");
        assert!(releases_from_json("not json").is_err());
    }

    #[test]
    fn standing_says_where_this_build_sits() {
        let releases = releases_from_json(RELEASES).expect("reads");
        assert_eq!(
            standing(Version::new(0, 3, 0), &releases),
            Standing::Behind(Version::new(0, 3, 1))
        );
        assert_eq!(standing(Version::new(0, 3, 1), &releases), Standing::Latest);
        assert_eq!(standing(Version::new(0, 3, 2), &releases), Standing::Ahead);
        assert_eq!(standing(Version::new(0, 3, 1), &[]), Standing::NoReleases);
    }

    const MANIFEST: &str = r#"{
        "schema": 2, "version": "0.2.6", "tag": "v0.2.6",
        "binaries": {
            "macos": {"arm64": {"dmg": {"file": "Concat-0.2.6-macos-arm64.dmg", "url": "https://example.test/arm64.dmg", "bytes": 59456586, "sha256": "26cc"}}},
            "linux": {"x86_64": {"appimage": {"file": "Concat-0.2.6-linux-x86_64.AppImage", "url": "https://example.test/x.AppImage", "bytes": 1, "sha256": "5c75"},
                                 "deb": {"file": "../evil.deb", "url": "https://example.test/evil", "bytes": 1, "sha256": "00"}}},
            "windows": {"aarch64": {"setup": {"file": "Concat-0.2.6-windows-aarch64-setup.exe", "url": "https://example.test/setup.exe", "sha256": "7042"}}}
        }
    }"#;

    #[test]
    fn the_package_is_found_by_platform_architecture_and_kind() {
        let mac =
            package_from_manifest(MANIFEST, "macos", "aarch64", PackageKind::Dmg).expect("a Mac's");
        assert_eq!(mac.file, "Concat-0.2.6-macos-arm64.dmg");
        assert_eq!(mac.bytes, 59456586);
        assert_eq!(mac.sha256, "26cc");
        let appimage = package_from_manifest(MANIFEST, "linux", "x86_64", PackageKind::AppImage)
            .expect("an AppImage");
        assert_eq!(appimage.url, "https://example.test/x.AppImage");
        // A size the manifest leaves out is zero, not a refusal.
        let setup = package_from_manifest(MANIFEST, "windows", "aarch64", PackageKind::Setup)
            .expect("an installer");
        assert_eq!(setup.bytes, 0);
        let missing = package_from_manifest(MANIFEST, "windows", "x86_64", PackageKind::Setup)
            .expect_err("no x86_64 Windows build in this manifest");
        assert!(
            missing.contains("no setup package for windows x86_64"),
            "{missing}"
        );
        // A file name with a folder in it is not a file name.
        let evil = package_from_manifest(MANIFEST, "linux", "x86_64", PackageKind::Deb)
            .expect_err("refused");
        assert!(evil.contains("not a file name"), "{evil}");
    }

    #[test]
    fn the_manifest_names_a_mac_arm64_and_everything_else_aarch64() {
        assert_eq!(manifest_arch("macos", "aarch64"), "arm64");
        assert_eq!(manifest_arch("macos", "x86_64"), "x86_64");
        assert_eq!(manifest_arch("linux", "aarch64"), "aarch64");
        assert_eq!(manifest_arch("windows", "aarch64"), "aarch64");
    }

    #[test]
    fn a_linux_install_is_told_by_what_is_on_the_machine() {
        assert_eq!(
            linux_kind(true, false, false, false, false),
            Ok(PackageKind::AppImage)
        );
        assert_eq!(
            linux_kind(false, false, true, false, false),
            Ok(PackageKind::Deb)
        );
        assert_eq!(
            linux_kind(false, false, false, true, false),
            Ok(PackageKind::Pacman)
        );
        assert_eq!(
            linux_kind(false, false, false, false, true),
            Ok(PackageKind::Rpm)
        );
        // A Flatpak is a Flatpak whatever else the environment says.
        assert_eq!(
            linux_kind(true, true, true, false, false),
            Err(Fixed::Flatpak)
        );
        assert_eq!(
            linux_kind(false, false, false, false, false),
            Err(Fixed::NotFromPackage)
        );
        // The setup's uninstaller wins even under Program Files,
        // where an administrator's run of the setup also puts it; the .msi
        // is the Program Files install without one; the portable zip is
        // neither.
        assert_eq!(windows_kind(true, false), Ok(PackageKind::Setup));
        assert_eq!(windows_kind(true, true), Ok(PackageKind::Setup));
        assert_eq!(windows_kind(false, true), Ok(PackageKind::Msi));
        assert_eq!(windows_kind(false, false), Err(Fixed::NotFromPackage));
        for kind in [
            PackageKind::Dmg,
            PackageKind::Setup,
            PackageKind::Msi,
            PackageKind::AppImage,
            PackageKind::Deb,
            PackageKind::Rpm,
            PackageKind::Pacman,
        ] {
            assert!(!kind.key().is_empty());
        }
        assert_eq!(
            manifest_url("v0.2.6"),
            "https://github.com/steel0rat/concat-live/releases/download/v0.2.6/manifest.json"
        );
    }

    #[test]
    fn the_updates_folder_hangs_off_the_data_directory() {
        let dirs = AppDirs::under(Path::new("/tmp/concat-updates-test"));
        assert_eq!(folder(&dirs), Path::new("/tmp/concat-updates-test/updates"));
    }
    #[test]
    fn fork_updates_never_point_at_upstream() {
        assert_eq!(REPO, "steel0rat/concat-live");
        assert!(manifest_url("v0.3.0").starts_with("https://github.com/steel0rat/concat-live/"));
        assert!(RELEASES_URL.contains("/steel0rat/concat-live/"));
    }

    #[test]
    fn a_failed_download_preserves_another_cached_package() {
        let directory = tempfile::tempdir().expect("temporary update cache");
        let kept = directory.path().join("previous.dmg");
        std::fs::write(&kept, b"keep old download").expect("old file");
        let package = Package {
            file: "future.dmg".into(),
            url: "invalid-url".into(),
            bytes: 1,
            sha256: "00".into(),
        };
        let result = download_package(
            &package,
            directory.path(),
            &AtomicBool::new(false),
            &mut |_, _| {},
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(&kept).expect("previous download survives"),
            b"keep old download"
        );
    }

    #[test]
    fn untrusted_download_urls_and_invalid_digests_are_refused() {
        let valid = Package {file:"Concat-0.3.0-macos-arm64.dmg".into(),
            url:"https://github.com/steel0rat/concat-live/releases/download/v0.3.0/Concat-0.3.0-macos-arm64.dmg".into(),
            bytes:8,sha256:"a".repeat(64)};
        validate_package(&valid, REPO, Some("v0.3.0")).expect("our release");
        for url in [
            valid.url.replace("steel0rat/concat-live", "jub0t/Concat"),
            valid
                .url
                .replace("https://github.com/", "https://github.com.evil.test/"),
            valid.url.replace("v0.3.0/", "v0.4.0/"),
            valid.url.clone() + "?redirect=other",
        ] {
            let package = Package {
                url,
                ..valid.clone()
            };
            assert!(validate_package(&package, REPO, Some("v0.3.0")).is_err());
        }
        let package = Package {
            sha256: "not-a-digest".into(),
            ..valid
        };
        assert!(validate_package(&package, REPO, None).is_err());
    }

    fn fake_swap_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("temporary app installation");
        let target = dir.path().join("Concat Live.app");
        let staged = dir.path().join("staged.app");
        let work = dir.path().join("work");
        for folder in [&target, &staged, &work] {
            std::fs::create_dir(folder).expect("folder");
        }
        std::fs::write(target.join("version"), b"previous").expect("previous app");
        std::fs::write(staged.join("version"), b"next").expect("new app");
        (dir, target, staged, work)
    }

    #[test]
    fn successful_app_swap_keeps_the_previous_bundle() {
        let (_dir, target, staged, work) = fake_swap_fixture();
        let previous = swap_app(&staged, &target, &work, || Ok(())).expect("swap");
        assert_eq!(
            std::fs::read(target.join("version")).expect("installed app"),
            b"next"
        );
        assert_eq!(
            std::fs::read(previous.join("version")).expect("preserved app"),
            b"previous"
        );
    }

    #[test]
    fn a_failed_post_install_check_restores_the_old_app() {
        let (_dir, target, staged, work) = fake_swap_fixture();
        let error = swap_app(&staged, &target, &work, || Err("smoke failed".into()))
            .expect_err("verification failure");
        assert!(error.contains("previous app restored"));
        assert_eq!(
            std::fs::read(target.join("version")).expect("restored app"),
            b"previous"
        );
        assert_eq!(
            std::fs::read(work.join("failed-app/version")).expect("failed new app preserved"),
            b"next"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_upstream_bundle_is_refused_before_any_executable_runs() {
        let dir = tempfile::tempdir().expect("temporary upstream bundle");
        let contents = dir.path().join("Concat.app/Contents");
        std::fs::create_dir_all(&contents).expect("bundle");
        std::fs::write(contents.join("Info.plist"),br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>app.concat.editor</string></dict></plist>"#).expect("plist");
        assert!(
            validate_bundle(
                contents.parent().expect("bundle"),
                Some(Version::new(0, 3, 0))
            )
            .expect_err("upstream refused")
            .contains("ru.steel0rat.concat-live")
        );
    }

    #[cfg(unix)]
    #[test]
    fn delayed_restart_preserves_literal_shell_characters_and_unicode() {
        use std::ffi::OsString;
        let temp = tempfile::tempdir().expect("restart arguments");
        let marker = temp.path().join("must-not-exist");
        let quote = char::from(96);
        let values = vec![
            OsString::from("/tmp/Монтаж и сцены/film"),
            OsString::from(format!(
                "$(touch '{}'); {quote}touch '{}'{quote}",
                marker.display(),
                marker.display()
            )),
            OsString::from("a\"b'c $HOME"),
        ];
        let mut args = vec![OsString::from("%s\\0")];
        args.extend(values.clone());
        let output = delayed_command(Path::new("/usr/bin/printf"), &args)
            .output()
            .expect("argument echo");
        assert!(output.status.success());
        let echoed: Vec<_> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .collect();
        for (actual, expected) in echoed.iter().zip(&values) {
            use std::os::unix::ffi::OsStrExt;
            assert_eq!(*actual, expected.as_os_str().as_bytes());
        }
        assert_eq!(echoed.len(), values.len());
        assert!(!marker.exists(), "shell metacharacters must remain data");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn restart_passes_the_exact_working_roots_to_launch_services() {
        use std::ffi::OsString;
        let temp = tempfile::tempdir().expect("restart roots");
        let root = temp.path().join("Мои видео и проекты");
        std::fs::create_dir(&root).expect("working folder");
        let before = std::env::var_os("CONCAT_MCP_ROOTS");
        let bundle = Path::new("/Applications/Concat Live.app");
        let project = root.join("Ролик");
        let command = restart_command(
            Some(&project),
            std::slice::from_ref(&root),
            Some(bundle),
            Path::new("/ignored"),
        )
        .expect("restart");
        let args: Vec<_> = command
            .get_args()
            .map(std::ffi::OsStr::to_os_string)
            .collect();
        let mut assignment = OsString::from("CONCAT_MCP_ROOTS=");
        assignment.push(std::env::join_paths([root]).expect("roots"));
        assert!(
            args.windows(2)
                .any(|pair| pair == [OsString::from("--env"), assignment.clone()])
        );
        assert!(args.windows(2).any(|pair| pair
            == [
                OsString::from("--project"),
                project.as_os_str().to_os_string()
            ]));
        assert_eq!(
            std::env::var_os("CONCAT_MCP_ROOTS"),
            before,
            "the parent environment is unchanged"
        );
    }
    #[cfg(unix)]
    #[test]
    fn capture_does_not_wait_for_a_descendants_inherited_stdout() {
        let start = Instant::now();
        let result = capture(Command::new("/bin/sh").args(["-c", "sleep 1 & exit 1"]));
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "capture waited for inherited stdout after its child exited"
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_capture_deadline_terminates_a_live_child() {
        let before = unsafe { libc::getpgrp() };
        let start = Instant::now();
        let error = capture_with_timeout(
            Command::new("/bin/sh").args(["-c", "exec sleep 10"]),
            Duration::from_millis(30),
        )
        .expect_err("deadline");
        assert!(error.contains("deadline"));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(
            unsafe { libc::getpgrp() },
            before,
            "the editor's process group is unchanged"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_terminates_only_its_owned_stdout_descendant_group() {
        let start = Instant::now();
        let output = capture_with_timeout(
            Command::new("/bin/sh").args(["-c", "sleep 10 & printf '%s' \"$!\"; exit 0"]),
            Duration::from_millis(100),
        )
        .expect("completed leader");
        let descendant = output
            .trim()
            .parse::<libc::pid_t>()
            .expect("our descendant PID");
        assert!(start.elapsed() < Duration::from_millis(500));
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(
            unsafe { libc::kill(descendant, 0) },
            0,
            "the owned descendant must not remain alive"
        );
    }
}
