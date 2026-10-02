# Contributing

## Build

NoSlacking needs a Rust toolchain (pinned in `rust-toolchain.toml`) and the
usual egui build dependencies. On Debian/Ubuntu:

```
sudo apt-get install libxkbcommon-dev libwayland-dev libgl1-mesa-dev cmake
```

```
cargo run                 # the real app
cargo run --features demo # a pretend Slack, offline
```

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

```
desktop-file-validate packaging/applications/cloud.yannick.NoSlacking.desktop
appstreamcli validate --no-net packaging/metainfo/cloud.yannick.NoSlacking.metainfo.xml
```

## Translations

Interface strings go through `t("…")` / `tn(…)` (`src/i18n.rs`). Catalogs are
`assets/i18n/<tag>.po`, compiled at build time. Add a language by adding its
`.po` and a `Locale` variant.
