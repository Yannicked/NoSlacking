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

## Scripting hooks

In the spirit of wee-slack's hooks, NoSlacking can run your own programs on
new messages: to speak them aloud, log them, or light a lamp. Hooks are off
until you turn them on under **Settings → Scripting hooks**, where each hook
names a program and what it runs for: mentions of you, direct messages, or
keywords (comma separated, matched as whole words, ignoring case).

- The program runs directly, never through a shell. Its command line is
  split on spaces; put `"…"` or `'…'` around a part with spaces. There are no
  variables, pipes or globs.
- It gets one JSON object on its standard input and nothing on its command
  line. Its output is ignored.
- It may run for 10 seconds before it is stopped, and at most four hooks run
  at once; a message that arrives while four are running skips them.
- Only failures are logged (the program could not start, failed, or was
  stopped), never the message.
- The JSON never holds a token, a cookie or a file link that would need one.

Only new messages from other people run hooks, not edits, history or your
own messages. A mention of you is checked first, then a direct message, then
the keywords; the first that matches is the `reason`, and a hook runs once
per message.

```json
{
  "version": 1,
  "event": "message",
  "reason": "keyword",
  "keyword": "deploy",
  "workspace": { "id": "T0123", "name": "Acme", "domain": "acme" },
  "conversation": { "id": "C0456", "name": "engineering", "kind": "channel" },
  "message": {
    "ts": "1700000000.000100",
    "thread_ts": null,
    "user": "U0789",
    "author": "Ana",
    "text": "<@U0001> is the deploy done?",
    "plain": "@Yannick is the deploy done?"
  },
  "permalink": "https://acme.slack.com/archives/C0456/p1700000000000100"
}
```

- `reason` is `mention`, `direct` or `keyword`; `keyword` is only there for
  `keyword`, and names the one that matched.
- `conversation.kind` is `channel`, `private`, `direct` or `group`.
- `message.text` is Slack's own markup; `message.plain` is the same text with
  people and channels by name. `thread_ts` is the thread's parent for a reply,
  else `null`. `user` is `null` for some apps.
- `permalink` opens the message in Slack, or is `null` when there is none.
- `version` goes up only when the shape changes in a way a script would
  notice; new fields may appear at any time.

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
