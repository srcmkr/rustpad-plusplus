# Changelog

What changed in each version of rustpad++. The newest version is at the top.

## Unreleased

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
