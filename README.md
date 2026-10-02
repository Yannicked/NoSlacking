# NoSlacking

A native [Slack](https://slack.com) client for Linux, macOS and Windows,
written in Rust. It is small, fast and quiet: one window, your channels and
DMs, threads, reactions, files and emoji, with no Electron.

It is built on [egui](https://github.com/emilk/egui) and the
[fastframe](https://github.com/crmne/fastframe) crates, the same foundation as
[ZapFast](https://github.com/crmne/zapfast) and
[Spotifast](https://github.com/crmne/spotifast).

## What it does

- **Workspaces:** channels, private channels, direct messages and group DMs,
  in one workspace rail across several workspaces, with your Slack sidebar
  sections.
- **Messages:** history that scrolls back, day separators, an unread line,
  and "jump to unread" and "jump to newest".
- **Threads:** a side panel, with "also send to the channel".
- **Search:** messages and files, with Slack's `from:`, `in:`, `before:`
  and `has:` filters, and jumping to a result in context.
- **Conversations:** start direct and group messages; browse, join, leave
  and create channels; channel details, pins and bookmarks.
- **Composer:** `@mention`, `#channel` and `:emoji:` autocomplete, slash
  commands, formatting shortcuts, drafts kept across restarts, and
  uploading pasted images and dropped files.
- **Rendering:** reactions with skin tones, your workspace's custom emoji,
  inline images, file cards, highlighted code blocks, and Slack's markup.
- **Notifications:** for DMs, mentions and your own keywords, with mute,
  per-conversation levels and Do Not Disturb.
- **Desktop:** the unread count in the title and launcher, a tray icon,
  start at login, themes (dark, light, your own palettes, and Omarchy on
  Linux), and a Ctrl+K quick switcher.
- **Accessibility:** keyboard navigation of messages, screen reader labels,
  and an English and Dutch interface.

## Signing in

Three ways, chosen on the sign-in screen. The first two reuse your own
Slack session; neither needs anything registered.

- **Sign in with your browser.** NoSlacking opens Slack's sign-in page in
  your browser. Sign in as usual (password, emailed code or SSO); when Slack
  hands the sign-in back, the browser passes it to NoSlacking, which
  registers itself for `slack://` links for this. If your browser does not
  pass it on, paste the `slack://` link from the page instead.
- **Paste your session cookie.** Paste your workspace address and the `d`
  cookie from a browser where you are logged in to Slack. The sign-in page
  explains where to find it.
- **Your own Slack app (advanced).** Create a free Slack app from the bundled
  manifest, paste its credentials, and sign in. This is the route Slack
  documents and supports, with live Socket Mode updates; a workspace admin
  may need to approve the app.

Session sign-ins get live messages over Slack's session socket. Tokens and
cookies are stored only in your operating system's keyring (Secret Service on
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

## Use at your own risk

NoSlacking is an independent, unofficial client. It is not made, endorsed,
supported or reviewed by Slack Technologies or Salesforce. "Slack" is a
trademark of Salesforce, Inc., used here only to say which service this
client works with.

Before you use it, know that:

- **Session sign-in is unsupported.** Both browser sign-in and the pasted
  cookie use endpoints that Slack does not document. Slack can change or
  remove them at any time, and NoSlacking may stop working without warning.
- **It may break your workspace's rules or Slack's terms.** Slack's terms
  limit third-party clients, and your workspace may only allow approved
  apps. Slack or a workspace admin may sign out or suspend an account that
  uses an unofficial client. Check with your workspace's admins if you are
  unsure, and do not use it where it is not allowed.
- **Your session is as powerful as your password.** The `d` cookie and the
  session tokens give full access to your account. NoSlacking keeps them in
  the OS keyring and sends them only to Slack's own hosts, but treat them
  like a password and sign out (or revoke the session in Slack) on a
  computer you stop using.
- **There is no warranty.** The software is provided as is; see the
  [license](LICENSE).

Use NoSlacking only with your own account, and use your own Slack app (the
documented route) where your workspace requires it.

## Thanks

NoSlacking owes its approach to two other unofficial clients:

- [Make Slack Great Again](https://github.com/punarinta/make-slack-great-again)
  (msga), which inspired session sign-in and signing in through the browser
  by catching Slack's hand-off.
- [wee-slack](https://github.com/wee-slack/wee-slack), which has reused the
  browser session for years.

## License

MIT; see [`LICENSE`](LICENSE).
