//! `rustpad --install-app` installs this build for all users, `--update`
//! replaces an installed rustpad++ with the latest signed release. Run as
//! root, normally through `pkexec` from the app, which asks for the password.
//!
//! Each action does one fixed thing; nothing here takes paths or other input
//! from the caller. From the Debian package, apt owns the files: the update
//! goes through apt.

use std::{fs, io, os::unix::fs::PermissionsExt, path::Path, process::Command};

use rustpad_update::{
    self as up,
    layout::{self, Install, LOCAL},
};

const POLICY_TEXT: &str = include_str!("../packaging/io.github.srcmkr.rustpad.policy");
/// Where the Debian package's update is put for apt.
const DOWNLOADS: &str = "/var/cache/rustpad";

/// Runs `action` if it is one; `None` for any other argument.
pub fn run(action: &str) -> Option<i32> {
    let result = match action {
        "--install-app" => install_app(),
        "--update" if Install::current() == Install::Deb => update_package(),
        "--update" => update(),
        _ => return None,
    };
    Some(match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("rustpad: {err}");
            1
        }
    })
}

fn require_root() -> io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "needs root"));
    }
    Ok(())
}

/// Writes `contents` to `path` through a temporary file, so a reader never
/// sees half of it.
fn write_file(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("new");
    fs::write(&tmp, contents)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

/// Installs this program, as it comes out of `cargo build`, for all users,
/// with a menu entry, icons and the rules for later updates.
fn install_app() -> io::Result<()> {
    require_root()?;
    let program = fs::read(std::env::current_exe()?)?;
    place(&program)
}

/// Writes the program (under a temporary name first, then renamed, so there
/// is never half a version), menu entry, icons and polkit rules.
fn place(program: &[u8]) -> io::Result<()> {
    write_file(Path::new(LOCAL.app), program, 0o755)?;
    let desktop = Path::new(LOCAL.desktop);
    write_file(desktop, crate::icon::desktop_entry(LOCAL.app).as_bytes(), 0o644)?;
    let hicolor = Path::new(LOCAL.icons);
    let icons_changed = crate::icon::install_icons(hicolor);
    crate::icon::refresh_caches(hicolor, icons_changed, desktop.parent().unwrap_or(desktop), true);
    write_file(Path::new(layout::POLICY), layout::fill(POLICY_TEXT, &LOCAL).as_bytes(), 0o644)
}

/// The latest release, if it is newer than this program.
fn newer_release() -> io::Result<up::Release> {
    let current = env!("CARGO_PKG_VERSION");
    let release = up::latest().ok_or_else(|| io::Error::other("no release information from GitHub"))?;
    if !up::is_newer(&release.version, current) {
        return Err(io::Error::other(format!("{} is not newer than {current}", release.version)));
    }
    Ok(release)
}

/// Replaces the rustpad++ under `/usr/local` with the latest release, if it
/// is newer and signed with the release key. Takes no input: it finds,
/// downloads and checks the release itself.
fn update() -> io::Result<()> {
    require_root()?;
    let release = newer_release()?;
    let archive = up::fetch_verified(&release, &up::archive_name(&release.version)).map_err(io::Error::other)?;
    let program = up::unpack(&archive).map_err(io::Error::other)?;
    place(&program)?;
    println!("{}", release.version);
    Ok(())
}

/// Installed from the Debian package: installs the latest release's package
/// with apt, if it is newer and signed with the release key.
fn update_package() -> io::Result<()> {
    require_root()?;
    let release = newer_release()?;
    let name = up::deb_name(&release.version);
    let package = up::fetch_verified(&release, &name).map_err(io::Error::other)?;
    // Readable for apt's download user, written by root only.
    fs::create_dir_all(DOWNLOADS)?;
    fs::set_permissions(DOWNLOADS, fs::Permissions::from_mode(0o755))?;
    let file = Path::new(DOWNLOADS).join(&name);
    write_file(&file, &package, 0o644)?;
    let status = Command::new("apt-get")
        .args(["install", "-y", "--no-install-recommends"])
        .arg(&file)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .status();
    fs::remove_file(&file).ok();
    if !status?.success() {
        return Err(io::Error::other("apt-get could not install the package"));
    }
    println!("{}", release.version);
    Ok(())
}
