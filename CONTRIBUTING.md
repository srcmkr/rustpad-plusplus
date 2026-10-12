# Contributing

Bug reports, ideas, translations and pull requests are welcome. For bigger
changes, please open an issue first so we can talk about the approach.

## Reporting a bug

Open an [issue](https://github.com/srcmkr/rustpad-plusplus/issues). Please
mention the version (*Help → About rustpad++*), your distribution and desktop,
and, if it is about a file, its size and encoding (shown in the status bar).

## Building from source

You need [Rust](https://rustup.rs/) 1.90 or newer and a few libraries. On
Debian, Ubuntu and Linux Mint:

```sh
sudo apt install build-essential libxkbcommon-x11-dev libxcb1-dev \
    libwayland-dev libfontconfig-dev libvulkan1
git clone https://github.com/srcmkr/rustpad-plusplus
cd rustpad-plusplus
cargo build --release
./target/release/rustpad
```

A copy run from `target/release` adds itself to the application menu of your
user. *Help → Check for Updates → Install for all users* installs it under
`/usr/local` instead, where it can update itself.

`RUSTPAD_UPDATE_FAKE=0.9.0` pretends that version is out, to see the update
offer without a release; `RUSTPAD_UPDATE_DELAY=<seconds>` shortens the wait
before the first check.

Before a pull request, run both of these; they should pass without warnings:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Translations

Texts are gettext catalogs in [`po/`](po): English is written in the code,
every other language is a `.po` file. To fix a translation, edit the
`msgstr`. To add a language, copy `po/rustpad.pot` to `po/<lang>.po`,
translate it (Poedit, Weblate or any text editor) and add it to `CATALOGS` in
[`src/i18n.rs`](src/i18n.rs). `cargo test` fails while a catalog misses a
text or keeps one the code no longer has; after changing texts in the code,
regenerate the template with `cargo test -- --ignored write_pot`.

## Where things are

| Path | Contents |
|---|---|
| [`src/doc`](src/doc) | The document: memory-mapped piece table, line index, rows and wrapping, encodings, type detection |
| [`src/views`](src/views) | Text, hex, JSON tree and Markdown views |
| [`src`](src) | Window, menus, dialogs, settings, session, translations, updates (`update.rs`, `manage.rs`) |
| [`crates/rustpad-update`](crates/rustpad-update) | Finding, verifying and unpacking releases; key and signing tools |
| [`packaging`](packaging) | Polkit rules, release key, package build |
| [`patches`](patches) | A patched copy of a dependency; the reason is noted in `Cargo.toml` |
| [`assets`](assets) | Logo, icons, menu entry template |
| [`po`](po) | Translations |
| [`.github/workflows`](.github/workflows) | Tests for every push, releases for every tag |
| [`dev`](dev) | The release script |
| [`CHANGELOG.md`](CHANGELOG.md) | What changed in each version; shown in the app and used as release notes |

## Releases

`dev/release.sh 0.2.0` sets the version, runs the tests, commits, tags and
pushes. The tag starts the release workflow: it tests, builds on Ubuntu 22.04
(so the program runs on older systems too), packs the archive, the Debian
package and the AppImage with `packaging/build-packages.sh`, signs all three
and publishes them as the latest release.

Installed copies find it by themselves: the Debian package updates through
apt, an installation under `/usr/local` replaces itself (both run
`rustpad --update` as root through pkexec, which downloads and checks the
release on its own), and an AppImage replaces its own file.

The signing key is made once with
`cargo run -p rustpad-update --example release-key -- <secret key file>`.
Its public half goes into `packaging/minisign.pub` and is built into every
copy of rustpad++; the secret half goes into the repository secret
`MINISIGN_SECRET_KEY`. Keep a backup of the secret key: without it, installed
copies cannot verify new releases.
