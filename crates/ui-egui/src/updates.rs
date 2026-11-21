//! Native release checks and verified installation through craft-update's platform helpers.
use crate::i18n::{tr, trf};
use crate::widgets::{Button, ButtonKind};
use craft_update::pins::{Choice, Pins};
use craft_update::{App, Http, Paths, Platform, Release, Stop, self_update};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

pub const APP: App = App::new("craft-anything", "Craft Anything", "Build and investigate apps", "storytold/craft-anything");
/// The version running, as the window and Home's notice name it.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A pinned, nightly, or unknown launcher choice must never be replaced by a stable update.
fn stable_choice() -> Result<(), String> {
    if Pins::load_default().choice(APP.id) == Choice::Stable {
        Ok(())
    } else {
        Err(tr("ArtCraft Launcher manages this app's version. Change its version there."))
    }
}

/// Ask GitHub for a newer stable release, and whether this installation can install it itself.
/// A launcher choice other than stable ends the check before any request.
fn release_check() -> (Result<Option<Release>, String>, bool) {
    let result = stable_choice()
        .and_then(|()| Http::new(concat!("Craft Anything/", env!("CARGO_PKG_VERSION"))))
        .and_then(|http| craft_update::check_for_update(&APP, &http, VERSION));
    let installable = result
        .as_ref()
        .ok()
        .and_then(|release| release.as_ref())
        .is_some_and(|release| self_update::is_self_updating(&APP) && self_update::release_installs(&APP, release));
    (result, installable)
}

enum Reply {
    Checked(Result<Option<Release>, String>, bool),
    Installed(Result<self_update::Restart, String>),
}
#[derive(Default)]
pub struct Updates {
    open: bool,
    read_outcome_requested: bool,
    message: String,
    release: Option<Release>,
    installable: bool,
    worker: Option<Receiver<Reply>>,
    installing: bool,
    restart: Option<self_update::Restart>,
    /// The first desktop frame starts a check that opens nothing.
    check_requested: bool,
    /// Home's notice was dismissed for this session.
    later: bool,
}
impl Updates {
    /// Read the previous update result when the first desktop frame can show it.
    pub fn startup(&mut self) {
        self.read_outcome_requested = true;
    }

    /// Run the manual check when the first desktop frame is drawn, without opening its window:
    /// Home offers a release that is found ([`Self::offered`]), and any other answer says nothing.
    pub fn check_at_start(&mut self) {
        self.check_requested = true;
    }

    /// Start a manual check off the UI thread; repeated requests share the running check.
    pub fn check(&mut self, ctx: &egui::Context) {
        self.check_using(ctx, release_check);
    }

    fn check_using(&mut self, ctx: &egui::Context, check: impl FnOnce() -> (Result<Option<Release>, String>, bool) + Send + 'static) {
        self.open = true;
        self.begin(ctx, check);
    }

    /// Start `check` on a worker unless one is running. The window stays as it is.
    fn begin(&mut self, ctx: &egui::Context, check: impl FnOnce() -> (Result<Option<Release>, String>, bool) + Send + 'static) {
        if self.worker.is_some() {
            return;
        }
        self.message = tr("Checking for updates…");
        self.release = None;
        self.installable = false;
        let (tx, rx) = channel();
        self.worker = Some(rx);
        let ctx = ctx.clone();
        let result = std::thread::Builder::new().name("release-check".into()).spawn(move || {
            let (result, installable) = check();
            let _ = tx.send(Reply::Checked(result, installable));
            ctx.request_repaint();
        });
        if let Err(error) = result {
            self.worker = None;
            self.message = error.to_string();
        }
    }
    fn install(&mut self, ctx: &egui::Context, release: Release) {
        self.install_using(ctx, move || {
            let install =
                self_update::current(&APP).ok_or(tr("This installation is managed externally. Update it with its package manager or open Releases."))?;
            let platform = Platform::current().ok_or(tr("No installer is available for this platform."))?;
            let http = Http::new(concat!("Craft Anything/", env!("CARGO_PKG_VERSION")))?;
            let paths = Paths::for_app(&APP)?;
            self_update::prepare_update(&APP, &install, &release, platform, &http, &paths, &Stop::default(), &mut |_| {}, &stable_choice, Some(VERSION))
        });
    }

    fn install_using(&mut self, ctx: &egui::Context, install: impl FnOnce() -> Result<self_update::Restart, String> + Send + 'static) {
        self.installing = true;
        self.message = tr("Downloading and verifying the update…");
        let (tx, rx) = channel();
        self.worker = Some(rx);
        let ctx = ctx.clone();
        let result = std::thread::Builder::new().name("release-install".into()).spawn(move || {
            let result = install();
            let _ = tx.send(Reply::Installed(result));
            ctx.request_repaint();
        });
        if let Err(error) = result {
            self.worker = None;
            self.installing = false;
            self.message = error.to_string();
        }
    }
    /// Returns true while installing: the caller pauses editing and starting work until restart.
    pub fn show(&mut self, ctx: &egui::Context, safe_to_install: bool) -> bool {
        if std::mem::take(&mut self.read_outcome_requested)
            && let Ok(paths) = Paths::for_app(&APP)
        {
            self.apply_outcome(self_update::take_outcome(&paths, VERSION));
        }
        self.start_requested_check(ctx, release_check);
        let reply = self.worker.as_ref().map(Receiver::try_recv);
        match reply {
            Some(Ok(Reply::Checked(result, installable))) => {
                self.installable = installable;
                self.worker = None;
                self.checked(result);
            }
            Some(Ok(Reply::Installed(result))) => {
                self.worker = None;
                match result {
                    Ok(restart) => {
                        self.restart = Some(restart);
                        self.message = tr("Update ready. Restarting…");
                    }
                    Err(error) => {
                        self.installing = false;
                        self.message = error;
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.worker = None;
                self.installing = false;
                self.message = tr("The update worker stopped unexpectedly. Try checking again.");
            }
            _ => {}
        }
        if self.restart.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self.installing && ctx.input(|i| i.viewport().close_requested()) {
            // The platform helper may be replacing files. Keep this process alive until it finishes.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.worker.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.open {
            let mut open = true;
            let mut install = false;
            let mut again = false;
            egui::Window::new(tr("Check for Updates"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .default_width(440.0)
                .max_width((ctx.content_rect().width() - 32.0).max(160.0))
                .show(ctx, |ui| {
                    ui.label(format!("Craft Anything {VERSION}"));
                    ui.label(&self.message);
                    if !safe_to_install && self.release.is_some() {
                        ui.label(tr("Save your work and stop running tasks before installing an update."));
                    }
                    if self.release.is_some() && !self.installable {
                        ui.label(tr("Use Releases or your package manager to update this installation."));
                    }
                    ui.horizontal(|ui| {
                        if let Some(release) = &self.release {
                            ui.hyperlink_to(tr("Releases"), &release.html_url);
                            let ready = safe_to_install && self.installable && self.worker.is_none() && self.restart.is_none();
                            install = ui.add(Button::new(ButtonKind::Primary, "Update and restart").enabled(ready)).clicked();
                        } else {
                            ui.hyperlink_to(tr("Releases"), APP.releases_url());
                        }
                        again = ui.add(Button::new(ButtonKind::Secondary, "Check again").enabled(self.worker.is_none() && !self.installing)).clicked();
                    });
                });
            self.open = open || self.installing;
            if install && let Some(release) = self.release.clone() {
                self.install(ctx, release);
            }
            if again {
                self.check(ctx);
            }
        }
        self.installing
    }
    /// The check at start, once. The last update's result, when its window is open, is left on
    /// screen: "Check again" is there.
    fn start_requested_check(&mut self, ctx: &egui::Context, check: impl FnOnce() -> (Result<Option<Release>, String>, bool) + Send + 'static) {
        if std::mem::take(&mut self.check_requested) && !self.open {
            self.begin(ctx, check);
        }
    }

    /// The newer release the last check found, for Home's notice, until the person says "Later".
    pub fn offered(&self) -> Option<&Release> {
        self.release.as_ref().filter(|_| !self.later)
    }

    /// Open the window on what the last check found: the release, and installing it.
    pub fn open(&mut self) {
        self.open = true;
    }

    /// Hide Home's notice for the rest of this session.
    pub fn later(&mut self) {
        self.later = true;
    }

    fn apply_outcome(&mut self, outcome: Option<self_update::Outcome>) {
        match outcome {
            Some(self_update::Outcome::Installed(version)) => {
                self.open = true;
                self.message = trf("Updated to {version}.", &[("version", &version.to_string())]);
            }
            Some(self_update::Outcome::Failed(error)) => {
                self.open = true;
                self.message = trf("The update failed: {error}", &[("error", &error)]);
            }
            None => {}
        }
    }

    fn checked(&mut self, result: Result<Option<Release>, String>) {
        match result {
            Ok(Some(release)) => {
                self.message = trf("{version} is available.", &[("version", &release.version_label())]);
                self.release = Some(release);
            }
            Ok(None) => {
                self.message = tr("You're up to date.");
                self.release = None;
            }
            Err(error) => {
                self.message = trf("Couldn't check for updates: {error}", &[("error", &error)]);
                self.release = None;
            }
        }
    }
    /// Start the prepared helper only after the app's normal exit path has accepted closing.
    pub fn finish(&mut self) {
        if let Some(restart) = self.restart.take()
            && let Err(error) = restart.start()
        {
            eprintln!("Couldn't restart after updating: {error}");
        }
    }
}

/// Runs the Windows installation helper before opening a normal application window.
pub fn finish_helper() -> Option<Result<(), String>> {
    self_update::finish(&APP, &std::env::args_os().skip(1).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(updates: &mut Updates, safe: bool, close: bool) -> egui::FullOutput {
        // The app's fonts, as its own context has them (they apply from the second pass): the
        // window's buttons are set in them.
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        let mut input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 400.0))), ..Default::default() };
        if close {
            input.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
        }
        let mut output = ctx.run_ui(input, |ui| {
            updates.show(ui.ctx(), safe);
        });
        output.textures_delta.clear();
        output
    }

    #[test]
    fn a_launcher_pin_stops_the_real_check_before_network_access() {
        const CHILD: &str = "CRAFT_UPDATE_PIN_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert!(stable_choice().is_err());
            let mut updates = Updates::default();
            updates.check(&egui::Context::default());
            let reply =
                updates.worker.as_ref().expect("check worker").recv_timeout(std::time::Duration::from_secs(5)).expect("pin check finishes without network");
            let (tx, rx) = channel();
            tx.send(reply).expect("receiver");
            updates.worker = Some(rx);
            draw(&mut updates, true, false);
            assert!(updates.message.contains("ArtCraft Launcher"));
            assert!(updates.release.is_none());
            assert!(!updates.installable);
            assert!(finish_helper().is_none());
            // The check at start is the same check: the pin ends it before any request, and
            // Home is offered nothing.
            let mut at_start = Updates::default();
            at_start.check_at_start();
            draw(&mut at_start, true, false);
            let reply = at_start
                .worker
                .as_ref()
                .expect("start check worker")
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("pin check finishes without network");
            let (tx, rx) = channel();
            tx.send(reply).expect("receiver");
            at_start.worker = Some(rx);
            draw(&mut at_start, true, false);
            assert!(at_start.message.contains("ArtCraft Launcher"));
            assert!(at_start.offered().is_none());
            assert!(!at_start.open, "a check nobody asked for opens no window");
            #[cfg(target_os = "linux")]
            {
                // This child has an isolated XDG data folder; never consume a user's update record.
                let paths = Paths::for_app(&APP).expect("isolated app folders");
                std::fs::create_dir_all(paths.logs()).expect("test logs");
                let failure = paths.logs().join("self-update-error.txt");
                std::fs::write(&failure, "test installer failure").expect("test failure record");
                let mut restarted = Updates::default();
                restarted.startup();
                draw(&mut restarted, true, false);
                assert!(restarted.open);
                assert!(restarted.message.contains("test installer failure"));
                assert!(!failure.exists(), "the helper result is consumed exactly once");
                restarted.open = false;
                restarted.startup();
                draw(&mut restarted, true, false);
                assert!(!restarted.open, "the old failure must not reappear on the next start");
            }

            return;
        }
        // Keep the process environment and the user's actual version choices untouched.
        let dir = std::env::temp_dir().join(format!("{}-updater-pin-test-{}", APP.id, std::process::id()));
        std::fs::create_dir_all(&dir).expect("test folder");
        let path = dir.join("versions.json");
        std::fs::write(&path, format!(r#"{{"apps":{{"{}":{{"Pinned":"0.1.0"}}}}}}"#, APP.id)).expect("test pin");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "updates::tests::a_launcher_pin_stops_the_real_check_before_network_access"])
            .env(CHILD, "1")
            .env(craft_update::pins::PATH_ENV, &path)
            .env("XDG_DATA_HOME", dir.join("data"))
            .status()
            .expect("isolated pin test");
        std::fs::remove_dir_all(dir).expect("remove test folder");
        assert!(status.success());
    }
    #[test]
    fn available_release_is_shown_but_never_installed_without_confirmation() {
        let release = Release { tag: "v99.0.0".into(), html_url: APP.releases_url(), ..Release::default() };
        let (tx, rx) = channel();
        tx.send(Reply::Checked(Ok(Some(release)), false)).expect("receiver is alive");
        let mut updates = Updates { open: true, worker: Some(rx), ..Updates::default() };
        let output = draw(&mut updates, false, false);
        assert!(updates.message.contains("99.0.0"));
        assert!(!output.shapes.is_empty(), "the result window must be rendered");
        assert!(updates.worker.is_none());
        assert!(!updates.installing);
        assert!(!updates.installable, "externally managed installs must only offer Releases");
        updates.installable = true;
        draw(&mut updates, true, false);
        assert!(!updates.installing, "an available update alone must never install");
    }

    #[test]
    fn checks_run_off_thread_and_repeated_requests_share_the_worker() {
        let ctx = egui::Context::default();
        let (started, wait) = channel();
        let mut updates = Updates::default();
        updates.check_using(&ctx, move || {
            wait.recv().expect("release test worker");
            (Ok(None), false)
        });
        assert!(updates.worker.is_some());
        updates.check_using(&ctx, || panic!("must not start a second check"));
        draw(&mut updates, false, false);
        started.send(()).expect("worker waiting");
        let reply = updates.worker.as_ref().expect("worker").recv_timeout(std::time::Duration::from_secs(5)).expect("worker finishes");
        let (tx, rx) = channel();
        tx.send(reply).expect("receiver");
        updates.worker = Some(rx);
        draw(&mut updates, true, false);
        assert_eq!(updates.message, "You're up to date.");
        assert!(updates.worker.is_none());
    }

    #[test]
    fn an_install_failure_unlocks_editing_and_close_is_cancelled_until_it_finishes() {
        let ctx = egui::Context::default();
        let (finish, wait) = channel();
        let mut updates = Updates { open: true, ..Updates::default() };
        updates.install_using(&ctx, move || {
            wait.recv().expect("release test worker");
            Err("signature verification failed".into())
        });
        assert!(updates.installing);
        let output = draw(&mut updates, false, true);
        assert!(output.viewport_output.values().any(|v| v.commands.contains(&egui::ViewportCommand::CancelClose)));
        finish.send(()).expect("worker waiting");
        let reply = updates.worker.as_ref().expect("worker").recv_timeout(std::time::Duration::from_secs(5)).expect("worker finishes");
        let (tx, rx) = channel();
        tx.send(reply).expect("receiver");
        updates.worker = Some(rx);
        draw(&mut updates, false, false);
        assert!(!updates.installing);
        assert!(updates.worker.is_none());
        assert!(updates.restart.is_none(), "failed verification must never schedule a restart");
        assert!(updates.message.contains("signature verification failed"));
        updates.finish();
        updates.apply_outcome(None);
    }
    /// Runs the check at start with `answer` in place of the network, and takes its reply.
    fn checked_at_start(updates: &mut Updates, answer: Result<Option<Release>, String>) {
        updates.check_at_start();
        updates.start_requested_check(&egui::Context::default(), move || (answer, false));
        let reply = updates.worker.as_ref().expect("start check worker").recv_timeout(std::time::Duration::from_secs(5)).expect("worker finishes");
        let (tx, rx) = channel();
        tx.send(reply).expect("receiver");
        updates.worker = Some(rx);
        draw(updates, true, false);
    }

    fn newer_release() -> Release {
        Release { tag: "v99.0.0".into(), version: craft_update::version::parse("99.0.0"), html_url: APP.releases_url(), ..Release::default() }
    }

    #[test]
    fn the_check_at_start_opens_nothing_and_offers_what_it_finds() {
        let mut updates = Updates::default();
        checked_at_start(&mut updates, Ok(Some(newer_release())));
        assert!(!updates.open, "only Home says so");
        assert_eq!(updates.offered().map(Release::version_label).as_deref(), Some("99.0.0"));
        updates.later();
        assert!(updates.offered().is_none(), "Later hides the notice");
        assert!(updates.release.is_some(), "and Help ▸ Check for Updates still has the release");

        let mut failed = Updates::default();
        checked_at_start(&mut failed, Err("network unavailable".into()));
        assert!(!failed.open, "a failed check nobody asked for says nothing");
        assert!(failed.offered().is_none());

        let mut current = Updates::default();
        checked_at_start(&mut current, Ok(None));
        assert!(!current.open);
        assert!(current.offered().is_none());
    }

    #[test]
    fn the_check_at_start_runs_once_and_leaves_the_last_updates_result_on_screen() {
        let ctx = egui::Context::default();
        let mut updates = Updates::default();
        updates.start_requested_check(&ctx, || panic!("no check was asked for"));
        assert!(updates.worker.is_none());

        updates.check_at_start();
        updates.apply_outcome(Some(self_update::Outcome::Failed("installer refused the update".into())));
        updates.start_requested_check(&ctx, || panic!("the result window is open"));
        assert!(updates.worker.is_none());
        assert!(updates.message.contains("installer refused the update"));
        updates.open = false;
        updates.start_requested_check(&ctx, || panic!("the request was for the first frame only"));
        assert!(updates.worker.is_none());
    }

    #[test]
    fn home_offers_a_found_release_and_later_hides_it() {
        use egui_kittest::kittest::Queryable as _;
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1280.0, 900.0)).with_max_steps(32).build_eframe(|cc| {
            crate::CraftApp::setup_context(&cc.egui_ctx);
            let mut app = crate::demo::app(crate::demo::engine(craft_anything_engine::Mode::Beginner, true));
            crate::demo::fill_project(&mut app);
            app.state.screen = crate::state::Screen::Home;
            app
        });
        harness.run_steps(3);
        assert!(harness.query_by_label("Get it").is_none(), "no newer release, no notice");

        harness.state_mut().updates.checked(Ok(Some(newer_release())));
        harness.run_steps(3);
        harness.get_by_label_contains(&format!("Craft Anything 99.0.0 is available (you have {VERSION})"));
        harness.get_by_label("Get it").click();
        harness.run_steps(3);
        assert!(harness.state().updates.open, "Get it opens the window that installs the release");
        harness.get_by_label("Update and restart");

        harness.get_by_label("Later").click();
        harness.run_steps(3);
        assert!(harness.query_by_label_contains("is available (you have").is_none(), "Later hides the notice for the session");
        assert!(harness.query_by_label("Get it").is_none());
    }

    #[test]
    fn a_failed_helper_is_reported_when_the_old_app_restarts() {
        let mut updates = Updates::default();
        updates.apply_outcome(Some(self_update::Outcome::Failed("installer refused the update".into())));
        assert!(updates.open);
        assert!(updates.message.contains("installer refused the update"));
        assert!(updates.worker.is_none());
        assert!(!updates.installing);
    }

    #[test]
    fn a_successful_restart_reports_the_installed_version() {
        let mut updates = Updates::default();
        let version = craft_update::version::parse("1.2.3").expect("valid test version");
        updates.apply_outcome(Some(self_update::Outcome::Installed(version)));
        assert!(updates.open);
        assert!(updates.message.contains("1.2.3"));
    }

    #[test]
    fn a_manual_check_reports_success_and_failure() {
        let mut updates = Updates::default();
        updates.checked(Ok(None));
        assert_eq!(updates.message, "You're up to date.");
        updates.checked(Err("network unavailable".into()));
        assert!(updates.message.contains("network unavailable"));
        assert!(updates.release.is_none());
    }
    #[test]
    fn a_dropped_worker_reports_failure_and_unblocks_the_app() {
        let (tx, rx) = channel();
        drop(tx);
        let mut updates = Updates { worker: Some(rx), installing: true, ..Updates::default() };
        assert!(!updates.show(&egui::Context::default(), false));
        assert!(updates.message.contains("stopped unexpectedly"));
    }
    #[test]
    fn the_manual_result_window_is_visible_without_network() -> Result<(), String> {
        use egui_kittest::kittest::Queryable as _;
        let mut updates = Updates { open: true, message: "You're up to date.".into(), ..Updates::default() };
        let mut first = true;
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(640.0, 360.0)).build_ui(|ui| {
            if std::mem::take(&mut first) {
                crate::theme::install_fonts(ui.ctx());
                ui.ctx().request_repaint();
                return;
            }
            updates.show(ui.ctx(), true);
        });
        harness.run();
        harness.get_by_label("You're up to date.");
        harness.get_by_label("Check again");
        if let Some(path) = std::env::var_os("CRAFT_UPDATE_SCREENSHOT") {
            harness.render()?.save(path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
