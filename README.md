# NoSlacking

A native [Slack](https://slack.com) client for Linux, macOS and Windows,
written in Rust. It is small, fast and quiet: one window, your channels and
DMs, threads, reactions, files and emoji, with no Electron.

It is built on [egui](https://github.com/emilk/egui) and the
[fastframe](https://github.com/crmne/fastframe) crates, the same foundation as
[ZapFast](https://github.com/crmne/zapfast) and
[Spotifast](https://github.com/crmne/spotifast).

## What it does

- Channels, private channels, direct messages and group DMs, in one workspace
  rail across several workspaces.
- Message history that scrolls back, day separators and an unread line.
- Threads in a side panel, with "also send to the channel".
- Emoji reactions, a picker, and your workspace's custom emoji.
- Inline image previews, file cards that download, and uploads.
- Slack's markup: bold, italic, code, quotes, mentions, channel links.
- Desktop themes (dark, light, your own palettes, and Omarchy on Linux),
  `@mention` and `:emoji:` autocomplete, a Ctrl+K quick switcher, and keyboard
  navigation.

## Signing in

Two ways, chosen on the sign-in screen:

- **Your Slack session (quick).** Paste your workspace address and the `d`
  cookie from a browser where you are logged in to Slack. NoSlacking reuses
  that session the way [wee-slack](https://github.com/wee-slack/wee-slack) and
  [Make Slack Great Again](https://github.com/punarinta/make-slack-great-again)
  do, and gets live messages over Slack's session socket. Nothing to register.
  The sign-in page explains where to find the cookie. This uses undocumented
  endpoints, so treat it as best-effort.
- **Your own Slack app (advanced).** Create a free Slack app from the bundled
  manifest, paste its credentials, and sign in. This is the sanctioned route,
  with live Socket Mode updates; a workspace admin may need to approve the app.

Tokens are stored only in your operating system's keyring (Secret Service on
Linux, the Keychain on macOS, the Credential Manager on Windows), never in a
file, and never written to the log.

## Building

```
cargo run
```

You need a Rust toolchain and the egui build dependencies; see
[`CONTRIBUTING.md`](CONTRIBUTING.md). To try the interface without a Slack
account:

```
cargo run --features demo
```

## Files

NoSlacking keeps settings and themes in your config directory, logs and read
state in your state directory, and the image cache in your cache directory.
Open them from **Settings → Files**.

## Disclaimer

NoSlacking is an independent client and is not affiliated with or endorsed by
Slack Technologies. "Slack" is a trademark of Salesforce. Session sign-in uses
endpoints Slack does not document; use it with your own account and at your own
risk.

## License

MIT; see [`LICENSE`](LICENSE).
