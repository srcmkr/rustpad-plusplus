//! Colors, fonts and user settings.
//!
//! Colors are plain functions reading the active palette through an atomic
//! index, so views can call them anywhere without a context and switching
//! the scheme is a single store plus a repaint.

use std::sync::atomic::{AtomicUsize, Ordering};

use gpui::{App, Font, FontFallbacks, Global, Hsla, Pixels, SharedString, px, rgb, rgb_to_hsla, rgba};

use crate::config::Config;

pub struct Settings {
    pub mono_family: SharedString,
    pub font_size: Pixels,
    pub line_numbers: bool,
    /// Wrap long lines at the window width instead of scrolling horizontally.
    pub word_wrap: bool,
    pub status_bar: bool,
    pub single_instance: bool,
    pub restore_session: bool,
    /// UI language: "auto" (from the environment) or a code like "de".
    pub language: String,
    /// Ask GitHub for new releases.
    pub check_updates: bool,
    /// A release the user does not want to be offered.
    pub skipped_version: Option<String>,
    /// The version that ran last, to show the changelog once after an update.
    pub last_version: Option<String>,
    /// Last main window placement.
    pub window: Option<gpui::WindowBounds>,
    pub recent: Vec<std::path::PathBuf>,
}

impl Global for Settings {}

pub const MAX_RECENT: usize = 5;

/// `windowed|maximized|fullscreen,x,y,w,h`
fn parse_window(v: &str) -> Option<gpui::WindowBounds> {
    let mut parts = v.split(',');
    let state = parts.next()?;
    let n: Vec<f32> = parts.map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let [x, y, w, h] = n[..] else { return None };
    if w < 200.0 || h < 120.0 {
        return None;
    }
    let b = gpui::Bounds::new(gpui::point(px(x), px(y)), gpui::size(px(w), px(h)));
    Some(match state {
        "maximized" => gpui::WindowBounds::Maximized(b),
        "fullscreen" => gpui::WindowBounds::Fullscreen(b),
        _ => gpui::WindowBounds::Windowed(b),
    })
}

fn format_window(w: &gpui::WindowBounds) -> String {
    let (state, b) = match w {
        gpui::WindowBounds::Windowed(b) => ("windowed", b),
        gpui::WindowBounds::Maximized(b) => ("maximized", b),
        gpui::WindowBounds::Fullscreen(b) => ("fullscreen", b),
    };
    format!(
        "{state},{},{},{},{}",
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height)
    )
}
pub const DEFAULT_FONT_SIZE: f32 = 14.0;

impl Settings {
    pub fn get(cx: &App) -> &Settings {
        cx.global::<Settings>()
    }

    pub fn from_config(c: &Config) -> Settings {
        // Names before the UI became translatable.
        let theme = c.get("theme").map(|n| match n {
            "Dunkel" => "Dark",
            "Hell" => "Light",
            n => n,
        });
        if let Some(i) = theme.and_then(|n| SCHEMES.iter().position(|s| s.name == n)) {
            set_scheme(i);
        }
        let language = c.get("language").filter(|l| !l.is_empty()).unwrap_or("auto").to_string();
        crate::i18n::set_language(&language);
        if let Some(w) = c.get("tab_width").and_then(|v| v.parse::<usize>().ok()) {
            crate::doc::rows::set_tab_width(w);
        }
        Settings {
            mono_family: c.get("font_family").unwrap_or("DejaVu Sans Mono").to_string().into(),
            font_size: px(c.get("font_size").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_FONT_SIZE).clamp(7.0, 48.0)),
            line_numbers: c.get("line_numbers") != Some("false"),
            word_wrap: c.get("word_wrap") == Some("true"),
            status_bar: c.get("status_bar") != Some("false"),
            single_instance: c.get("single_instance") != Some("false"),
            restore_session: c.get("restore_session") != Some("false"),
            language,
            check_updates: c.get("check_updates") != Some("false"),
            skipped_version: c.get("skipped_version").filter(|v| !v.is_empty()).map(str::to_string),
            last_version: c.get("last_version").filter(|v| !v.is_empty()).map(str::to_string),
            window: c.get("window").and_then(parse_window),
            recent: c.recent.clone(),
        }
    }

    /// Write the current settings to disk.
    pub fn save(&self) {
        let mut c = Config::default();
        c.set("theme", scheme().name);
        c.set("font_family", &self.mono_family);
        c.set("font_size", &f32::from(self.font_size).to_string());
        c.set("tab_width", &crate::doc::rows::tab_width().to_string());
        c.set("line_numbers", if self.line_numbers { "true" } else { "false" });
        c.set("word_wrap", if self.word_wrap { "true" } else { "false" });
        c.set("status_bar", if self.status_bar { "true" } else { "false" });
        c.set("single_instance", if self.single_instance { "true" } else { "false" });
        c.set("restore_session", if self.restore_session { "true" } else { "false" });
        c.set("language", &self.language);
        c.set("check_updates", if self.check_updates { "true" } else { "false" });
        if let Some(v) = &self.skipped_version {
            c.set("skipped_version", v);
        }
        if let Some(v) = &self.last_version {
            c.set("last_version", v);
        }
        if let Some(w) = &self.window {
            c.set("window", &format_window(w));
        }
        c.recent = self.recent.clone();
        c.save();
    }

    pub fn add_recent(&mut self, path: &std::path::Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(MAX_RECENT);
    }

    pub fn mono(&self) -> Font {
        let mut f = gpui::font(self.mono_family.clone());
        f.fallbacks = Some(FontFallbacks::from_fonts(vec![
            "DejaVu Sans Mono".into(),
            "Noto Sans Mono".into(),
            "Liberation Mono".into(),
            "Noto Color Emoji".into(),
        ]));
        f
    }

    pub fn line_height(&self) -> Pixels {
        (self.font_size * 1.45).round()
    }
}

macro_rules! palette {
    (opaque { $($name:ident,)* } alpha { $($aname:ident,)* }) => {
        pub struct Scheme {
            /// English; stored in settings.conf and translated for display.
            pub name: &'static str,
            pub dark: bool,
            $($name: u32,)*
            $($aname: u32,)*
        }
        $(#[allow(dead_code)] pub fn $name() -> Hsla { rgb_to_hsla(rgb(scheme().$name)) })*
        $(#[allow(dead_code)] pub fn $aname() -> Hsla { rgb_to_hsla(rgba(scheme().$aname)) })*
    };
}

palette! {
    opaque {
        bg, bg_alt, bg_panel, bg_hover, bg_active, border, text, text_dim, text_faint, accent,
        gutter_text, gutter_active, cursor, syn_key, syn_string, syn_number, syn_keyword,
        syn_punct, syn_heading, syn_offset, warn, error, primary, on_primary,
    }
    alpha {
        selection, current_line, find_match, scrollbar_thumb, scrollbar_thumb_active, shadow,
        glass, glass_border, control, control_hover, control_active,
    }
}

pub static SCHEMES: [Scheme; 5] = [
    Scheme {
        name: n_!("Dark"),
        dark: true,
        bg: 0x1e1f22,
        bg_alt: 0x2b2d30,
        bg_panel: 0x252629,
        bg_hover: 0x393b40,
        bg_active: 0x43454a,
        border: 0x393b40,
        text: 0xd4d6db,
        text_dim: 0x868a91,
        text_faint: 0x5a5d63,
        accent: 0x4c8dff,
        gutter_text: 0x5f636a,
        gutter_active: 0xa9adb5,
        cursor: 0xe8e9ec,
        syn_key: 0x9cdcfe,
        syn_string: 0xce9178,
        syn_number: 0xb5cea8,
        syn_keyword: 0x569cd6,
        syn_punct: 0x9a9ea5,
        syn_heading: 0x6cb6ff,
        syn_offset: 0x7d8590,
        warn: 0xe5c07b,
        error: 0xf07178,
        primary: 0x4c8dff,
        on_primary: 0xffffff,
        selection: 0x2f65caaa,
        current_line: 0xffffff0a,
        find_match: 0xe5c07b55,
        scrollbar_thumb: 0xffffff30,
        scrollbar_thumb_active: 0xffffff55,
        shadow: 0x00000080,
        glass: 0x2b2d30eb,
        glass_border: 0xffffff17,
        control: 0xffffff0d,
        control_hover: 0xffffff17,
        control_active: 0xffffff21,
    },
    Scheme {
        name: n_!("Light"),
        dark: false,
        bg: 0xffffff,
        bg_alt: 0xf0f0f0,
        bg_panel: 0xf6f6f6,
        bg_hover: 0xe2e6ec,
        bg_active: 0xd5dbe3,
        border: 0xd6d6d6,
        text: 0x1f2328,
        text_dim: 0x5f6670,
        text_faint: 0xa0a6ad,
        accent: 0x2f6fe0,
        gutter_text: 0x9aa0a6,
        gutter_active: 0x3c4043,
        cursor: 0x111111,
        syn_key: 0x0451a5,
        syn_string: 0xa31515,
        syn_number: 0x098658,
        syn_keyword: 0x0000ff,
        syn_punct: 0x6b7280,
        syn_heading: 0x1d4ed8,
        syn_offset: 0x8a8f98,
        warn: 0xb26b00,
        error: 0xc62828,
        primary: 0x2f6fe0,
        on_primary: 0xffffff,
        selection: 0x3d7eff40,
        current_line: 0x0000000a,
        find_match: 0xffd33d66,
        scrollbar_thumb: 0x00000030,
        scrollbar_thumb_active: 0x00000055,
        shadow: 0x00000040,
        glass: 0xfafafaeb,
        glass_border: 0x0000001a,
        control: 0x0000000a,
        control_hover: 0x00000012,
        control_active: 0x0000001c,
    },
    Scheme {
        name: "Monokai",
        dark: true,
        bg: 0x272822,
        bg_alt: 0x1e1f1c,
        bg_panel: 0x2f3029,
        bg_hover: 0x3e3d32,
        bg_active: 0x49483e,
        border: 0x3e3d32,
        text: 0xf8f8f2,
        text_dim: 0x9d9a87,
        text_faint: 0x75715e,
        accent: 0xa6e22e,
        gutter_text: 0x75715e,
        gutter_active: 0xd0d0c0,
        cursor: 0xf8f8f0,
        syn_key: 0x66d9ef,
        syn_string: 0xe6db74,
        syn_number: 0xae81ff,
        syn_keyword: 0xf92672,
        syn_punct: 0xa8a89a,
        syn_heading: 0xa6e22e,
        syn_offset: 0x8f8b77,
        warn: 0xfd971f,
        error: 0xf92672,
        primary: 0xa6e22e,
        on_primary: 0x1e1f1c,
        selection: 0x49483ecc,
        current_line: 0xffffff0c,
        find_match: 0xe6db7455,
        scrollbar_thumb: 0xffffff30,
        scrollbar_thumb_active: 0xffffff55,
        shadow: 0x00000080,
        glass: 0x3a3b33eb,
        glass_border: 0xffffff17,
        control: 0xffffff0d,
        control_hover: 0xffffff17,
        control_active: 0xffffff21,
    },
    Scheme {
        name: "Solarized Dark",
        dark: true,
        bg: 0x002b36,
        bg_alt: 0x073642,
        bg_panel: 0x05313d,
        bg_hover: 0x0d4553,
        bg_active: 0x15505f,
        border: 0x0d4553,
        text: 0xc5ccc4,
        text_dim: 0x839496,
        text_faint: 0x586e75,
        accent: 0x268bd2,
        gutter_text: 0x586e75,
        gutter_active: 0x93a1a1,
        cursor: 0xeee8d5,
        syn_key: 0x268bd2,
        syn_string: 0x2aa198,
        syn_number: 0xd33682,
        syn_keyword: 0x859900,
        syn_punct: 0x839496,
        syn_heading: 0xb58900,
        syn_offset: 0x657b83,
        warn: 0xb58900,
        error: 0xdc322f,
        primary: 0x268bd2,
        on_primary: 0xffffff,
        selection: 0x268bd255,
        current_line: 0xffffff0a,
        find_match: 0xb5890066,
        scrollbar_thumb: 0xffffff30,
        scrollbar_thumb_active: 0xffffff55,
        shadow: 0x00000080,
        glass: 0x0a3c48eb,
        glass_border: 0xffffff17,
        control: 0xffffff0d,
        control_hover: 0xffffff17,
        control_active: 0xffffff21,
    },
    Scheme {
        name: "Aurora",
        dark: true,
        bg: 0x111114,
        bg_alt: 0x0a0a0c,
        bg_panel: 0x161618,
        bg_hover: 0x26262b,
        bg_active: 0x2e2e34,
        border: 0x1c1c20,
        text: 0xf2f2f4,
        text_dim: 0xa1a1aa,
        text_faint: 0x6b6b74,
        accent: 0x3b82f6,
        gutter_text: 0x4a4a52,
        gutter_active: 0xa1a1aa,
        cursor: 0xf2f2f4,
        syn_key: 0xa78bfa,
        syn_string: 0x5eead4,
        syn_number: 0x93c5fd,
        syn_keyword: 0x818cf8,
        syn_punct: 0x71717a,
        syn_heading: 0xa78bfa,
        syn_offset: 0x6b6b74,
        warn: 0xfbbf24,
        error: 0xf87171,
        primary: 0xf2f2f4,
        on_primary: 0x0a0a0c,
        selection: 0x4f46e566,
        current_line: 0xffffff08,
        find_match: 0xfbbf2440,
        scrollbar_thumb: 0xffffff26,
        scrollbar_thumb_active: 0xffffff45,
        shadow: 0x000000a0,
        glass: 0x1e1e22e0,
        glass_border: 0xffffff1a,
        control: 0xffffff14,
        control_hover: 0xffffff1f,
        control_active: 0xffffff24,
    },
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);

pub fn scheme() -> &'static Scheme {
    &SCHEMES[CURRENT.load(Ordering::Relaxed)]
}

pub fn scheme_index() -> usize {
    CURRENT.load(Ordering::Relaxed)
}

pub fn set_scheme(i: usize) {
    CURRENT.store(i.min(SCHEMES.len() - 1), Ordering::Relaxed);
}


impl Scheme {
    /// Background plus three sample colors for scheme previews.
    pub fn preview(&self) -> [u32; 4] {
        [self.bg, self.syn_keyword, self.text, self.syn_string]
    }
}
