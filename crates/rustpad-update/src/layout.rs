//! How rustpad++ is installed, and where its parts are then.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Where one installation keeps its files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paths {
    pub app: &'static str,
    pub desktop: &'static str,
    /// The `hicolor` icon theme the icons go into.
    pub icons: &'static str,
}

/// The Debian package.
pub const DEB: Paths = Paths {
    app: "/usr/bin/rustpad",
    desktop: "/usr/share/applications/rustpad.desktop",
    icons: "/usr/share/icons/hicolor",
};

/// Installed from the settings ("Install for all users") or by an update of
/// such an installation.
pub const LOCAL: Paths = Paths {
    app: "/usr/local/bin/rustpad",
    desktop: "/usr/local/share/applications/rustpad.desktop",
    icons: "/usr/local/share/icons/hicolor",
};

/// The polkit rules that let the installed program update itself.
pub const POLICY: &str = "/usr/share/polkit-1/actions/io.github.srcmkr.rustpad.policy";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// From the Debian package; apt owns the files.
    Deb,
    /// Under `/usr/local`.
    Local,
    /// An AppImage; its file is where it was put.
    AppImage(PathBuf),
    /// Run from where it was built.
    Build,
}

impl Install {
    /// How the running program is installed. Found out on the first call and
    /// kept: once an update has replaced the program, its path reads
    /// "/usr/bin/rustpad (deleted)" and no longer says where it came from.
    pub fn current() -> Self {
        static CURRENT: OnceLock<Install> = OnceLock::new();
        CURRENT.get_or_init(Self::detect).clone()
    }

    fn detect() -> Self {
        let exe = std::env::current_exe().and_then(|e| e.canonicalize()).unwrap_or_default();
        let var = |k| std::env::var_os(k).map(PathBuf::from);
        let appdir = var("APPDIR").and_then(|d| d.canonicalize().ok());
        match Self::image(&exe, var("APPIMAGE"), appdir) {
            Some(image) if image.is_file() => Install::AppImage(image),
            _ => Self::of(&exe),
        }
    }

    /// The AppImage file, if `exe` runs from inside its mount at `appdir`.
    /// A program started from another AppImage (a terminal, say) inherits
    /// that one's variables, and its own path lies outside the mount.
    fn image(exe: &Path, appimage: Option<PathBuf>, appdir: Option<PathBuf>) -> Option<PathBuf> {
        appdir.filter(|d| exe.starts_with(d)).and(appimage)
    }

    fn of(exe: &Path) -> Self {
        if exe == Path::new(DEB.app) {
            Install::Deb
        } else if exe == Path::new(LOCAL.app) {
            Install::Local
        } else {
            Install::Build
        }
    }

    /// The program to start, e.g. after an update.
    pub fn app(&self) -> Option<PathBuf> {
        match self {
            Install::Deb => Some(DEB.app.into()),
            Install::Local => Some(LOCAL.app.into()),
            Install::AppImage(image) => Some(image.clone()),
            Install::Build => None,
        }
    }

    /// Installed for all users: the system's menu entry and icons are there.
    pub fn is_system(&self) -> bool {
        matches!(self, Install::Deb | Install::Local)
    }
}

/// `@APP@` in a packaging template, for `paths`.
pub fn fill(template: &str, paths: &Paths) -> String {
    template.replace("@APP@", paths.app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_where_it_runs() {
        assert_eq!(Install::of(Path::new("/usr/bin/rustpad")), Install::Deb);
        assert_eq!(Install::of(Path::new("/usr/local/bin/rustpad")), Install::Local);
        assert_eq!(Install::of(Path::new("/home/me/rustpad++/target/release/rustpad")), Install::Build);
        assert_eq!(fill("Exec=@APP@ %F", &DEB), "Exec=/usr/bin/rustpad %F");
        assert!(Install::Local.is_system() && !Install::Build.is_system());
    }

    #[test]
    fn ignores_appimage_it_was_started_from() {
        let mount = || Some(PathBuf::from("/tmp/.mount_rustpaXYZ"));
        let image = || Some(PathBuf::from("/home/me/rustpad.AppImage"));
        assert_eq!(Install::image(Path::new("/tmp/.mount_rustpaXYZ/usr/bin/rustpad"), image(), mount()), image());
        assert_eq!(Install::image(Path::new("/usr/local/bin/rustpad"), image(), mount()), None);
        assert_eq!(Install::image(Path::new("/usr/local/bin/rustpad"), image(), None), None);
    }
}
