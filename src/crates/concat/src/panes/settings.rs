// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The settings sheet: the preferences, the two model lists, and the API
//! server's switch.
//!
//! Everything on the sheet is a preference or follows from one, so most
//! messages do three things: change the pane, write the preference, and
//! apply it - to the downloaders, to the server, to the words on screen.
//! The model lists are the one part with a worker behind them: a download
//! reports as [`SettingsMsg::ModelProgress`] and ends as
//! [`SettingsMsg::ModelFinished`], and the lists are rebuilt from what is
//! on disk after anything that could have changed it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use concat_host::models::SourcePreference;
use concat_host::updates::{self, Fixed, Installed, Release, Standing, Version};
use slint::{ModelRc, SharedString, VecModel};

use crate::close_window;
use crate::host::{on_ui, spawn};
use crate::i18n::{self, t, tf};
use crate::keymap;
use crate::panes::Msg;
use crate::platform;
use crate::prefs;
use crate::studio::Studio;
use crate::ui::{ModelData, SettingsData, ShortcutData};

/// Everything that can happen to the settings sheet.
#[derive(Clone, Debug)]
pub enum SettingsMsg {
    /// The preferences read back on launch, and applied.
    Restore,
    /// The sheet is asked for: the menu, the tray or the shortcut.
    Open,
    Close,
    PageChanged(i32),
    /// The Shortcuts page: a row asked to hold the next key it should
    /// be bound to. The sheet closing puts the listening down too — see
    /// `Close` — so no key anywhere binds something the page stopped
    /// caring about.
    ShortcutCapture(String),
    /// One row put back as it ships, taking its chord off whoever holds
    /// it now, and every row put back at once.
    ShortcutReset(String),
    ShortcutResetAll,
    /// Show this run's log in the file manager.
    ShowLog,
    LanguageChanged(i32),
    /// The log level's row: an index into [`LOG_LEVELS`].
    LogLevelChanged(i32),
    PlayheadStopsChanged(bool),
    /// The proxies switch: whether large files get a smaller copy to play.
    ProxiesChanged(bool),
    /// The remember-layout switch: whether a launch opens on the last
    /// session's panels and window size.
    RememberLayoutChanged(bool),
    /// The GPU switch: on, the window and the compositor draw on the GPU;
    /// off, in software. Read at the next launch.
    GpuChanged(bool),
    /// The magnetic timeline switch; the tray's button is the same fact.
    MagneticChanged(bool),
    DownloadSourceChanged(i32),
    DownloadBaseEdited(String),
    ServerEnabledChanged(bool),
    /// The chosen workspace applies only to this live launch.
    LiveWorkspaceChanged(PathBuf),
    /// The address half of where the server listens; the port is its own
    /// field, so a person types "127.0.0.1" and "7420" and never a colon.
    ServerHostEdited(String),
    ServerPortEdited(i32),
    ServerTokenEdited(String),
    ServerTokenGenerated,
    /// Make this the model the engine uses.
    ModelActivated(String),
    ModelDownload(String),
    ModelCancel(String),
    ModelRemove(String),
    /// A failed download's way round: open the model's first source in
    /// the browser, which the network may let through where it did not
    /// let Concat ...
    ModelOpenLink(String),
    /// ... and install the file that browser saved, once it is picked.
    ModelChooseFile(String),
    ModelChosen {
        id: String,
        path: std::path::PathBuf,
    },
    /// A download's worker reporting where it is.
    ModelProgress {
        id: String,
        /// Megabytes so far.
        fetched: f32,
        /// Megabytes in all, when the server said.
        total: Option<f32>,
        unpacking: bool,
    },
    /// A download's worker is done: the model on disk, or why not.
    ModelFinished {
        id: String,
        result: Result<(), String>,
    },
    /// The Version page asks GitHub for the releases.
    CheckUpdates,
    /// The launch's periodic check and application of prepared updates.
    AutoTick,
    /// A verified app waits for an idle editor before it replaces the bundle.
    AppPrepared {
        version: Version,
        result: Result<updates::PreparedApp, String>,
    },
    /// A verified skill waits until the current chat request has finished.
    SkillPrepared(Result<Option<updates::skill::InstalledSkill>, String>),
    SkillActivated {
        version: Version,
        result: Result<(), String>,
    },
    /// The releases, newest first, or why they could not be had.
    UpdatesFetched(Result<Vec<Release>, String>),
    /// A release picked from the Version page's list.
    VersionChosen(i32),
    /// Put the picked release in this one's place.
    InstallVersion,
    InstallCancel,
    /// The switch's worker reporting its download, in megabytes.
    InstallProgress {
        fetched: f32,
        total: f32,
    },
    /// The switch's worker is done: what it did, or why not.
    InstallFinished {
        version: Version,
        result: Result<Installed, String>,
    },
}

/// The Version page's index in the column of pages.
const VERSION_PAGE: i32 = 6;
const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// The Shortcuts page's index, which the launch screen's Keyboard
/// shortcuts card opens the sheet on.
pub const SHORTCUTS_PAGE: i32 = 7;

/// One downloadable model, as the settings sheet shows it.
#[derive(Clone, Debug)]
pub struct ModelState {
    pub id: String,
    pub name: String,
    pub note: String,
    pub megabytes: f32,
    pub accuracy: i32,
    pub installed: bool,
    pub active: bool,
    /// Megabytes fetched so far while a download runs.
    pub fetched: Option<f32>,
    pub unpacking: bool,
    /// Why the last download failed, when it failed for a reason the row
    /// can say and offer a way round.
    pub failure: Option<concat_host::models::Failure>,
}

impl ModelState {
    /// The model as one row of the sheet's list.
    fn row(&self) -> ModelData {
        let total = self.megabytes;
        let fetched = self.fetched.unwrap_or(0.0);
        ModelData {
            id: self.id.as_str().into(),
            name: self.name.as_str().into(),
            note: self.note.as_str().into(),
            size: format!("{total:.0} MB").into(),
            accuracy: self.accuracy,
            installed: self.installed,
            active: self.active && self.installed,
            downloading: self.fetched.is_some(),
            progress: if total > 0.0 {
                (fetched / total).min(1.0)
            } else {
                0.0
            },
            transferred: if self.unpacking {
                t("settings.unpacking").into()
            } else {
                tf(
                    "settings.mbOfMb",
                    &[&format!("{fetched:.0}"), &format!("{total:.0}")],
                )
                .into()
            },
            eta: SharedString::new(),
            problem: self
                .failure
                .as_ref()
                .map(failure_line)
                .unwrap_or_default()
                .into(),
            manual: self.failure.is_some() && model_sources(&self.id).is_some(),
        }
    }
}

/// What model `id` is called as a file and where it is downloaded from,
/// whichever list it is on; `None` for one that cannot be installed from
/// a single file.
fn model_sources(id: &str) -> Option<(String, Vec<String>)> {
    concat_speech::Transcriber::model_sources(id)
        .or_else(|| concat_speech::Speech::model_sources(id))
}

/// A failed download, as the row under the model says it.
fn failure_line(failure: &concat_host::models::Failure) -> String {
    use concat_host::models::Failure;
    use concat_host::net::Cause;

    match failure {
        Failure::NotTheFile => t("settings.notTheModelFile"),
        Failure::Unreachable { cause, proxy } => match cause {
            Cause::Blocked => t("settings.downloadBlocked"),
            Cause::Offline => t("settings.downloadOffline"),
            Cause::Dns => t("settings.downloadDns"),
            Cause::Proxy => tf(
                "settings.downloadProxy",
                &[&proxy.as_deref().unwrap_or_default()],
            ),
            Cause::Certificate => t("settings.downloadCertificate"),
            Cause::Reset => t("settings.downloadReset"),
            Cause::Refused | Cause::Timeout => t("settings.downloadNoAnswer"),
            Cause::Status(code) => tf("settings.downloadStatus", &[code]),
            Cause::Other => t("settings.downloadFailed"),
        },
    }
}

/// The models of a kind that are on disk, in the settings' order: the
/// rows of a sheet's model list.
pub fn installed(models: &[ModelState]) -> Vec<&ModelState> {
    models.iter().filter(|model| model.installed).collect()
}

/// The settings sheet's state.
#[derive(Default)]
pub struct SettingsPane {
    pub open: bool,
    pub tab: i32,
    /// The Shortcuts page's row listening for its next key: the action
    /// that key will bind to. None when none is — and when the sheet
    /// closes under a listening row, so a key pressed in the editor
    /// afterwards dispatches rather than binding.
    pub capturing: Option<String>,
    pub language: usize,
    /// The switch that keeps the playhead inside the content.
    pub playhead_stops: bool,
    /// Index into `SourcePreference::ALL`: where model downloads look first.
    pub download_source: usize,
    /// The base URL of a custom download source.
    pub download_base: String,
    /// Launch-only Automation settings, excluded from every preferences save.
    live_server: Option<prefs::ServerPrefs>,
    /// An explicit folder replaces the default write roots for this live launch.
    live_workspace: Option<PathBuf>,
    /// Why the server is not running when the switch is on: the bind that
    /// failed. Empty while it runs, or is off.
    pub server_error: String,
    /// The transcriber models, installed or not.
    pub transcribers: Vec<ModelState>,
    /// The voice models, installed or not.
    pub voices: Vec<ModelState>,
    /// The Version page's list: the stable releases from 0.2.5 on, newest
    /// first, once GitHub has been asked.
    pub releases: Vec<Release>,
    /// Which of `releases` the page's dropdown shows.
    pub version_choice: usize,
    /// What the Version page says under the version: looking, up to date,
    /// a newer release, or why it could not look.
    pub update_status: String,
    /// GitHub has been asked and has not answered yet.
    pub checking: bool,
    /// GitHub has answered this run: the page asks once when it is opened,
    /// and again only when the button is pressed.
    checked: bool,
    /// A switch in flight: megabytes fetched, and megabytes in all once
    /// the server has said.
    install: Option<(f32, f32)>,
    /// The running switch's stop flag.
    install_cancel: Arc<AtomicBool>,
    auto_timer: slint::Timer,
    next_auto_check: Option<Instant>,
    pending_app: Option<(Version, updates::PreparedApp)>,
    pending_skill: Option<updates::skill::InstalledSkill>,
    installed_skill: Option<updates::skill::InstalledSkill>,
    skill_status: String,
    checking_skill: bool,
    applying: bool,
    skipped_version: Option<Version>,
}

impl SettingsPane {
    pub(crate) fn for_live() -> Self {
        Self {
            live_server: Some(prefs::ServerPrefs {
                enabled: true,
                listen: "127.0.0.1:0".to_owned(),
                token: String::new(),
            }),
            ..Self::default()
        }
    }

    pub(crate) fn live_workspace(&self) -> Option<&Path> {
        self.live_workspace.as_deref()
    }

    fn select_live_workspace(&mut self, live_enabled: bool, path: PathBuf) -> Result<(), String> {
        if !live_enabled || self.live_server.is_none() {
            return Err("Рабочую папку можно выбирать только в Concat Live.".to_owned());
        }
        let canonical = path.canonicalize().map_err(|error| {
            format!(
                "Не удалось открыть рабочую папку {}: {error}",
                path.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "Рабочая папка должна быть каталогом: {}",
                path.display()
            ));
        }
        self.live_workspace = Some(canonical);
        Ok(())
    }

    fn server_roots(&self, live_enabled: bool) -> Vec<PathBuf> {
        if !live_enabled {
            return Vec::new();
        }
        if let Some(workspace) = self.live_workspace() {
            return vec![workspace.to_path_buf()];
        }
        std::env::var_os("CONCAT_MCP_ROOTS")
            .filter(|paths| !paths.is_empty())
            .map(|paths| std::env::split_paths(&paths).collect())
            .unwrap_or_default()
    }

    fn server_prefs<'a>(&'a self, prefs: &'a prefs::Preferences) -> &'a prefs::ServerPrefs {
        self.live_server.as_ref().unwrap_or(&prefs.server)
    }

    fn server_prefs_mut<'a>(
        &'a mut self,
        prefs: &'a mut prefs::Preferences,
    ) -> &'a mut prefs::ServerPrefs {
        self.live_server.as_mut().unwrap_or(&mut prefs.server)
    }

    fn change_server(&mut self, studio: &mut Studio, change: impl FnOnce(&mut prefs::ServerPrefs)) {
        change(self.server_prefs_mut(&mut studio.prefs));
        if self.live_server.is_none() {
            studio.prefs.save(&studio.host.dirs);
        }
        self.apply_server(studio);
    }

    /// Applies one message. The studio is the rest of the window; while
    /// this runs the studio's copy of the pane is a blank it must not read.
    pub fn update(&mut self, msg: SettingsMsg, studio: &mut Studio) {
        match msg {
            SettingsMsg::Restore => {
                self.language = studio
                    .languages
                    .iter()
                    .position(|language| {
                        Some(language.code.as_str()) == studio.prefs.locale.as_deref()
                    })
                    .unwrap_or(0);
                self.playhead_stops = studio.prefs.playhead_stops_at_end;
                // Not a choice: the app decodes on whatever hardware the
                // platform has, and a file the hardware will not take opens
                // in software (concat_media::hardware). Linux has no default
                // device, so there this is software as before.
                concat_media::set_hardware_decode(true);
                self.download_source = Self::download_source(studio).0;
                self.download_base = studio.prefs.download_base.clone().unwrap_or_default();
                Self::apply_download_source(studio);
                self.apply_server(studio);
                self.refresh(studio);
                if studio.live_enabled && cfg!(target_os = "macos") {
                    match updates::skill::current(&studio.host.dirs) {
                        Ok(skill) => self.installed_skill = skill,
                        Err(error) => self.skill_status = error,
                    }
                    self.auto_timer.start(
                        slint::TimerMode::Repeated,
                        Duration::from_secs(5),
                        || {
                            on_ui(|studio, _, _| {
                                studio.handle(Msg::Settings(SettingsMsg::AutoTick))
                            });
                        },
                    );
                    on_ui(|studio, _, _| studio.handle(Msg::Settings(SettingsMsg::AutoTick)));
                }
            }
            SettingsMsg::Open => {
                self.refresh(studio);
                self.open = true;
            }
            SettingsMsg::Close => {
                // A row left listening when the sheet goes would take
                // the next key anywhere in the window; it is put down
                // with the sheet.
                self.capturing = None;
                self.open = false;
            }
            SettingsMsg::PageChanged(index) => {
                self.tab = index;
                // The Version page looks for the releases the first time it
                // is opened in a run, so it has something to say and to list
                // without a press; the button looks again.
                if index == VERSION_PAGE && !self.checked {
                    self.check_updates();
                }
            }
            SettingsMsg::ShortcutCapture(id) => self.capturing = Some(id),
            SettingsMsg::ShortcutReset(id) => {
                self.capturing = None;
                let lost = keymap::reset(&mut studio.prefs, &id);
                studio.prefs.save(&studio.host.dirs);
                // The shipped chord came back off whoever was holding
                // it, and that row now has no key until it is reset.
                if let Some(lost) = lost {
                    studio.notify(&tf("shortcuts.unboundNow", &[&keymap::name(lost)]), false);
                }
            }
            SettingsMsg::ShortcutResetAll => {
                self.capturing = None;
                keymap::reset_all(&mut studio.prefs);
                studio.prefs.save(&studio.host.dirs);
            }
            SettingsMsg::ShowLog => {
                // The file this run is writing, when there is one, so the
                // manager opens with it selected; the folder when there is
                // not, which is still where the previous runs are.
                let target = concat_host::logs::current()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| concat_host::logs::folder(&studio.host.dirs));
                if let Err(error) = platform::reveal(&target.to_string_lossy()) {
                    studio.notify(&i18n::tf("settings.couldNotShowLog", &[&error]), true);
                }
            }
            SettingsMsg::LanguageChanged(index) => {
                let index = index.max(0) as usize;
                if let Some(language) = studio.languages.get(index).cloned() {
                    self.language = index;
                    studio.prefs.locale = Some(language.code.clone());
                    // The words change on the publish that follows: Rust's
                    // on their way through `t`, the tree's through
                    // `I18n.lang`.
                    i18n::select(&language.code, &studio.host.dirs);
                }
                studio.prefs.save(&studio.host.dirs);
            }
            SettingsMsg::LogLevelChanged(index) => {
                let Some(level) = LOG_LEVELS.get(index.max(0) as usize) else {
                    return;
                };
                studio.prefs.log_level = Some(concat_host::logs::level_name(*level).to_owned());
                studio.prefs.save(&studio.host.dirs);
                // Applied now, whatever the environment said: a person at
                // the sheet is asking for this run too.
                concat_host::logs::set_level(*level);
            }
            SettingsMsg::PlayheadStopsChanged(on) => {
                self.playhead_stops = on;
                studio.prefs.playhead_stops_at_end = on;
                studio.prefs.save(&studio.host.dirs);
                // A playhead already out past the end comes back in when
                // the switch goes on; seek does the clamp.
                let at = studio.playhead;
                studio.seek(at);
            }
            SettingsMsg::ProxiesChanged(on) => {
                studio.prefs.proxies = on;
                studio.prefs.save(&studio.host.dirs);
                // On, the media already in the project gets its proxies
                // straight away (refresh_art's stamp holds the switch);
                // off, playback reads the files from the next frame.
            }
            SettingsMsg::RememberLayoutChanged(on) => {
                studio.prefs.remember_layout = Some(on);
                // Off forgets what was kept, so turning it on again starts
                // from the arrangement on screen then, not a stale one.
                if on {
                    studio.prefs.save(&studio.host.dirs);
                    studio.remember_layout_soon();
                } else {
                    studio.prefs.layout = None;
                    studio.prefs.save(&studio.host.dirs);
                }
            }
            SettingsMsg::GpuChanged(on) => {
                studio.prefs.software_gpu = !on;
                studio.prefs.software_after_crash = None;
                studio.prefs.save(&studio.host.dirs);
                // Read only at launch: offered as a restart now, which
                // saves and reopens the project, or later.
                studio.confirm = Some(crate::studio::Confirm::Restart);
            }
            SettingsMsg::MagneticChanged(on) => {
                studio.prefs.set_magnetic(on);
                studio.prefs.save(&studio.host.dirs);
            }
            SettingsMsg::DownloadSourceChanged(index) => {
                let index = (index.max(0) as usize).min(SourcePreference::ALL.len() - 1);
                self.download_source = index;
                studio.prefs.download_source = Some(SourcePreference::ALL[index].name().to_owned());
                studio.prefs.save(&studio.host.dirs);
                Self::apply_download_source(studio);
            }
            SettingsMsg::DownloadBaseEdited(text) => {
                let base = text.trim().to_owned();
                self.download_base = base.clone();
                studio.prefs.download_base = (!base.is_empty()).then_some(base);
                studio.prefs.save(&studio.host.dirs);
                Self::apply_download_source(studio);
            }
            SettingsMsg::ServerEnabledChanged(on) => {
                self.change_server(studio, |server| server.enabled = on);
            }
            SettingsMsg::LiveWorkspaceChanged(path) => {
                match self.select_live_workspace(studio.live_enabled, path) {
                    Ok(()) => self.apply_server(studio),
                    Err(error) => {
                        self.server_error = error.clone();
                        studio.notify(&error, true);
                    }
                }
            }
            SettingsMsg::ServerHostEdited(text) => {
                self.change_server(studio, |server| {
                    let (_, port) = split_listen(&server.listen);
                    server.listen = join_listen(&text, port);
                });
            }
            SettingsMsg::ServerPortEdited(port) => {
                self.change_server(studio, |server| {
                    let (host, _) = split_listen(&server.listen);
                    server.listen = join_listen(&host, port.clamp(0, 65535) as u16);
                });
            }
            SettingsMsg::ServerTokenEdited(text) => {
                self.change_server(studio, |server| server.token = text.trim().to_owned());
            }
            SettingsMsg::ServerTokenGenerated => {
                self.change_server(studio, |server| server.token = new_token());
            }
            SettingsMsg::ModelActivated(id) => {
                if self.is_transcriber(&id) {
                    studio.prefs.transcriber_model = Some(id);
                } else {
                    studio.prefs.tts_model = Some(id);
                }
                studio.prefs.save(&studio.host.dirs);
                self.refresh(studio);
            }
            SettingsMsg::ModelDownload(id) => self.download(&id, studio),
            SettingsMsg::ModelOpenLink(id) => {
                if let Some(url) = model_sources(&id).and_then(|(_, urls)| urls.into_iter().next())
                    && let Err(error) = platform::open_link(&url)
                {
                    studio.notify(&error, true);
                }
            }
            SettingsMsg::ModelChooseFile(id) => {
                crate::host::pick_files_then(
                    t("settings.chooseModelFile"),
                    None,
                    move |studio, _, _, paths| {
                        if let Some(path) = paths.into_iter().next() {
                            studio.handle(Msg::Settings(SettingsMsg::ModelChosen { id, path }));
                        }
                    },
                );
            }
            // The file goes the way a download does - into the model's
            // partial, checked against the digest, unpacked if it is an
            // archive - so a wrong file is refused the same way.
            SettingsMsg::ModelChosen { id, path } => {
                if let Some((file, _)) = model_sources(&id) {
                    concat_host::models::supply(&file, path);
                    self.download(&id, studio);
                }
            }
            SettingsMsg::ModelCancel(id) => {
                if self.is_transcriber(&id) {
                    studio.host.transcriber.cancel_download();
                } else {
                    studio.host.speech.cancel_download();
                }
            }
            SettingsMsg::ModelRemove(id) => {
                let result = if self.is_transcriber(&id) {
                    studio.host.transcriber.delete_model(&studio.host.dirs, &id)
                } else {
                    studio.host.speech.delete_model(&studio.host.dirs, &id)
                };
                if let Err(error) = result {
                    studio.notify(&error, true);
                }
                self.refresh(studio);
            }
            SettingsMsg::ModelProgress {
                id,
                fetched,
                total,
                unpacking,
            } => {
                if let Some(model) = self.model_mut(&id) {
                    model.fetched = Some(fetched);
                    model.unpacking = unpacking;
                    if let Some(total) = total {
                        model.megabytes = total;
                    }
                }
            }
            SettingsMsg::ModelFinished { id, result } => {
                // The host's account of a failed fetch, when it was the
                // network or the file and not a cancel or the disk.
                let failure = result.as_ref().err().and_then(|_| {
                    model_sources(&id)
                        .and_then(|(file, _)| concat_host::models::last_failure(&file))
                });
                if let Some(model) = self.model_mut(&id) {
                    model.fetched = None;
                    model.unpacking = false;
                    model.failure = failure.clone();
                }
                match result {
                    Ok(()) => {
                        studio.notify(&t("settings.modelReady"), false);
                        if self.is_transcriber(&id) && studio.prefs.transcriber_model.is_none() {
                            studio.prefs.transcriber_model = Some(id.clone());
                        } else if !self.is_transcriber(&id) && studio.prefs.tts_model.is_none() {
                            studio.prefs.tts_model = Some(id.clone());
                        }
                        studio.prefs.save(&studio.host.dirs);
                    }
                    Err(error) => match &failure {
                        Some(failure) => studio.notify(&failure_line(failure), true),
                        None => studio.notify(&error, true),
                    },
                }
                self.refresh(studio);
            }
            SettingsMsg::CheckUpdates => {
                self.check_updates();
                self.check_skill(studio);
                self.next_auto_check = Some(Instant::now() + AUTO_CHECK_INTERVAL);
            }
            SettingsMsg::AutoTick => self.poll_updates(studio),
            SettingsMsg::AppPrepared { version, result } => {
                match result {
                    Ok(prepared) if !self.install_cancel.load(Ordering::Relaxed) => {
                        self.pending_app = Some((version, prepared));
                        self.update_status = format!(
                            "Concat Live {version} загружен. Ожидаю завершения работы в редакторе."
                        );
                    }
                    Ok(_) => self.install = None,
                    Err(_) if self.install_cancel.load(Ordering::Relaxed) => self.install = None,
                    Err(error) => {
                        self.install = None;
                        self.update_status =
                            format!("Не удалось подготовить обновление {version}: {error}");
                    }
                }
                self.apply_ready_update(studio);
            }
            SettingsMsg::SkillPrepared(result) => {
                self.checking_skill = false;
                match result {
                    Ok(Some(skill)) => {
                        self.skill_status = format!(
                            "Навык {} загружен. Он подключится после текущего запроса.",
                            skill.version
                        );
                        self.pending_skill = Some(skill);
                    }
                    Ok(None) => {
                        self.skill_status = match &self.installed_skill {
                            Some(skill) => format!("Установлена версия навыка {}.", skill.version),
                            None => "Опубликованных обновлений навыка пока нет. Используется установленный в Codex навык.".into(),
                        };
                    }
                    Err(error) => self.skill_status = format!("Не удалось обновить навык: {error}"),
                }
                self.apply_ready_update(studio);
            }
            SettingsMsg::SkillActivated { version, result } => {
                self.applying = false;
                crate::agent::resume_after_update();
                match result {
                    Ok(()) => match updates::skill::current(&studio.host.dirs) {
                        Ok(skill) => {
                            self.installed_skill = skill;
                            self.skill_status = format!(
                                "Навык {version} подключится со следующего запроса. Беседа сохранена."
                            );
                        }
                        Err(error) => self.skill_status = error,
                    },
                    Err(error) => {
                        self.skill_status =
                            format!("Не удалось подключить навык {version}: {error}")
                    }
                }
                self.apply_ready_update(studio);
            }
            SettingsMsg::UpdatesFetched(result) => {
                self.checking = false;
                match result {
                    Ok(releases) => {
                        self.checked = true;
                        let current = Version::current();
                        self.update_status = match updates::standing(current, &releases) {
                            Standing::Latest => tf("settings.upToDate", &[&current]),
                            Standing::Behind(newer) => {
                                tf("settings.updateAvailable", &[&newer, &current])
                            }
                            Standing::Ahead => tf("settings.aheadOfReleases", &[&current]),
                            Standing::NoReleases => t("settings.noReleasesYet"),
                        };
                        // The list starts on its newest release, which is
                        // the one most people opened the page for.
                        self.version_choice = 0;
                        self.releases = releases;
                        if studio.live_enabled
                            && cfg!(target_os = "macos")
                            && updates::installed_as().is_ok()
                            && let Some(index) = self.automatic_release(current)
                        {
                            self.version_choice = index;
                            self.install_version(studio);
                        }
                    }
                    Err(error) => self.update_status = tf("settings.couldNotCheck", &[&error]),
                }
            }
            SettingsMsg::VersionChosen(index) => {
                self.version_choice =
                    (index.max(0) as usize).min(self.releases.len().saturating_sub(1));
            }
            SettingsMsg::InstallVersion => self.install_version(studio),
            SettingsMsg::InstallCancel => {
                if self.applying {
                    return;
                }
                self.install_cancel.store(true, Ordering::Relaxed);
                self.skipped_version = self
                    .releases
                    .get(self.version_choice)
                    .map(|release| release.version);
                if self.pending_app.take().is_some() {
                    self.install = None;
                }
            }
            SettingsMsg::InstallProgress { fetched, total } => {
                if self.install.is_some() {
                    self.install = Some((fetched, total));
                }
            }
            SettingsMsg::InstallFinished { version, result } => {
                self.install = None;
                self.applying = false;
                match result {
                    Ok(Installed::InstallerRunning) => {
                        log::info!("update: the installer for {version} is running; closing");
                        studio.notify(&tf("settings.installerOpened", &[&version]), false);
                        quit_for_switch(|_, _| {});
                    }
                    Ok(Installed::Replaced) => {
                        // The bundle on disk is already the new build; what
                        // is left is to start it, and that waits on the
                        // project being saved.
                        log::info!("update: {version} is in place; starting it");
                        studio.notify(&tf("settings.versionInstalled", &[&version]), false);
                        let project = studio
                            .session
                            .as_ref()
                            .map(|session| PathBuf::from(session.path()));
                        restart(project);
                    }
                    Ok(Installed::Opened) => {
                        crate::agent::resume_after_update();
                        studio.notify(&tf("settings.packageOpened", &[&version]), false);
                    }
                    // A cancel is a choice, not a failure worth a toast.
                    Err(_) if self.install_cancel.load(Ordering::Relaxed) => {
                        crate::agent::resume_after_update()
                    }
                    Err(error) => {
                        crate::agent::resume_after_update();
                        self.update_status = format!("Не удалось установить {version}: {error}");
                        log::warn!("update: {version} could not be installed: {error}");
                        studio.notify(&tf("settings.couldNotInstall", &[&version, &error]), true);
                    }
                }
            }
        }
    }

    /// Asks GitHub for the releases on a worker; the answer comes back as
    /// [`SettingsMsg::UpdatesFetched`].
    fn check_updates(&mut self) {
        if self.checking {
            return;
        }
        self.checking = true;
        self.update_status = t("settings.checkingForUpdates");
        spawn(updates::fetch_releases, |studio, _, _, result| {
            studio.handle(Msg::Settings(SettingsMsg::UpdatesFetched(result)));
        });
    }

    /// Fetches the chosen release's package for this machine on a worker,
    /// checks it against the release's digest, and puts it in place. The
    /// download reports as [`SettingsMsg::InstallProgress`] and the whole
    /// ends as [`SettingsMsg::InstallFinished`].
    fn install_version(&mut self, studio: &mut Studio) {
        if self.install.is_some() || self.applying {
            return;
        }
        let Some(release) = self.releases.get(self.version_choice).cloned() else {
            return;
        };
        let kind = match updates::installed_as() {
            Ok(kind) => kind,
            Err(why) => {
                studio.notify(&fixed_note(why), true);
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.install_cancel = Arc::clone(&cancel);
        self.install = Some((0.0, 0.0));
        let folder = updates::folder(&studio.host.dirs);
        let version = release.version;
        spawn(
            move || {
                let result = updates::fetch_package(&release, kind).and_then(|package| {
                    let mut report = |received: u64, total: u64| {
                        on_ui(move |studio, _, _| {
                            studio.handle(Msg::Settings(SettingsMsg::InstallProgress {
                                fetched: received as f32 / 1_000_000.0,
                                total: total as f32 / 1_000_000.0,
                            }));
                        });
                    };
                    updates::download_package(&package, &folder, &cancel, &mut report)
                });
                if kind == updates::PackageKind::Dmg {
                    let result = result.and_then(|path| updates::prepare_app(&path, version));
                    SettingsMsg::AppPrepared { version, result }
                } else {
                    SettingsMsg::InstallFinished {
                        version,
                        result: result.and_then(|path| updates::install(&path, kind)),
                    }
                }
            },
            |studio, _, _, message| studio.handle(Msg::Settings(message)),
        );
    }

    fn automatic_release(&self, current: Version) -> Option<usize> {
        if self.install.is_some() || self.pending_app.is_some() || self.applying {
            return None;
        }
        self.releases
            .first()
            .filter(|release| {
                release.version > current && self.skipped_version != Some(release.version)
            })
            .map(|_| 0)
    }

    fn check_skill(&mut self, studio: &Studio) {
        if !studio.live_enabled
            || !cfg!(target_os = "macos")
            || self.checking_skill
            || self.pending_skill.is_some()
            || self.applying
        {
            return;
        }
        self.checking_skill = true;
        self.skill_status = "Проверяю опубликованные версии навыка…".into();
        let dirs = studio.host.dirs.clone();
        spawn(
            move || {
                let Some(manifest) = updates::skill::fetch_latest(Version::current())? else {
                    return Ok(None);
                };
                if updates::skill::current(&dirs)?
                    .is_some_and(|current| current.version >= manifest.version)
                {
                    return Ok(None);
                }
                let skill = updates::skill::prepare(
                    &manifest,
                    &dirs,
                    &AtomicBool::new(false),
                    &mut |_, _| {},
                )?;
                Ok(Some(skill))
            },
            |studio, _, _, result| studio.handle(Msg::Settings(SettingsMsg::SkillPrepared(result))),
        );
    }

    fn poll_updates(&mut self, studio: &mut Studio) {
        if !studio.live_enabled || !cfg!(target_os = "macos") {
            return;
        }
        if self
            .next_auto_check
            .is_none_or(|next| Instant::now() >= next)
        {
            self.next_auto_check = Some(Instant::now() + AUTO_CHECK_INTERVAL);
            self.check_updates();
            self.check_skill(studio);
        }
        self.apply_ready_update(studio);
    }

    fn apply_ready_update(&mut self, studio: &mut Studio) {
        if self.applying || studio.busy_for_update() || crate::host::native_dialog_open() {
            return;
        }
        if self.pending_skill.is_some()
            && !crate::agent::busy_for_update(false)
            && crate::agent::pause_for_update(false)
        {
            let skill = self.pending_skill.take().expect("a prepared skill");
            let version = skill.version;
            let dirs = studio.host.dirs.clone();
            self.applying = true;
            spawn(
                move || updates::skill::activate(&dirs, &skill),
                move |studio, _, _, result| {
                    studio.handle(Msg::Settings(SettingsMsg::SkillActivated {
                        version,
                        result,
                    }))
                },
            );
            return;
        }
        if self.pending_app.is_none()
            || crate::agent::busy_for_update(true)
            || !crate::agent::pause_for_update(true)
        {
            return;
        }
        let (version, prepared) = self.pending_app.take().expect("a prepared app");
        studio.flush_commit();
        let saved = studio
            .session
            .as_mut()
            .map(|session| session.save(None))
            .transpose();
        if let Err(error) = saved {
            self.install = None;
            self.skipped_version = Some(version);
            self.update_status =
                format!("Обновление отложено: не удалось сохранить проект: {error}");
            studio.notify(&self.update_status, true);
            crate::agent::resume_after_update();
            return;
        }
        self.applying = true;
        self.update_status = format!("Устанавливаю Concat Live {version}…");
        spawn(
            move || {
                updates::install_prepared_app(&prepared).map(|backup| {
                    log::info!("update: previous app preserved at {}", backup.display());
                    Installed::Replaced
                })
            },
            move |studio, _, _, result| {
                studio.handle(Msg::Settings(SettingsMsg::InstallFinished {
                    version,
                    result,
                }))
            },
        );
    }

    /// The Version page's list: each release's number.
    pub fn version_rows(&self) -> Vec<SharedString> {
        self.releases
            .iter()
            .map(|release| release.version.to_string().into())
            .collect()
    }

    /// Beside each release: the day it came out, or that it is this one.
    pub fn version_details(&self) -> Vec<SharedString> {
        let current = Version::current();
        self.releases
            .iter()
            .map(|release| {
                if release.version == current {
                    t("settings.thisVersion").into()
                } else {
                    release.published.as_str().into()
                }
            })
            .collect()
    }

    /// The remembered download source: its place in the menu, and itself.
    pub fn download_source(studio: &Studio) -> (usize, SourcePreference) {
        let preference =
            SourcePreference::parse(studio.prefs.download_source.as_deref().unwrap_or_default());
        let index = SourcePreference::ALL
            .iter()
            .position(|candidate| *candidate == preference)
            .unwrap_or(0);
        (index, preference)
    }

    /// Tells the downloaders where to look first, from the preferences.
    fn apply_download_source(studio: &Studio) {
        let (_, preference) = Self::download_source(studio);
        concat_host::models::set_preference(
            preference,
            studio.prefs.download_base.as_deref().unwrap_or_default(),
        );
    }

    /// Starts or stops the API's server to match the preferences. A server
    /// already running is stopped first, so an edited address or token
    /// takes effect; a bind that fails turns the switch back off and says
    /// why on the page.
    fn apply_server(&mut self, studio: &mut Studio) {
        if studio.live_enabled {
            crate::live::invalidate_server();
        }
        // Detached: the address is free again when this returns, and a
        // call or an export the old server was in the middle of is
        // cancelled and finished off the event loop, not waited for on it.
        if let Some(server) = studio.host.server.take() {
            server.stop_detached();
        }
        self.server_error.clear();
        let exporter = studio.host.exporter.clone();
        let open_projects = studio.host.open_projects.clone();
        let dirs = studio.host.dirs.clone();
        let live_enabled = studio.live_enabled;
        let roots = self.server_roots(live_enabled);
        let prefs = self.server_prefs(&studio.prefs);
        if !prefs.enabled {
            return;
        }
        // A token being typed is weak until it is long enough; the switch
        // stays on and the server waits for it, rather than the switch
        // flipping off at the first keystroke.
        if !prefs.token.is_empty() && concat_server::check_token(&prefs.token).is_err() {
            self.server_error = tf("settings.tokenTooWeak", &[&concat_server::MIN_TOKEN_CHARS]);
            return;
        }
        let window = live_enabled.then(crate::live::Window::new);
        let started = prefs
            .listen
            .trim()
            .parse::<std::net::SocketAddr>()
            .map_err(|_| tf("settings.notAnAddress", &[&prefs.listen]))
            .and_then(|address| {
                let config = concat_server::Config {
                    json: Some(address),
                    roots,
                    token: Some(prefs.token.clone()).filter(|token| !token.is_empty()),
                    // Where `concat --mcp`, started by an agent, finds this
                    // server rather than opening projects behind its back.
                    discovery: Some(concat_server::discovery::path_in(&dirs.config)),
                    ..concat_server::Config::default()
                };
                // The window keeps the export slot and project claims shared.
                // Live mode forwards edits to its existing session on the event loop.
                concat_server::Server::start(config, move |events| {
                    let mut api = concat_api::Api::new(events)?;
                    api.share_exporter(exporter);
                    api.share_open_projects(open_projects);
                    if let Some(window) = window {
                        api.enable_live(Arc::new(window));
                    }
                    Ok(api)
                })
            });
        match started {
            Ok(server) => studio.host.server = Some(server),
            Err(error) => {
                self.server_prefs_mut(&mut studio.prefs).enabled = false;
                if self.live_server.is_none() {
                    studio.prefs.save(&studio.host.dirs);
                }
                self.server_error = error.clone();
                studio.notify(&error, true);
            }
        }
    }

    /// What the Automation page says under the switch.
    fn server_status(&self, studio: &Studio) -> String {
        match &studio.host.server {
            Some(server) => {
                let address = server
                    .json_addr()
                    .map(|address| address.to_string())
                    .unwrap_or_default();
                tf(
                    "settings.listeningOnConnected",
                    &[&address, &server.connections()],
                )
            }
            None if !self.server_error.is_empty() => self.server_error.clone(),
            None => t("settings.off"),
        }
    }

    /// The two model lists, from what is on disk. A download in flight
    /// keeps its progress across the rebuild.
    fn refresh(&mut self, studio: &mut Studio) {
        let dirs = &studio.host.dirs;
        let downloading: HashMap<String, (Option<f32>, bool)> = self
            .transcribers
            .iter()
            .chain(self.voices.iter())
            .filter(|model| model.fetched.is_some())
            .map(|model| (model.id.clone(), (model.fetched, model.unpacking)))
            .collect();
        let failed: HashMap<String, concat_host::models::Failure> = self
            .transcribers
            .iter()
            .chain(self.voices.iter())
            .filter_map(|model| Some((model.id.clone(), model.failure.clone()?)))
            .collect();
        let chosen_transcriber = studio.prefs.transcriber_model.clone();
        let chosen_voice = studio.prefs.tts_model.clone();
        if let Ok(status) = concat_speech::Transcriber::status(dirs) {
            self.transcribers = status
                .models
                .iter()
                .map(|model| {
                    let (fetched, unpacking) =
                        downloading.get(&model.id).copied().unwrap_or((None, false));
                    ModelState {
                        id: model.id.clone(),
                        name: model.label.clone(),
                        note: model.blurb.clone(),
                        megabytes: model.size_bytes as f32 / 1_000_000.0,
                        accuracy: if model.id.starts_with("tiny") {
                            2
                        } else if model.id.starts_with("base") {
                            3
                        } else {
                            4
                        },
                        installed: model.downloaded,
                        active: chosen_transcriber.as_deref() == Some(model.id.as_str()),
                        fetched,
                        unpacking,
                        failure: failed.get(&model.id).cloned(),
                    }
                })
                .collect();
        }
        if let Ok(status) = concat_speech::Speech::status(dirs) {
            studio.speech.speakers = status.voices.clone();
            self.voices = status
                .models
                .iter()
                .map(|model| {
                    let (fetched, unpacking) =
                        downloading.get(&model.id).copied().unwrap_or((None, false));
                    ModelState {
                        id: model.id.clone(),
                        name: model.label.clone(),
                        note: model.blurb.clone(),
                        megabytes: model.size_bytes as f32 / 1_000_000.0,
                        accuracy: if model.id.contains("int8") { 4 } else { 5 },
                        installed: model.downloaded,
                        active: chosen_voice.as_deref() == Some(model.id.as_str()),
                        fetched,
                        unpacking,
                        failure: failed.get(&model.id).cloned(),
                    }
                })
                .collect();
        }
        // An engine with nothing chosen falls back to whatever is installed,
        // rather than silently having no model at all.
        for list in [&mut self.transcribers, &mut self.voices] {
            if !list.iter().any(|model| model.active && model.installed)
                && let Some(first) = list.iter_mut().find(|model| model.installed)
            {
                first.active = true;
            }
        }
    }

    fn is_transcriber(&self, id: &str) -> bool {
        self.transcribers.iter().any(|model| model.id == id)
    }

    fn model_mut(&mut self, id: &str) -> Option<&mut ModelState> {
        self.transcribers
            .iter_mut()
            .chain(self.voices.iter_mut())
            .find(|model| model.id == id)
    }

    /// Starts a download on a worker. Its reports come back as messages.
    fn download(&mut self, id: &str, studio: &Studio) {
        let transcriber = self.is_transcriber(id);
        let Some(model) = self.model_mut(id) else {
            return;
        };
        if model.installed || model.fetched.is_some() {
            return;
        }
        model.fetched = Some(0.0);
        model.failure = None;
        let id = id.to_owned();
        let dirs = studio.host.dirs.clone();
        let whisper = Arc::clone(&studio.host.transcriber);
        let speech = Arc::clone(&studio.host.speech);
        spawn(
            move || {
                let report = |progress: concat_speech::DownloadProgress| {
                    let msg = SettingsMsg::ModelProgress {
                        id: progress.id,
                        fetched: progress.received as f32 / 1_000_000.0,
                        total: (progress.total > 0).then(|| progress.total as f32 / 1_000_000.0),
                        unpacking: progress.unpacking,
                    };
                    on_ui(move |studio, _, _| studio.handle(Msg::Settings(msg)));
                };
                let result = if transcriber {
                    whisper.download_model(&dirs, &id, report)
                } else {
                    speech.download_model(&dirs, &id, report)
                };
                (id, result)
            },
            |studio, _, _, (id, result)| {
                studio.handle(Msg::Settings(SettingsMsg::ModelFinished { id, result }));
            },
        );
    }

    /// The transcriber list's rows.
    pub fn transcriber_rows(&self) -> Vec<ModelData> {
        self.transcribers.iter().map(ModelState::row).collect()
    }

    /// The voice list's rows.
    pub fn voice_rows(&self) -> Vec<ModelData> {
        self.voices.iter().map(ModelState::row).collect()
    }

    /// The sheet as Slint shows it.
    pub fn data(&self, studio: &Studio) -> SettingsData {
        let server_prefs = self.server_prefs(&studio.prefs);
        SettingsData {
            open: self.open,
            tab: self.tab,
            // The keymap's rows as the page draws them: the chord each
            // action is on now, and whether that moved from the one it
            // ships on — which is the row's cue to offer its reset.
            shortcuts: ModelRc::from(Rc::new(VecModel::from(
                keymap::ACTIONS
                    .iter()
                    .map(|action| ShortcutData {
                        id: action.id.into(),
                        label: keymap::name(action.id).into(),
                        keys: keymap::bound(&studio.prefs, action.id)
                            .map(keymap::display)
                            .unwrap_or_default()
                            .into(),
                        custom: studio.prefs.keybinds.contains_key(action.id),
                    })
                    .collect::<Vec<_>>(),
            ))),
            capturing: self.capturing.clone().unwrap_or_default().into(),
            language: self.language as i32,
            log_level: LOG_LEVELS
                .iter()
                .position(|level| *level == log::max_level())
                .unwrap_or(2) as i32,
            playhead_stops: self.playhead_stops,
            proxies: studio.prefs.proxies,
            remember_layout: studio.prefs.remembers_layout(),
            layout_switchable: !crate::platform::phone(),
            gpu: !studio.prefs.draws_in_software(),
            gpu_switchable: cfg!(any(windows, target_os = "linux", target_os = "macos")),
            magnetic: studio.prefs.magnetic(),
            download_source: self.download_source as i32,
            download_base: self.download_base.as_str().into(),
            server_enabled: server_prefs.enabled,
            server_host: split_listen(&server_prefs.listen).0.into(),
            server_port: i32::from(split_listen(&server_prefs.listen).1),
            server_token: match &studio.host.server {
                // A token the server minted for itself is shown where a
                // chosen one would be typed: it is how a caller gets in.
                Some(server) if server_prefs.token.is_empty() => server.token().into(),
                _ => server_prefs.token.as_str().into(),
            },
            server_status: self.server_status(studio).into(),
            server_state: match &studio.host.server {
                Some(_) => 1,
                None if !self.server_error.is_empty() => 2,
                None => 0,
            },
            server_local: is_loopback(&split_listen(&server_prefs.listen).0),
            disk: {
                let on_disk: Vec<&ModelState> = installed(&self.transcribers)
                    .into_iter()
                    .chain(installed(&self.voices))
                    .collect();
                let megabytes: f32 = on_disk.iter().map(|model| model.megabytes).sum();
                tf(
                    "settings.installedMbOnDisk",
                    &[&on_disk.len(), &format!("{megabytes:.0}")],
                )
                .into()
            },
            version: env!("CARGO_PKG_VERSION").into(),
            update_status: self.update_status.as_str().into(),
            auto_updates: studio.live_enabled && cfg!(target_os = "macos"),
            skill_version: self
                .installed_skill
                .as_ref()
                .map(|skill| skill.version.to_string())
                .unwrap_or_else(|| "Из каталога Codex".into())
                .into(),
            skill_update_status: self.skill_status.as_str().into(),
            checking: self.checking || self.checking_skill,
            version_choice: self.version_choice as i32,
            installing: self.install.is_some(),
            can_cancel_install: !self.applying,
            install_progress: match self.install {
                Some((fetched, total)) if total > 0.0 => (fetched / total).min(1.0),
                _ => 0.0,
            },
            install_transferred: match self.install {
                Some((fetched, total)) if total > 0.0 => tf(
                    "settings.mbOfMb",
                    &[&format!("{fetched:.0}"), &format!("{total:.0}")],
                ),
                Some(_) => tf(
                    "settings.downloadingVersion",
                    &[&self
                        .releases
                        .get(self.version_choice)
                        .map(|release| release.version.to_string())
                        .unwrap_or_default()],
                ),
                None => String::new(),
            }
            .into(),
            can_install: self.install.is_none()
                && !self.releases.is_empty()
                && updates::installed_as().is_ok(),
            install_note: updates::installed_as()
                .err()
                .map(fixed_note)
                .unwrap_or_default()
                .into(),
        }
    }
}

/// Why the Install button is off, in words.
fn fixed_note(why: Fixed) -> String {
    match why {
        Fixed::Flatpak => t("settings.fromFlatpak"),
        Fixed::Store => t("settings.fromStore"),
        Fixed::NotFromPackage => t("settings.notFromPackage"),
    }
}

/// Saves and closes the project, then quits - running `after`, the start of
/// a build already replaced on disk, only once the project has been written.
///
/// A switch is only worth making if the save lands first. The relaunch of a
/// replaced build used to be scheduled before the save, so a save that then
/// failed started the new build over a window that still held the old
/// session. Now a failed save starts nothing and quits nothing: the window
/// stays up with `close_project`'s reason, and the bundle already on disk
/// can be started by hand.
/// Saves, quits and starts Concat again on `project`; see
/// `updates::restart`. A save that fails keeps the window, as any close.
pub(crate) fn restart(_project: Option<std::path::PathBuf>) {
    log::info!("close: restarting Concat Live");
    quit_for_switch(move |project, roots| {
        if let Err(error) = updates::restart_with_roots(project.as_deref(), &roots) {
            log::warn!("restart: could not start Concat again: {error}");
        }
    });
}

fn quit_for_switch(after: impl FnOnce(Option<PathBuf>, Vec<PathBuf>) + Send + 'static) {
    on_ui(move |studio, app, models| {
        if crate::host::native_dialog_open() || studio.busy_for_update() {
            slint::Timer::single_shot(Duration::from_millis(250), move || quit_for_switch(after));
            return;
        }
        log::info!("close: quitting for a version switch");
        let project = studio
            .session
            .as_ref()
            .map(|session| PathBuf::from(session.path()));
        let roots = studio.settings.server_roots(studio.live_enabled);
        studio.save_layout(slint::ComponentHandle::window(app));
        let outcome = close_window(
            || studio.close_project(),
            || {
                after(project, roots);
                slint::ComponentHandle::window(app).hide().ok();
                slint::quit_event_loop().ok();
            },
        );
        if let Err(error) = outcome {
            crate::agent::resume_after_update();
            log::warn!("close: not switching, project could not be saved: {error}");
            studio.offer_quit_without_saving(error);
            studio.refresh_art();
            studio.publish(app, models);
        }
    });
}

/// A fresh token: 128 bits from the OS's randomness, as the standard
/// library hands it out through its hasher's seed, spelled in hex.
/// The log level row's choices, in the order the sheet lists them:
/// errors only, warnings, normal, debug, everything.
/// https://github.com/jub0t/Concat/issues/223
const LOG_LEVELS: [log::LevelFilter; 5] = [
    log::LevelFilter::Error,
    log::LevelFilter::Warn,
    log::LevelFilter::Info,
    log::LevelFilter::Debug,
    log::LevelFilter::Trace,
];

pub fn new_token() -> String {
    // The server's own minting, from the operating system's randomness.
    // The hasher's seed below is the fallback for a platform where that
    // fails, which none the app ships on does.
    if let Ok(token) = concat_server::mint_token() {
        return token;
    }
    use std::hash::{BuildHasher, Hasher};
    let word = |salt: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.finish()
    };
    format!("{:016x}{:016x}", word(1), word(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_updates_skip_current_older_cancelled_and_inflight_versions() {
        let mut pane = SettingsPane::default();
        let release = |patch| Release {
            version: Version::new(0, 3, patch),
            tag: format!("v0.3.{patch}"),
            published: String::new(),
        };
        pane.releases = vec![release(2), release(1), release(0)];
        assert_eq!(pane.automatic_release(Version::new(0, 3, 0)), Some(0));
        pane.skipped_version = Some(Version::new(0, 3, 2));
        assert_eq!(pane.automatic_release(Version::new(0, 3, 0)), None);
        assert_eq!(pane.automatic_release(Version::new(0, 3, 1)), None);
        pane.install = Some((1.0, 2.0));
        assert_eq!(pane.automatic_release(Version::new(0, 2, 0)), None);
        pane.install = None;
        pane.applying = true;
        assert_eq!(pane.automatic_release(Version::new(0, 2, 0)), None);
        pane.applying = false;
        assert_eq!(pane.automatic_release(Version::new(0, 4, 0)), None);
    }

    #[test]
    fn live_automation_changes_never_enter_saved_preferences() {
        let mut prefs = prefs::Preferences::default();
        let before = serde_json::to_value(&prefs).expect("serializes preferences");
        let mut pane = SettingsPane::for_live();
        let server = pane.server_prefs_mut(&mut prefs);
        assert!(server.enabled);
        assert_eq!(server.listen, "127.0.0.1:0");
        assert!(server.token.is_empty());
        server.enabled = false;
        server.listen = "127.0.0.1:7654".to_owned();
        assert_eq!(
            serde_json::to_value(&prefs).expect("serializes preferences"),
            before
        );
        assert!(!pane.server_prefs(&prefs).enabled);
        assert_eq!(pane.server_prefs(&prefs).listen, "127.0.0.1:7654");
    }

    #[test]
    fn a_chosen_live_workspace_replaces_default_roots_without_enabling_automation() {
        let mut prefs = prefs::Preferences::default();
        let before = serde_json::to_value(&prefs).expect("serializes preferences");
        let mut pane = SettingsPane::for_live();
        assert_eq!(pane.live_workspace(), None);
        let configured = std::env::var_os("CONCAT_MCP_ROOTS")
            .filter(|paths| !paths.is_empty())
            .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(pane.server_roots(true), configured);
        assert!(pane.server_roots(false).is_empty());

        pane.server_prefs_mut(&mut prefs).enabled = false;
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        let canonical = directory.canonicalize().expect("an existing directory");
        pane.select_live_workspace(true, directory.join("."))
            .expect("accepts an existing directory");
        assert_eq!(pane.live_workspace(), Some(canonical.as_path()));
        assert_eq!(pane.server_roots(true), vec![canonical]);
        assert!(pane.server_roots(false).is_empty());
        assert!(!pane.server_prefs(&prefs).enabled);
        assert_eq!(
            serde_json::to_value(&prefs).expect("serializes preferences"),
            before
        );
    }

    #[test]
    fn a_live_workspace_replaces_the_previous_choice_and_rejects_invalid_paths() {
        let mut pane = SettingsPane::for_live();
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        pane.select_live_workspace(true, directory.to_path_buf())
            .expect("accepts an existing directory");
        let selected = directory
            .join("src")
            .canonicalize()
            .expect("source directory");
        pane.select_live_workspace(true, selected.clone())
            .expect("replaces the previous workspace");
        assert_eq!(pane.server_roots(true), vec![selected.clone()]);

        let file = directory.join("Cargo.toml");
        for invalid in [file.clone(), file.join("not-a-directory")] {
            let error = pane
                .select_live_workspace(true, invalid)
                .expect_err("a file and a missing path cannot be workspaces");
            assert!(!error.is_empty());
            assert_eq!(pane.live_workspace(), Some(selected.as_path()));
            assert_eq!(pane.server_roots(true), vec![selected.clone()]);
        }
        assert!(
            pane.select_live_workspace(false, directory.to_path_buf())
                .is_err()
        );
        assert_eq!(pane.live_workspace(), Some(selected.as_path()));
        let mut normal = SettingsPane::default();
        assert!(
            normal
                .select_live_workspace(false, directory.to_path_buf())
                .is_err()
        );
        assert_eq!(normal.live_workspace(), None);
    }

    #[test]
    fn normal_automation_still_edits_saved_preferences() {
        let mut prefs = prefs::Preferences::default();
        let mut pane = SettingsPane::default();
        pane.server_prefs_mut(&mut prefs).enabled = true;
        assert!(prefs.server.enabled);
        assert!(pane.server_prefs(&prefs).enabled);
    }

    #[test]
    fn a_token_is_thirty_two_hex_digits_and_never_the_same_twice() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn a_row_shows_the_download_as_a_fraction_of_the_whole() {
        let model = ModelState {
            id: "base".into(),
            name: "Base".into(),
            note: String::new(),
            megabytes: 200.0,
            accuracy: 3,
            installed: false,
            active: true,
            fetched: Some(50.0),
            unpacking: false,
            failure: None,
        };
        let row = model.row();
        assert!(row.downloading);
        assert!(!row.active, "a model that is not installed is not active");
        assert!((row.progress - 0.25).abs() < 1e-6);
        assert!(row.problem.is_empty() && !row.manual);
    }

    #[test]
    fn a_failed_download_says_why_and_offers_the_way_round() {
        use concat_host::models::Failure;
        use concat_host::net::Cause;

        let failed = |id: &str, failure| ModelState {
            id: id.into(),
            name: id.into(),
            note: String::new(),
            megabytes: 75.0,
            accuracy: 2,
            installed: false,
            active: false,
            fetched: None,
            unpacking: false,
            failure: Some(failure),
        };
        let blocked = Failure::Unreachable {
            cause: Cause::Blocked,
            proxy: None,
        };
        let row = failed("tiny", blocked.clone()).row();
        assert_eq!(row.problem.as_str(), t("settings.downloadBlocked"));
        assert!(
            row.manual,
            "a whisper model is one file, so it can be chosen"
        );
        let row = failed("no-such-model", blocked).row();
        assert!(!row.problem.is_empty());
        assert!(
            !row.manual,
            "nothing to download it from, nothing to choose"
        );
        let proxy = Failure::Unreachable {
            cause: Cause::Proxy,
            proxy: Some("127.0.0.1:7890".into()),
        };
        assert!(failure_line(&proxy).contains("127.0.0.1:7890"));
        assert!(
            failure_line(&Failure::Unreachable {
                cause: Cause::Status(404),
                proxy: None
            })
            .contains("404")
        );
    }

    #[test]
    fn installed_keeps_the_settings_order() {
        let model = |id: &str, installed: bool| ModelState {
            id: id.into(),
            name: id.into(),
            note: String::new(),
            megabytes: 1.0,
            accuracy: 1,
            installed,
            active: false,
            fetched: None,
            unpacking: false,
            failure: None,
        };
        let list = [model("a", true), model("b", false), model("c", true)];
        let ids: Vec<&str> = installed(&list).iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);
    }
}

/// The listen address as its two halves: the host and the port. The
/// stored form is one string, "127.0.0.1:7420", because that is what the
/// server binds and the CLI takes; the sheet shows and edits the halves.
/// A bracketed IPv6 host keeps its brackets. An address with no port, or
/// a port that is not a number, gets the default port; an empty host gets
/// loopback.
/// Whether a listening host is this computer's alone: a loopback address
/// or "localhost". Anything else - 0.0.0.0, a LAN address, a name - can
/// be reached from the network, and the Automation page says so.
pub fn is_loopback(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub fn split_listen(listen: &str) -> (String, u16) {
    let (default_host, default_port) = match split_once_port(prefs::DEFAULT_LISTEN) {
        Some((host, Some(port))) => (host, port),
        _ => ("127.0.0.1", 7420),
    };
    let listen = listen.trim();
    // A host with a port that does not parse keeps the host: "host:" or
    // "host:abc" is a host and the default port, not a host of that name.
    let (host, port) = match split_once_port(listen) {
        Some((host, port)) => (host, port.unwrap_or(default_port)),
        None => (listen, default_port),
    };
    let host = host.trim();
    (
        if host.is_empty() {
            default_host.to_owned()
        } else {
            host.to_owned()
        },
        port,
    )
}

/// The host before the last colon and the port after it, when there is
/// such a place for a port: `None` for text with no colon or a bare IPv6
/// address, and a `None` port when what follows the colon is not one.
fn split_once_port(listen: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = listen.rsplit_once(':')?;
    // "::1" with no brackets is all colons; only a bracketed IPv6 host, or
    // a plain host, has a port after its last colon.
    if host.contains(':') && !host.ends_with(']') {
        return None;
    }
    Some((host, port.trim().parse().ok()))
}

/// The inverse of [`split_listen`]: the one string the server binds.
pub fn join_listen(host: &str, port: u16) -> String {
    let host = host.trim();
    let host = if host.is_empty() {
        split_listen(prefs::DEFAULT_LISTEN).0
    } else if host.contains(':') && !host.starts_with('[') {
        // A bare IPv6 address needs its brackets to take a port.
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    format!("{host}:{port}")
}

#[cfg(test)]
mod listen_tests {
    use super::*;

    #[test]
    fn a_listen_address_splits_into_host_and_port_and_joins_back() {
        assert_eq!(
            split_listen("127.0.0.1:7420"),
            ("127.0.0.1".to_owned(), 7420)
        );
        assert_eq!(split_listen("0.0.0.0:80"), ("0.0.0.0".to_owned(), 80));
        assert_eq!(split_listen("[::1]:7420"), ("[::1]".to_owned(), 7420));
        assert_eq!(
            split_listen("  10.0.0.5 : 9000 "),
            ("10.0.0.5".to_owned(), 9000)
        );
        assert_eq!(join_listen("127.0.0.1", 7420), "127.0.0.1:7420");
        assert_eq!(join_listen("[::1]", 1), "[::1]:1");
        assert_eq!(
            join_listen("::1", 7420),
            "[::1]:7420",
            "brackets are put on"
        );
        for listen in ["127.0.0.1:7420", "[::1]:7420", "0.0.0.0:65535"] {
            let (host, port) = split_listen(listen);
            assert_eq!(join_listen(&host, port), listen);
        }
    }

    /// The Automation page warns for any host past this computer.
    #[test]
    fn only_a_loopback_host_is_this_computer_alone() {
        for host in [
            "127.0.0.1",
            "127.0.0.2",
            "[::1]",
            "::1",
            "localhost",
            " LocalHost ",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["0.0.0.0", "192.168.1.9", "[::]", "example.com", ""] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn a_missing_or_bad_half_gets_the_default() {
        assert_eq!(
            split_listen("192.168.1.9"),
            ("192.168.1.9".to_owned(), 7420)
        );
        assert_eq!(
            split_listen("192.168.1.9:"),
            ("192.168.1.9".to_owned(), 7420)
        );
        assert_eq!(
            split_listen("192.168.1.9:abc"),
            ("192.168.1.9".to_owned(), 7420)
        );
        assert_eq!(
            split_listen("192.168.1.9:70000"),
            ("192.168.1.9".to_owned(), 7420)
        );
        assert_eq!(split_listen(":9000"), ("127.0.0.1".to_owned(), 9000));
        assert_eq!(split_listen(""), ("127.0.0.1".to_owned(), 7420));
        assert_eq!(
            split_listen("::1"),
            ("::1".to_owned(), 7420),
            "bare IPv6 is a host"
        );
        assert_eq!(join_listen("", 9000), "127.0.0.1:9000");
    }
}
