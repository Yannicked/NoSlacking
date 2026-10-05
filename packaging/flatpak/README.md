# Flatpak

`cloud.yannick.NoSlacking.yml` builds NoSlacking on the freedesktop 26.08
runtime with its Rust SDK extension. The release workflow builds it on every
tag and attaches a `.flatpak` bundle to the release.

Flatpak builds without network, so every crate is listed with its checksum in
`cargo-sources.json`. That file is generated from `Cargo.lock` and not kept in
git; make it again whenever `Cargo.lock` changes.

## Building locally

From the repository root:

```sh
# Once: the builder, the SDK and its Rust extension.
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user flathub org.flatpak.Builder \
    org.freedesktop.Sdk//26.08 org.freedesktop.Sdk.Extension.rust-stable//26.08

# The crate list, with flatpak-builder-tools' generator (it needs aiohttp and
# tomlkit; the workflow pins the commit and the versions).
python3 flatpak-cargo-generator.py Cargo.lock -o packaging/flatpak/cargo-sources.json

# Build, install for your user, and run.
flatpak run org.flatpak.Builder --user --install --force-clean \
    --state-dir=packaging/flatpak/.flatpak-builder \
    packaging/flatpak/build packaging/flatpak/cloud.yannick.NoSlacking.yml
flatpak run cloud.yannick.NoSlacking
```

## What the sandbox allows, and what it costs

- The window (Wayland, else X11, with the GPU), the network, desktop
  notifications, the keyring (Secret Service) and the tray item.
- Files are saved to Downloads only; uploads are picked through the
  desktop's file portal.
- "Start when you log in" cannot write the desktop's autostart folder from
  the sandbox; it says to add NoSlacking to the desktop's startup apps
  instead.
- Spell checking uses the runtime's Hunspell dictionaries (the freedesktop
  runtime ships about 160, English and Dutch among them).
