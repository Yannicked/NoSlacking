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

## Translations

Interface strings go through `t("…")` / `tn(…)` (`src/i18n.rs`). Catalogs are
`assets/i18n/<tag>.po`, compiled at build time. Add a language by adding its
`.po` and a `Locale` variant.
