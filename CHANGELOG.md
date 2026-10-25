# Changelog

What changed in each version of rustpad++. The newest version is at the top.

## 0.4.1 - 2026-10-25

- rustpad++ starts about seven times as fast: the window shows its content
  after about 35 ms instead of 230 ms. The first frames are drawn on the CPU
  while the graphics card gets ready, which it then takes over unnoticed.
  Without a usable graphics driver, rustpad++ keeps drawing on the CPU.
- The status bar no longer keeps showing "indexing lines" after a large file
  has been indexed.

## 0.4.0 - 2026-10-25

- New color scheme Aurora (Settings → Appearance): graded dark surfaces,
  syntax colors in violet, indigo and teal, and an About dialog lit from
  below.
- A refreshed look in every color scheme: buttons, tabs and switches are
  rounded pills, menus and dropdowns are translucent panels, dialogs have
  rounder corners. The interface uses the Inter font where it is installed.
- The logo in the About dialog is sharp at every display scale.
- rustpad++ now also speaks Spanish, French, Italian and Brazilian
  Portuguese. These translations are drafts; corrections from native
  speakers are welcome.
- Find and replace can match case and whole words only (the two switches in
  the find bar). Ignoring case also works for umlauts and other letters
  beyond ASCII, in files of any size.
- Line endings can be converted: Edit → Line Endings, or click "LF"/"CRLF"
  in the status bar. Saving then writes only that kind, which also evens out
  files with mixed line endings.
- Many tabs no longer get cut off: the tab bar scrolls (also with the mouse
  wheel), the active tab is always shown, and a list at its end shows all
  tabs when they do not fit.
- The settings no longer have an empty "Advanced" tab.
- Very long paragraphs and code blocks in the Markdown view, such as a large
  JSON file shown as Markdown, open and scroll without delay.
- The update check uses a newer TLS library that fixes several security
  issues (RUSTSEC-2026-0346 to -0349, -0381, -0382).
- rustpad++ is now built on GPUI Community Edition, the maintained community
  fork of GPUI.

## 0.3.0 - 2026-10-21

- CSV and TSV files can be shown as a table (View > Table, Ctrl+5); they still
  open as text. Clicking a column header sorts by it, numbers by value;
  dragging a header's right edge changes the column's width, a double-click
  there resets it. Double-click or Enter on a row jumps to it in the text.
- Opening rustpad++ from a terminal that is itself an AppImage (Tabby, for
  example) no longer makes file managers open files with that program.

## 0.2.0 - 2026-10-17

- Markdown files open in the rendered view; Ctrl+1 shows the source. New,
  empty Markdown files still open as text.
- Ctrl+mouse wheel zooms, one step per notch, in every view.
- The rendered Markdown view zooms along with the other views.

## 0.1.0 - 2026-10-12

The first public version.

### Files of any size

- Files are memory-mapped, not loaded: a 1 TiB file opens at once, and lines
  of large files are counted in the background.
- Four views of the same file: text, hex, a lazily read JSON tree and rendered
  Markdown (Ctrl+1 to Ctrl+4, Ctrl+E cycles).
- Encodings: UTF-8 with or without BOM, UTF-16 LE/BE, Windows-1252,
  ISO-8859-1 and ISO-8859-15 are detected; reopen or convert from the status
  bar.

### Editing

- Find and replace (Ctrl+H); replace all is a single undo step.
- Go to a line, a byte offset or a percentage (Ctrl+G).
- Word wrap (View menu), duplicate and delete lines, zoom.
- Edit menu with cut, copy, paste, undo and redo.

### Never lose work

- All tabs come back after quitting, unsaved changes included; changes are
  backed up every second.
- Files changed, replaced, truncated or deleted by other programs are
  noticed; growing logs can be followed like `tail -f`.

### Around it

- English and German, following the system language or chosen in the
  settings.
- Color schemes Dark, Light, Monokai and Solarized Dark.
- A menu entry and sharp icons, and rustpad++ as the default application for
  text, log, Markdown, JSON and other files (Settings → File Types).
- Opening more files adds tabs to the running window.
- Updates: new versions are offered in the app, checked against a signature
  and installed on request.
