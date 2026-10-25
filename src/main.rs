#[macro_use]
mod i18n;
mod assets;
mod config;
mod doc;
mod file_types;
mod icon;
mod instance;
mod manage;
mod placement;
mod prompt;
mod session;
mod settings_window;
mod theme;
mod ui;
mod update;
mod views;
mod workspace;

use std::path::PathBuf;
use std::time::Instant;

use gpui::{
    App, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};

use theme::Settings;
use views::text::*;
use workspace::*;

/// GPUI adds this many pixels to the x origin of every new X11 window.
pub const X11_ORIGIN_SHIFT: f32 = 2.0;

const USAGE: &str = "rustpad++ - a fast text editor for files of any size

usage: rustpad [-n] [--startup-bench] [FILE...]

  -n, --new-instance  do not hand the files to an already running window
  --startup-bench     print the time to the first rendered frame and exit
";

fn main() {
    let t0 = Instant::now();
    // Non-UTF-8 arguments are file names, handled below.
    let args: Vec<String> = std::env::args_os().skip(1).filter_map(|a| a.into_string().ok()).collect();
    // Run as root through pkexec, without any UI.
    if let [action] = args.as_slice()
        && let Some(code) = manage::run(action)
    {
        std::process::exit(code);
    }
    // For packaging: the menu entry for a program installed at PATH.
    if let [flag, path] = args.as_slice()
        && flag == "--desktop-entry"
    {
        print!("{}", icon::desktop_entry(path));
        return;
    }
    // Before an update can replace this program and blur where it lives.
    rustpad_update::layout::Install::current();
    let mut bench = false;
    let mut new_instance = false;
    let mut files = Vec::new();
    for a in std::env::args_os().skip(1) {
        match a.to_str() {
            Some("--startup-bench") => bench = true,
            Some("-n") | Some("--new-instance") => new_instance = true,
            Some("-h") | Some("--help") => {
                print!("{USAGE}");
                return;
            }
            _ => files.push(PathBuf::from(a)),
        }
    }

    let config = config::Config::load();
    let settings = Settings::from_config(&config);
    let single = !bench && !new_instance && settings.single_instance;
    if single && instance::hand_off(&files) {
        return;
    }
    let incoming = if single { instance::listen() } else { None };
    // One process owns the session (tabs + unsaved changes); benchmarks never do.
    if !bench {
        session::acquire();
    }

    gpui_platform::application().with_assets(assets::Assets).run(move |cx: &mut App| {
        cx.set_global(settings);
        ui::init(cx);
        prompt::install(cx);
        bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());

        // Restore the last placement if it is still on a connected display.
        let saved = Settings::get(cx).window.filter(|w| {
            let b = match w {
                WindowBounds::Windowed(b) | WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => *b,
            };
            cx.displays().iter().any(|d| d.bounds().intersects(&b))
        });
        // GPUI's X11 backend creates windows 2 px right of the requested origin.
        let saved = saved.map(|w| match w {
            WindowBounds::Windowed(mut b) => {
                b.origin.x -= px(X11_ORIGIN_SHIFT);
                WindowBounds::Windowed(b)
            }
            other => other,
        });
        let window_bounds =
            saved.unwrap_or_else(|| WindowBounds::Windowed(Bounds::centered(None, size(px(1180.), px(780.)), cx)));
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(window_bounds),
                    titlebar: Some(TitlebarOptions { title: Some("rustpad++".into()), ..Default::default() }),
                    app_id: Some("rustpad".into()),
                    window_min_size: Some(size(px(360.), px(200.))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Workspace::new(files, window, cx)),
            )
            .expect("failed to open window");
        icon::apply();
        if !bench {
            icon::register();
            // After an update: quit like the user would, then the new copy starts.
            cx.set_global(update::Quitter(std::rc::Rc::new(move |cx: &mut App| {
                window.update(cx, |ws, window, cx| ws.quit(&Quit, window, cx)).ok();
            })));
            update::init(cx);
            if update::first_start_after_update(cx) {
                window.update(cx, |ws, window, cx| ws.show_changelog(window, cx)).ok();
            }
        }

        if let Some(rx) = incoming {
            cx.spawn(async move |cx| {
                while let Ok(files) = rx.recv().await {
                    window
                        .update(cx, |ws, window, cx| {
                            for f in &files {
                                ws.open_path(f, window, cx);
                            }
                            window.activate();
                        })
                        .ok();
                }
            })
            .detach();
        }

        if bench {
            window
                .update(cx, |_, window, _| {
                    // The second callback runs once the first frame has been drawn.
                    window.on_next_frame(move |window, _| {
                        window.on_next_frame(move |_, cx| {
                            println!("first frame: {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
                            cx.quit();
                        });
                        window.refresh();
                    });
                })
                .ok();
        }
        cx.activate(true);
    });
    if single {
        instance::cleanup();
    }
}

fn bind_keys(cx: &mut App) {
    let e = Some("Editor");
    let h = Some("HexView");
    let j = Some("JsonView");
    let m = Some("MarkdownView");
    let c = Some("CsvView");
    cx.bind_keys([
        // Global
        KeyBinding::new("ctrl-o", Open, None),
        KeyBinding::new("ctrl-s", Save, None),
        KeyBinding::new("ctrl-alt-s", SaveAs, None),
        KeyBinding::new("ctrl-shift-s", SaveAll, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("escape", CloseOverlay, Some("Workspace")),
        KeyBinding::new("escape", CloseOverlay, Some("Settings")),
        KeyBinding::new("ctrl-n", NewFile, None),
        KeyBinding::new("ctrl-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-pagedown", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("ctrl-pageup", PrevTab, None),
        KeyBinding::new("ctrl-1", ShowText, None),
        KeyBinding::new("ctrl-2", ShowHex, None),
        KeyBinding::new("ctrl-3", ShowTree, None),
        KeyBinding::new("ctrl-4", ShowMarkdown, None),
        KeyBinding::new("ctrl-5", ShowTable, None),
        KeyBinding::new("ctrl-e", CycleView, None),
        KeyBinding::new("ctrl-f", Find, None),
        KeyBinding::new("f3", FindNext, None),
        KeyBinding::new("shift-f3", FindPrev, None),
        KeyBinding::new("ctrl-h", Replace, None),
        KeyBinding::new("ctrl-g", GotoLine, None),
        KeyBinding::new("ctrl-=", ZoomIn, None),
        KeyBinding::new("ctrl-+", ZoomIn, None),
        KeyBinding::new("ctrl--", ZoomOut, None),
        KeyBinding::new("ctrl-0", ZoomReset, None),
        KeyBinding::new("ctrl-q", Quit, None),
        // Editor
        KeyBinding::new("left", MoveLeft, e),
        KeyBinding::new("right", MoveRight, e),
        KeyBinding::new("up", MoveUp, e),
        KeyBinding::new("down", MoveDown, e),
        KeyBinding::new("shift-left", SelectLeft, e),
        KeyBinding::new("shift-right", SelectRight, e),
        KeyBinding::new("shift-up", SelectUp, e),
        KeyBinding::new("shift-down", SelectDown, e),
        KeyBinding::new("ctrl-left", WordLeft, e),
        KeyBinding::new("ctrl-right", WordRight, e),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, e),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, e),
        KeyBinding::new("home", Home, e),
        KeyBinding::new("end", End, e),
        KeyBinding::new("shift-home", SelectHome, e),
        KeyBinding::new("shift-end", SelectEnd, e),
        KeyBinding::new("ctrl-home", DocStart, e),
        KeyBinding::new("ctrl-end", DocEnd, e),
        KeyBinding::new("ctrl-shift-home", SelectDocStart, e),
        KeyBinding::new("ctrl-shift-end", SelectDocEnd, e),
        KeyBinding::new("pageup", PageUp, e),
        KeyBinding::new("pagedown", PageDown, e),
        KeyBinding::new("shift-pageup", SelectPageUp, e),
        KeyBinding::new("shift-pagedown", SelectPageDown, e),
        KeyBinding::new("backspace", Backspace, e),
        KeyBinding::new("shift-backspace", Backspace, e),
        KeyBinding::new("delete", Delete, e),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, e),
        KeyBinding::new("ctrl-delete", DeleteWordRight, e),
        KeyBinding::new("enter", Newline, e),
        KeyBinding::new("tab", Tab, e),
        KeyBinding::new("ctrl-a", SelectAll, e),
        KeyBinding::new("ctrl-c", Copy, e),
        KeyBinding::new("ctrl-insert", Copy, e),
        KeyBinding::new("ctrl-x", Cut, e),
        KeyBinding::new("ctrl-v", Paste, e),
        KeyBinding::new("shift-insert", Paste, e),
        KeyBinding::new("ctrl-z", Undo, e),
        KeyBinding::new("ctrl-y", Redo, e),
        KeyBinding::new("ctrl-shift-z", Redo, e),
        KeyBinding::new("ctrl-d", DuplicateLine, e),
        KeyBinding::new("ctrl-shift-l", DeleteLine, e),
        KeyBinding::new("escape", Cancel, e),
        // Hex
        KeyBinding::new("left", MoveLeft, h),
        KeyBinding::new("right", MoveRight, h),
        KeyBinding::new("up", MoveUp, h),
        KeyBinding::new("down", MoveDown, h),
        KeyBinding::new("shift-left", SelectLeft, h),
        KeyBinding::new("shift-right", SelectRight, h),
        KeyBinding::new("shift-up", SelectUp, h),
        KeyBinding::new("shift-down", SelectDown, h),
        KeyBinding::new("pageup", PageUp, h),
        KeyBinding::new("pagedown", PageDown, h),
        KeyBinding::new("home", Home, h),
        KeyBinding::new("end", End, h),
        KeyBinding::new("ctrl-home", DocStart, h),
        KeyBinding::new("ctrl-end", DocEnd, h),
        KeyBinding::new("ctrl-a", SelectAll, h),
        KeyBinding::new("ctrl-c", Copy, h),
        // JSON tree
        KeyBinding::new("up", MoveUp, j),
        KeyBinding::new("down", MoveDown, j),
        KeyBinding::new("left", MoveLeft, j),
        KeyBinding::new("right", MoveRight, j),
        KeyBinding::new("enter", Newline, j),
        KeyBinding::new("pageup", PageUp, j),
        KeyBinding::new("pagedown", PageDown, j),
        KeyBinding::new("home", DocStart, j),
        KeyBinding::new("end", DocEnd, j),
        KeyBinding::new("ctrl-home", DocStart, j),
        KeyBinding::new("ctrl-end", DocEnd, j),
        // Markdown
        KeyBinding::new("up", MoveUp, m),
        KeyBinding::new("down", MoveDown, m),
        KeyBinding::new("pageup", PageUp, m),
        KeyBinding::new("pagedown", PageDown, m),
        KeyBinding::new("ctrl-home", DocStart, m),
        KeyBinding::new("ctrl-end", DocEnd, m),
        // CSV table
        KeyBinding::new("up", MoveUp, c),
        KeyBinding::new("down", MoveDown, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("ctrl-home", DocStart, c),
        KeyBinding::new("ctrl-end", DocEnd, c),
    ]);
}
