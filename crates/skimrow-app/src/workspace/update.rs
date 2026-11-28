//! Updates, Zed-style: look for a new release a little after startup and then
//! hourly, download it in the background, and offer ONE title-bar button —
//! "Restart to update" — that installs it and relaunches. Quitting installs a
//! downloaded update too, so the next start is the new version either way.
//!
//! The work itself (GitHub's API, download, unpack, the file swap) is
//! `skimrow_update`, blocking, run on `background_spawn`. Development builds
//! never replace themselves (`build_info::self_update_allowed`): a manual check
//! there only names the latest release. An install the user can't write to
//! (Program Files, /usr/bin, a read-only bundle) gets a button to the release
//! page instead.

use super::*;
use skimrow_update::{InstallTarget, Release, Staged};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Where the running app is installed — found at startup by [`init_updates`],
/// before anything could replace it.
static INSTALL: OnceLock<InstallTarget> = OnceLock::new();

/// The first automatic check waits a little: the connect goes first.
const FIRST_CHECK: Duration = Duration::from_secs(10);
/// Then once an hour, like Zed.
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// Called from `main` at startup (release builds): locate the install, delete
/// what the last update left there, and drop staged versions that are
/// installed by now. Must run before an update could replace the binary: on
/// Linux the running executable's path reads `… (deleted)` after that.
pub fn init_updates() {
    if !crate::build_info::self_update_allowed() {
        return;
    }
    match InstallTarget::current() {
        Ok(target) => {
            skimrow_update::cleanup(&target);
            let _ = INSTALL.set(target);
        }
        Err(e) => log(format!("updates: {e:#}")),
    }
    if let Some(cache) = update_cache() {
        skimrow_update::prune(&cache, crate::build_info::VERSION);
    }
}

fn install_target() -> Option<&'static InstallTarget> {
    INSTALL.get()
}

/// Downloads and staged updates, `<cache>/update/<version>/`.
fn update_cache() -> Option<PathBuf> {
    skimrow_config::cache_dir().map(|dir| dir.join("update"))
}

/// What GitHub is told: the app and version, nothing about the user.
fn user_agent() -> String {
    format!("skimrow/{}", crate::build_info::VERSION)
}

#[derive(Clone, Debug)]
pub(super) enum UpdateState {
    /// Not looked yet.
    Idle,
    Checking,
    UpToDate,
    Downloading {
        release: Release,
        /// Bytes so far (the download thread counts them).
        done: Arc<AtomicU64>,
    },
    /// Downloaded and unpacked: the title bar offers "Restart to update".
    Ready {
        release: Release,
        staged: Staged,
    },
    /// Newer, but this copy can't replace itself (`why`): the button opens the
    /// release page.
    Manual {
        release: Release,
        why: String,
    },
    /// Swapping the files in (then the app restarts / quits).
    Installing {
        release: Release,
    },
    Failed {
        message: String,
        /// The release it failed to get, if it got that far.
        release: Option<Release>,
    },
}

impl UpdateState {
    /// The release it knows about, if any.
    pub(super) fn release(&self) -> Option<&Release> {
        match self {
            Self::Downloading { release, .. }
            | Self::Ready { release, .. }
            | Self::Manual { release, .. }
            | Self::Installing { release } => Some(release),
            Self::Failed { release, .. } => release.as_ref(),
            Self::Idle | Self::Checking | Self::UpToDate => None,
        }
    }

    /// Something is in flight (a check, a download, an install).
    pub(super) fn busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading { .. } | Self::Installing { .. }
        )
    }

    /// One line for Settings → ABOUT.
    pub(super) fn describe(&self) -> String {
        match self {
            Self::Idle if !crate::build_info::self_update_allowed() => {
                "Development build: it doesn't update itself".into()
            }
            Self::Idle => "Not checked yet".into(),
            Self::Checking => "Checking for updates…".into(),
            Self::UpToDate => format!("Up to date ({})", crate::build_info::VERSION),
            Self::Downloading { release, done } => {
                let done = done.load(Ordering::Relaxed);
                match release.asset.as_ref().map(|a| a.size).filter(|s| *s > 0) {
                    Some(size) => format!(
                        "Downloading {} · {}%",
                        release.version,
                        (done * 100 / size).min(100)
                    ),
                    None => format!("Downloading {} · {} KB", release.version, done / 1024),
                }
            }
            Self::Ready { release, .. } => format!("{} is ready to install", release.version),
            Self::Manual { release, why } => format!("{} is available ({why})", release.version),
            Self::Installing { release } => format!("Installing {}…", release.version),
            Self::Failed { message, .. } => format!("Update failed: {message}"),
        }
    }
}

impl Workspace {
    /// From `new`: the automatic check, a little after startup and then hourly
    /// while Settings → "Check for updates automatically" is on (release builds).
    #[cfg_attr(test, allow(dead_code))]
    pub(super) fn start_update_checks(&mut self, cx: &mut Context<Self>) {
        if !crate::build_info::self_update_allowed() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let mut wait = FIRST_CHECK;
            loop {
                cx.background_executor().timer(wait).await;
                wait = CHECK_EVERY;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.settings.auto_update {
                            this.check_for_updates(false, cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Ask GitHub for the latest release. `manual` (Settings / palette) reports
    /// in the status line; the automatic check is quiet until there's a button
    /// to show.
    pub(super) fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        match &self.update {
            UpdateState::Ready { release, .. } => {
                if manual {
                    self.status = format!(
                        "skimrow {} is downloaded: Restart to update (title bar)",
                        release.version
                    );
                    cx.notify();
                }
                return;
            }
            state if state.busy() => {
                if manual {
                    self.status = state.describe();
                    cx.notify();
                }
                return;
            }
            _ => {}
        }
        self.update = UpdateState::Checking;
        if manual {
            self.status = "Checking for updates…".into();
        }
        cx.notify();
        let url = skimrow_update::endpoint(crate::build_info::REPOSITORY);
        let ua = user_agent();
        let task = cx.background_spawn(async move {
            skimrow_update::fetch_latest(&url, &ua, skimrow_update::asset_name())
                .map_err(|e| format!("{e:#}"))
        });
        self.spawn_db(cx, task, move |this, result, cx| {
            this.on_update_checked(manual, result, cx)
        });
    }

    /// The check's answer.
    pub(super) fn on_update_checked(
        &mut self,
        manual: bool,
        result: Result<Release, String>,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.update, UpdateState::Checking) {
            return;
        }
        let release = match result {
            Ok(release) => release,
            Err(message) => {
                if manual {
                    self.status = format!("Update check failed: {message}");
                }
                self.update = UpdateState::Failed {
                    message,
                    release: None,
                };
                return;
            }
        };
        if !skimrow_update::is_newer(crate::build_info::VERSION, &release.version) {
            if manual {
                self.status = format!("skimrow is up to date ({})", crate::build_info::VERSION);
            }
            self.update = UpdateState::UpToDate;
            return;
        }
        match self.install_blocker(&release) {
            Some(why) => {
                if manual {
                    self.status = format!("skimrow {} is available ({why})", release.version);
                }
                self.update = UpdateState::Manual { release, why };
            }
            None => self.start_download(release, manual, cx),
        }
    }

    /// Why this copy can't replace itself with `release`, if it can't.
    fn install_blocker(&self, release: &Release) -> Option<String> {
        if !crate::build_info::self_update_allowed() {
            return Some("development builds don't update themselves".into());
        }
        if release.asset.is_none() {
            return Some("no download for this platform".into());
        }
        let Some(target) = install_target() else {
            return Some("can't tell where skimrow is installed".into());
        };
        if let Err(e) = target.check_writable() {
            return Some(format!("{e:#}"));
        }
        if update_cache().is_none() {
            return Some("no cache directory to download into".into());
        }
        None
    }

    fn start_download(&mut self, release: Release, manual: bool, cx: &mut Context<Self>) {
        let Some(cache) = update_cache() else { return };
        let done = Arc::new(AtomicU64::new(0));
        self.update = UpdateState::Downloading {
            release: release.clone(),
            done: done.clone(),
        };
        if manual {
            self.status = format!("Downloading skimrow {}…", release.version);
        }
        self.tick_update_progress(cx);
        let ua = user_agent();
        let job = release.clone();
        let task = cx.background_spawn(async move {
            skimrow_update::prepare(&job, &cache, &ua, &done).map_err(|e| format!("{e:#}"))
        });
        self.spawn_db(cx, task, move |this, result, _cx| {
            this.on_update_downloaded(release, manual, result)
        });
    }

    /// The download (and unpack) is done.
    pub(super) fn on_update_downloaded(
        &mut self,
        release: Release,
        manual: bool,
        result: Result<Staged, String>,
    ) {
        if !matches!(self.update, UpdateState::Downloading { .. }) {
            return;
        }
        match result {
            Ok(staged) => {
                if manual {
                    self.status = format!(
                        "skimrow {} is downloaded: Restart to update (title bar)",
                        release.version
                    );
                }
                self.update = UpdateState::Ready { release, staged };
            }
            Err(message) => {
                if manual {
                    self.status = format!("Update download failed: {message}");
                }
                self.update = UpdateState::Failed {
                    message,
                    release: Some(release),
                };
            }
        }
    }

    /// Repaint Settings once a second while downloading (its progress line).
    fn tick_update_progress(&mut self, cx: &mut Context<Self>) {
        if self.update_ticker {
            return;
        }
        self.update_ticker = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let go_on = this
                .update(cx, |this, cx| {
                    let on = matches!(this.update, UpdateState::Downloading { .. });
                    this.update_ticker = on;
                    if on && this.is_modal(Modal::Settings) {
                        cx.notify();
                    }
                    on
                })
                .unwrap_or(false);
            if !go_on {
                break;
            }
        })
        .detach();
    }

    /// What a restart would throw away, one line per tab.
    pub(super) fn unsaved_work(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|t| {
                let mut what = Vec::new();
                if t.editing.is_some() {
                    what.push("a cell being edited".to_string());
                }
                match t.pending.len() {
                    0 => {}
                    1 => what.push("1 staged change".into()),
                    n => what.push(format!("{n} staged changes")),
                }
                if t.in_transaction {
                    what.push("an open transaction".into());
                }
                if t.applying.is_some() {
                    what.push("changes being applied".into());
                } else if t.running {
                    what.push("a running query".into());
                }
                (!what.is_empty()).then(|| format!("“{}”: {}", t.title, what.join(", ")))
            })
            .collect()
    }

    /// The title-bar button (and the palette's "Restart to update"): install
    /// and relaunch — after asking, when that would lose work.
    pub(super) fn request_update_restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.update, UpdateState::Ready { .. }) {
            self.status = "No update is downloaded yet".into();
            cx.notify();
            return;
        }
        if !self.unsaved_work().is_empty() {
            self.modal = Some(Modal::ConfirmRestart);
            cx.notify();
            return;
        }
        self.install_and_restart(window, cx);
    }

    /// Save what the close paths save (geometry, tabs, scratch), swap the new
    /// version in off the UI thread, then let gpui relaunch it.
    pub(super) fn install_and_restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let UpdateState::Ready { release, staged } = &self.update else {
            return;
        };
        let (release, staged) = (release.clone(), staged.clone());
        self.close_modal(cx);
        let Some(target) = install_target().cloned() else {
            self.update = UpdateState::Failed {
                message: "can't tell where skimrow is installed".into(),
                release: Some(release),
            };
            cx.notify();
            return;
        };
        save_window_geometry(window);
        self.flush_session(cx);
        session::store::flush_scratch();
        self.update = UpdateState::Installing {
            release: release.clone(),
        };
        cx.notify();
        let task = cx.background_spawn(async move {
            skimrow_update::install(&staged, &target)
                .map(|()| target.app)
                .map_err(|e| format!("{e:#}"))
        });
        self.spawn_db(cx, task, move |this, result, cx| match result {
            Ok(app) => {
                // The path found at startup: once replaced, the running
                // binary's own path no longer names it (Linux).
                cx.set_restart_path(app);
                cx.restart();
            }
            Err(message) => {
                this.status = format!("Update failed: {message}");
                this.update = UpdateState::Failed {
                    message,
                    release: Some(release),
                };
            }
        });
    }

    /// Quitting installs a downloaded update too (as Zed does), so the next
    /// start is the new version. Called from every close path; a failure is
    /// dropped — the staged copy stays in the cache, and the next start's check
    /// finds it and offers the button again.
    pub(super) fn install_on_quit(&mut self) {
        let UpdateState::Ready { release, staged } = &self.update else {
            return;
        };
        let Some(target) = install_target() else {
            return;
        };
        match skimrow_update::install(staged, target) {
            Ok(()) => {
                self.update = UpdateState::Installing {
                    release: release.clone(),
                }
            }
            Err(e) => log(format!("installing the update on quit: {e:#}")),
        }
    }

    /// Settings → check for updates automatically. Turning it on looks now.
    pub(super) fn set_auto_update(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.auto_update = on;
        self.save_settings();
        if on && crate::build_info::self_update_allowed() {
            self.check_for_updates(false, cx);
        }
        cx.notify();
    }

    /// The release page (notes, manual download) of the release it knows about.
    pub(super) fn open_release_page(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.update.release().map(|r| r.page.clone()) {
            if !page.is_empty() {
                cx.open_url(&page);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    fn release(version: &str) -> Release {
        Release {
            version: skimrow_update::Version::parse(version).unwrap(),
            page: format!("https://github.com/tochk/skimrow/releases/tag/v{version}"),
            asset: Some(skimrow_update::Asset {
                name: "skimrow-linux-x64.zip".into(),
                url: "https://example.invalid/skimrow.zip".into(),
                size: 100,
                sha256: None,
            }),
        }
    }

    fn staged(version: &str) -> Staged {
        Staged {
            version: skimrow_update::Version::parse(version).unwrap(),
            dir: PathBuf::from("/nonexistent/staged"),
            files: vec![skimrow_update::APP_FILE.into()],
        }
    }

    #[gpui::test]
    fn an_older_or_equal_release_is_up_to_date(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.update = UpdateState::Checking;
                ws.on_update_checked(true, Ok(release("0.0.1")), cx);
                assert!(matches!(ws.update, UpdateState::UpToDate));
                assert!(ws.status.contains("up to date"), "{}", ws.status);
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_dev_build_only_points_at_the_release(cx: &mut TestAppContext) {
        // Tests are a debug build: never replaces itself, offers the page.
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.update = UpdateState::Checking;
                ws.on_update_checked(false, Ok(release("999.0.0")), cx);
                match &ws.update {
                    UpdateState::Manual { release, why } => {
                        assert_eq!(release.version.major, 999);
                        assert!(why.contains("development"), "{why}");
                    }
                    other => panic!("expected Manual, got {other:?}"),
                }
                // The automatic check reports nothing in the status line.
                assert!(!ws.status.contains("999"));
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_failed_check_is_kept_for_settings(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.update = UpdateState::Checking;
                ws.on_update_checked(true, Err("offline".into()), cx);
                assert!(matches!(ws.update, UpdateState::Failed { .. }));
                assert!(ws.update.describe().contains("offline"));
                assert!(ws.status.contains("offline"));
                // A late answer for a check nobody waits on is dropped.
                ws.on_update_checked(true, Ok(release("999.0.0")), cx);
                assert!(matches!(ws.update, UpdateState::Failed { .. }));
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_finished_download_offers_the_restart(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.update = UpdateState::Downloading {
                    release: release("1.2.3"),
                    done: Arc::new(AtomicU64::new(42)),
                };
                assert_eq!(ws.update.describe(), "Downloading 1.2.3 · 42%");
                ws.on_update_downloaded(release("1.2.3"), false, Ok(staged("1.2.3")));
                assert!(matches!(ws.update, UpdateState::Ready { .. }));
                // A manual check now just points at the button.
                ws.check_for_updates(true, cx);
                assert!(matches!(ws.update, UpdateState::Ready { .. }));
                assert!(ws.status.contains("Restart to update"), "{}", ws.status);
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_failed_download_keeps_the_release(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, _cx| {
                ws.update = UpdateState::Downloading {
                    release: release("1.2.3"),
                    done: Arc::new(AtomicU64::new(0)),
                };
                ws.on_update_downloaded(release("1.2.3"), true, Err("disk full".into()));
                match &ws.update {
                    UpdateState::Failed { release, message } => {
                        assert_eq!(message, "disk full");
                        assert!(release.is_some(), "the page stays reachable");
                    }
                    other => panic!("expected Failed, got {other:?}"),
                }
            })
            .unwrap();
    }

    #[gpui::test]
    fn restart_asks_first_when_work_would_be_lost(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                ws.tab_mut(id).unwrap().in_transaction = true;
                ws.update = UpdateState::Ready {
                    release: release("1.2.3"),
                    staged: staged("1.2.3"),
                };
                let lost = ws.unsaved_work();
                assert_eq!(lost.len(), 1);
                assert!(lost[0].contains("open transaction"), "{lost:?}");
                ws.request_update_restart(window, cx);
                assert!(ws.is_modal(Modal::ConfirmRestart));
                // Nothing was installed or restarted: still ready.
                assert!(matches!(ws.update, UpdateState::Ready { .. }));
            })
            .unwrap();
    }

    #[gpui::test]
    fn no_install_without_a_known_install_dir(cx: &mut TestAppContext) {
        // Tests never locate an install (`init_updates` isn't called), so the
        // restart and the quit hook can't touch anything on disk.
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.update = UpdateState::Ready {
                    release: release("1.2.3"),
                    staged: staged("1.2.3"),
                };
                ws.install_on_quit();
                assert!(matches!(ws.update, UpdateState::Ready { .. }));
                ws.request_update_restart(window, cx);
                assert!(matches!(ws.update, UpdateState::Failed { .. }));
            })
            .unwrap();
    }

    #[gpui::test]
    fn restart_without_a_download_says_so(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.request_update_restart(window, cx);
                assert!(!ws.is_modal(Modal::ConfirmRestart));
                assert!(ws.status.contains("No update"));
            })
            .unwrap();
    }

    #[gpui::test]
    fn auto_update_toggle_is_saved_and_checks_nothing_in_a_dev_build(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                assert!(ws.settings.auto_update);
                ws.set_auto_update(false, cx);
                assert!(!ws.settings.auto_update);
                ws.set_auto_update(true, cx);
                assert!(ws.settings.auto_update);
                assert!(matches!(ws.update, UpdateState::Idle));
            })
            .unwrap();
    }
}
