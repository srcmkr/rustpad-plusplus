//! New versions: a check against GitHub's latest release 30 seconds after the
//! start and every 6 hours after that, and installing one, depending on how
//! rustpad++ is installed: from the Debian package or under `/usr/local`,
//! `rustpad --update` does it as root (downloading and checking the release
//! itself, see `manage`); an AppImage replaces its own file. A build run from
//! its own folder can be installed for all users. Then rustpad++ restarts
//! through its normal quit, so the session (and unsaved work) comes along.

use std::{
    io::Read as _,
    path::Path,
    process::{Command, Stdio},
    rc::Rc,
    time::Duration,
};

use gpui::{App, AppContext as _, Global, Hsla};
use rustpad_update::{
    Release,
    layout::{Install, LOCAL},
};

use crate::theme::{self, Settings};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const FIRST_CHECK: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Default)]
pub struct Updates {
    pub latest: Option<Release>,
    /// The last check got no answer.
    pub failed: bool,
    pub checking: bool,
    pub busy: bool,
    pub outcome: Option<Outcome>,
}

impl Global for Updates {}

/// Quits the main window the normal way (session, questions), for a restart.
pub struct Quitter(pub Rc<dyn Fn(&mut App)>);

impl Global for Quitter {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Replaces the installed rustpad++ with the latest signed release.
    Update,
    /// Installs this build for all users.
    InstallApp,
}

impl Action {
    fn argument(self) -> &'static str {
        match self {
            Action::Update => "--update",
            Action::InstallApp => "--install-app",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Outcome {
    Done,
    /// The password dialog was closed.
    Cancelled,
    /// Wrong password or not allowed.
    Refused,
    /// No way to ask for the password here; the command to run by hand.
    NoPrompt(String),
    Failed(String),
}

pub fn init(cx: &mut App) {
    cx.set_global(Updates::default());
    // RUSTPAD_UPDATE_DELAY=<seconds> shortens the first wait for testing.
    let first = std::env::var("RUSTPAD_UPDATE_DELAY").ok().and_then(|s| s.parse().ok()).map_or(FIRST_CHECK, Duration::from_secs);
    cx.spawn(async move |cx| {
        let mut wait = first;
        loop {
            cx.background_executor().timer(wait).await;
            wait = EVERY;
            if cx.update(|cx| Settings::get(cx).check_updates.then(|| check(cx))).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Whether this version runs for the first time after an update (not after a
/// new installation). Remembers this version, so it is true only once.
pub fn first_start_after_update(cx: &mut App) -> bool {
    let s = cx.global_mut::<Settings>();
    let previous = s.last_version.replace(VERSION.to_string());
    if previous.as_deref() != Some(VERSION) {
        s.save();
    }
    previous.is_some_and(|p| rustpad_update::is_newer(VERSION, &p))
}

/// Asks GitHub now, in the background.
pub fn check(cx: &mut App) {
    // Development aid: RUSTPAD_UPDATE_FAKE=<version> pretends GitHub offers
    // that version, without asking it.
    let fake = std::env::var("RUSTPAD_UPDATE_FAKE").ok();
    let task = cx.background_spawn(async move {
        match fake {
            Some(version) => Some(Release {
                page: format!("https://github.com/{}/releases", rustpad_update::REPOSITORY),
                version,
                assets: Vec::new(),
            }),
            None => rustpad_update::latest(),
        }
    });
    cx.global_mut::<Updates>().checking = true;
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        let latest = task.await;
        cx.update(|cx| {
            let updates = cx.global_mut::<Updates>();
            updates.checking = false;
            updates.failed = latest.is_none();
            if latest.is_some() {
                updates.latest = latest;
            }
            cx.refresh_windows();
        })
        .ok();
    })
    .detach();
}

/// A newer release than this one, skipped or not.
pub fn newer(cx: &App) -> Option<Release> {
    let latest = cx.try_global::<Updates>()?.latest.clone()?;
    rustpad_update::is_newer(&latest.version, VERSION).then_some(latest)
}

/// A newer release the user has not skipped.
pub fn available(cx: &App) -> Option<Release> {
    let skipped = Settings::get(cx).skipped_version.clone();
    newer(cx).filter(|r| skipped.as_deref() != Some(r.version.as_str()))
}

/// Never offers this version again; a later one is offered as usual.
pub fn skip(cx: &mut App) {
    let Some(release) = newer(cx) else { return };
    let s = cx.global_mut::<Settings>();
    s.skipped_version = Some(release.version);
    s.save();
    cx.refresh_windows();
}

/// Whether this rustpad++ can install an update: from the package, under
/// `/usr/local` or as an AppImage. A build run from its own folder only
/// points to the release page.
pub fn can_update() -> bool {
    Install::current() != Install::Build
}

/// Whether this is a build run from its own folder, which can be installed
/// for all users.
pub fn is_build() -> bool {
    Install::current() == Install::Build
}

/// One line on where things stand, and the color of the dot in front of it.
pub fn status(cx: &App) -> (String, Hsla) {
    let Some(state) = cx.try_global::<Updates>() else { return (String::new(), theme::text_faint()) };
    match newer(cx) {
        Some(release) => (tr!("Version {version} is available.", version = release.version), theme::error()),
        None if state.checking => (tr!("Checking…").to_string(), theme::text_faint()),
        None if state.latest.is_some() => (tr!("rustpad++ is up to date.").to_string(), gpui::rgb(0x3fb950).into()),
        None if state.failed => (tr!("Could not check for updates.").to_string(), theme::text_faint()),
        None if !Settings::get(cx).check_updates => (tr!("Not checked: the check is switched off.").to_string(), theme::text_faint()),
        None => (tr!("Checks shortly after the start.").to_string(), theme::text_faint()),
    }
}

/// What happened to the last install, if anything is worth saying.
pub fn note(cx: &App) -> Option<String> {
    let state = cx.try_global::<Updates>()?;
    match &state.outcome {
        _ if state.busy => Some(tr!("Installing the update…").to_string()),
        Some(Outcome::Refused) => Some(tr!("Not allowed, or wrong password.").to_string()),
        Some(Outcome::Failed(error)) => Some(tr!("The update did not work: {error}", error = error)),
        Some(Outcome::NoPrompt(command)) => {
            Some(format!("{} {command}", tr!("No password prompt is available here. Run this in a terminal:")))
        }
        _ => None,
    }
}

/// Installs the latest release (`Action::Update`) or this build for all
/// users (`Action::InstallApp`), then restarts from the installed copy.
pub fn install(action: Action, cx: &mut App) {
    if cx.global::<Updates>().busy {
        return;
    }
    let updates = cx.global_mut::<Updates>();
    updates.busy = true;
    updates.outcome = None;
    cx.refresh_windows();
    let task = cx.background_spawn(async move {
        match (action, Install::current()) {
            (Action::Update, Install::AppImage(image)) => match update_appimage(&image) {
                Ok(()) => Outcome::Done,
                Err(error) => Outcome::Failed(error),
            },
            _ => run_privileged(action),
        }
    });
    cx.spawn(async move |cx| {
        let outcome = task.await;
        cx.update(|cx| {
            let updates = cx.global_mut::<Updates>();
            updates.busy = false;
            match outcome {
                Outcome::Done => restart(cx),
                Outcome::Cancelled => {}
                other => updates.outcome = Some(other),
            }
            cx.refresh_windows();
        })
        .ok();
    })
    .detach();
}

/// Replaces an AppImage with the latest release's, if that is newer and
/// signed. The file belongs to the user, so no password is needed.
fn update_appimage(image: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let release = rustpad_update::latest().ok_or("no release information from GitHub")?;
    if !rustpad_update::is_newer(&release.version, VERSION) {
        return Err(format!("{} is not newer than {VERSION}", release.version));
    }
    let data = rustpad_update::fetch_verified(&release, &rustpad_update::appimage_name(&release.version))?;
    let new = image.with_extension("new");
    std::fs::write(&new, data).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&new, image).map_err(|e| e.to_string())
}

/// Runs `rustpad <action>` as root through pkexec: the installed program for
/// an update (the polkit rules name it), this build to install it. Blocks
/// until the password was given and the action is done.
fn run_privileged(action: Action) -> Outcome {
    let program = match action {
        Action::Update => Install::current().app(),
        Action::InstallApp => std::env::current_exe().ok(),
    };
    let Some(program) = program.filter(|p| p.is_file()) else {
        return Outcome::Failed("rustpad not found".into());
    };
    let by_hand = format!("sudo {} {}", program.display(), action.argument());
    let child = Command::new("pkexec")
        .arg(&program)
        .arg(action.argument())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else { return Outcome::NoPrompt(by_hand) };
    let mut errors = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut errors).ok();
    }
    let Ok(status) = child.wait() else { return Outcome::Failed("pkexec".into()) };
    match status.code() {
        Some(0) => Outcome::Done,
        Some(126) => Outcome::Cancelled,
        Some(127) if errors.contains("No authentication agent") => Outcome::NoPrompt(by_hand),
        Some(127) => Outcome::Refused,
        _ => Outcome::Failed(errors.lines().last().unwrap_or_default().trim_start_matches("rustpad: ").to_owned()),
    }
}

/// Starts the installed copy once this process has ended, then quits the
/// normal way. If quitting is called off (unsaved changes, "Cancel"), the new
/// copy starts whenever this one ends.
fn restart(cx: &mut App) {
    // The updated copy; after "Install for all users", the new one.
    let app = Install::current().app().unwrap_or_else(|| LOCAL.app.into());
    let waiter = Command::new("sh")
        .args(["-c", r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec "$0""#])
        .arg(&app)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match waiter {
        Ok(_) => match cx.try_global::<Quitter>().map(|q| q.0.clone()) {
            Some(quit) => quit(cx),
            None => cx.quit(),
        },
        Err(err) => eprintln!("rustpad: cannot start {}: {err}", app.display()),
    }
}
