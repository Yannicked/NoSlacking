# Contributing

## Build

NoSlacking needs a Rust toolchain (pinned in `rust-toolchain.toml`) and the
usual egui build dependencies. On Debian/Ubuntu:

```
sudo apt-get install libxkbcommon-dev libwayland-dev libgl1-mesa-dev libasound2-dev libssl-dev cmake
```

```
cargo run                 # the real app
cargo run --features demo # a pretend Slack, offline
```

Sound plays through ALSA on Linux (`libasound2-dev`, or
`alsa-lib-devel`). Huddles' DTLS is OpenSSL's: Linux links the system's
(`libssl-dev`, or `openssl-devel`), and macOS and Windows build it from
source, which needs Perl. Every build has huddles, so these are always
needed.

## Before a pull request

```
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo deny check          # advisories, licenses, sources (cargo install cargo-deny)
```

A new dependency must use a license allowed in `deny.toml` and come from
crates.io; the only git sources allowed are the fastframe crates and the
egui and winit forks.

Keep the layered design: UI reads the model and pushes actions; the worker
owns the network. See `AGENTS.md`.

## Packaging

Everything a package needs beyond the binary is in `packaging/`, named after
the app id `cloud.yannick.NoSlacking`.

### Linux

- `applications/cloud.yannick.NoSlacking.desktop` goes to
  `share/applications/`. It registers the `noslacking://` scheme that the
  "your own app" sign-in returns to.
- `icons/hicolor/` mirrors the icon theme layout; copy it over
  `share/icons/hicolor/`. The window icon is compiled in from the 256 px
  PNG, so keep that path.
- `metainfo/cloud.yannick.NoSlacking.metainfo.xml` goes to
  `share/metainfo/` for software centers.

- Noto Color Emoji (about 10 MB) is compiled in by the default
  `bundled-emoji` feature, since some desktops (Fedora) ship it only as
  a COLRv1 font the app cannot draw. A package can build with
  `--no-default-features --features highlight` and install
  `assets/fonts/NotoColorEmoji.ttf` (with its license) as
  `share/noslacking/NotoColorEmoji.ttf` instead, or rely on a colour bitmap
  emoji font in the system's font directories. macOS bundles no emoji font
  and Windows only the flags Segoe UI Emoji lacks
  (`assets/fonts/NotoColorEmoji-Flags.ttf`, made with
  `pyftsubset NotoColorEmoji.ttf --unicodes="U+1F1E6-1F1FF,U+1F3F4,U+E0020-E007F" --layout-features='*'`).

```
desktop-file-validate packaging/applications/cloud.yannick.NoSlacking.desktop
appstreamcli validate --no-net packaging/metainfo/cloud.yannick.NoSlacking.metainfo.xml
```

### macOS

`macos/bundle.sh` wraps a binary in `NoSlacking.app`, with
`macos/Info.plist` (which declares the `noslacking://` scheme), an `.icns`
made from the 512 px icon, and an ad-hoc signature:

```
cargo build --release
packaging/macos/bundle.sh target/release/noslacking 0.1.0 dist
```

For a universal binary, build `aarch64-apple-darwin` and
`x86_64-apple-darwin` and `lipo -create` them first. The bundle is not
notarized, so Gatekeeper asks on first launch.

The app does not handle macOS's open-URL event yet, so `noslacking://` and
`slack://` links opened elsewhere do not reach it, even inside the bundle.
macOS does not start a second copy with the link as an argument (as Linux
and Windows do); it sends the running app an Apple event, which only an
`NSApplicationDelegate` with `application:openURLs:` (or an
`NSAppleEventManager` handler) receives. Neither winit (crmne/winit
apps-0.30), eframe nor the fastframe crates expose one, and defining one
takes `unsafe` Objective-C class declarations, which this crate forbids.
The right home is a small safe API in `fastframe-macos` (which already
holds the AppKit `unsafe`), forwarding each URL into the same path a second
launch's `Request::Open` takes (`Command::Callback`). Until then sign-in on
macOS uses the loopback redirect, the default on every platform.

The bundle declares `noslacking://` only. Linux and Windows borrow
`slack://` for a browser sign-in and give it back afterwards
(`src/slack_links.rs`); on macOS that takes Launch Services calls
(`LSSetDefaultHandlerForURLScheme` to claim, `LSCopyDefaultHandlerForURLScheme`
to remember the Slack app's bundle id), which also need `unsafe` here. A
bundle that declared `slack` would let Launch Services pick NoSlacking for
the Slack app's links with nothing to give them back, so it waits for the
same `fastframe-macos` API.

### Windows

`build.rs` embeds `windows/noslacking.ico` and the version info in
`noslacking.exe` through `winresource`, using the Windows SDK's `rc.exe`
(or `llvm-rc` when cross-compiling). Without one the build still succeeds,
with a warning and no icon. The `.exe` registers `noslacking://` for the
current user on first sign-in, so it needs no installer.

### Releases

Bump `version` in `Cargo.toml`, then push a matching tag (`v0.2.0` for
`0.2.0`). `.github/workflows/release.yml` builds the three platforms and
publishes a GitHub release with:

- `noslacking-<version>-linux-x86_64.tar.gz`: `bin/noslacking` and a
  `share/` tree (desktop file, icons, metainfo); copy both into `~/.local`
  or `/usr/local`. Built on Ubuntu 24.04, so it needs glibc 2.39 or newer.
- `noslacking-<version>-macos-universal.zip`: `NoSlacking.app` for Apple
  silicon and Intel.
- `noslacking-<version>-windows-x86_64.zip`: `noslacking.exe`.
- `SHA256SUMS` for all three.

Each archive also carries `LICENSE` and `README.md`.

## Translations

Interface strings go through `t("…")` / `tn(…)` (`src/i18n.rs`). Catalogs are
`assets/i18n/<tag>.po`, compiled at build time. Add a language by adding its
`.po` and a `Locale` variant.

Translate whole sentences: a name, a count or an error goes in through a
`{name}` placeholder with `tf("Signed in to {name}.", &[("name", name)])`,
never by gluing a translated piece to it. A test scans `src` for every
`t`, `tf` and `tn` literal and fails when `nl.po` lacks one or a
translation drops a placeholder.
