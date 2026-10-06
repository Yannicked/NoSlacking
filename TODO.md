# TODO

Findings from a code review at `684c148`. Line numbers are as of that
commit. When this review was written, `cargo fmt`, clippy (`-D warnings`)
and all 59 tests passed.

Status on 2026-10-02: every review finding (P0 to P3) and every feature
below is done on `main`, which has 366 tests. What is left are known limits
and things to test, under "Follow-ups".

## P0: Security

- [x] **Credentials leak to any host whose URL contains `files.slack.com`.**
      `ui/mod.rs:177` uses `url.contains(..)`, and then `slack/client.rs:292,383`
      attaches `Bearer` and `Cookie: d=` to any host. A bot, webhook or link
      preview with `image_url: https://evil.com/x.png?files.slack.com`
      (or `files.slack.com.evil.com`) is enough to send the token out.
      - Fix in two places: parse the URL in `image_uri`, and check again in
        `get_bytes` before adding credentials (`https` plus an exact host
        allowlist).
      - Apply the same check to `Command::Download` (`backend/worker.rs:533`).
      - Add tests with hostile URLs.
- [x] **Links with any scheme are passed to `open::that_detached`.**
      In `mrkdwn.rs:253`, any target that contains `:` becomes a link. That
      includes `C:\x.exe`, `file://`, `smb://` and `javascript:`.
      - Allow only `http`, `https` and `mailto` (and `slack://`, handled
        internally). Show other schemes as plain text, or ask before opening.
- [x] **Image decompression bombs.** `MAX_IMAGE_BYTES` limits only the
      compressed size (`images.rs:25`). egui_extras decodes with no size
      limit, decodes every GIF frame, and never evicts decoded textures.
      - Write our own loader: read the dimensions first, set
        `image::Limits`, cap the number of GIF frames, and downscale to the
        size the image is shown at.
- [x] **Panic messages skip `redact_tokens`** (`main.rs:112`). Apply both
      redaction rules, or use `PanicMessage::Omit`.
- [x] **`redact_tokens` gaps** (`main.rs:190`). It misses:
      - `xoxa-`, `xoxr-` and `xoxs-` tokens
      - `wss://` socket URLs
      - the OAuth client secret, which `Debug` prints on `AppCredentials`
        and `OauthApp`

      Then:
      - Move `redact_tokens` into the lib and add tests.
      - Hand-write `Debug` for the types that hold secrets.
- [x] **Private files stay on disk after sign-out.** Keep authed images in a
      per-team cache folder, delete it on sign-out, and call `forget_all`
      (`images.rs:107`).
- [x] **Loopback OAuth listener** (`auth.rs:197`) ends on the first
      `/callback` request, whatever its state. Keep listening until the
      expected `state` arrives, and match the path exactly.
- [x] **Session sign-in** (`slack/session.rs`):
      - `normalize_workspace` (`:43`) accepts any host with a dot. Restrict
        it to `*.slack.com` and enterprise domains.
      - The seeded client (`:101`) has no timeouts.
- [x] **Single-instance socket** (`single_instance.rs:82`):
      - `read_line` is unbounded and there is no per-connection deadline.
      - `0o600` applies only when the file is created. Call
        `set_permissions` on the existing file too.
- [x] **Demo folder** `/tmp/noslacking-demo` (`main.rs:96`) is shared by
      all users and can be pre-created as a symlink. Use a per-user temp dir.

## P1: Correctness bugs

### Data loss and wrong results

- [x] **Editing a message breaks its mentions and links.** `StartEdit`
      uses `unescape` (`app.rs:1519`) and `Edit` uses `escape` (`:1465`),
      so `<@U123>` is saved as `&lt;@U123&gt;`.
      - Convert the wire text into editable text, seed `Draft.mentions`,
        and save through `to_wire`.
      - Add a round-trip test.
- [x] **A picked file can upload to the wrong conversation.** `PickUpload`
      looks up the team and channel when the dialog closes
      (`app.rs:1576`). Capture them when the button is pressed.
- [x] **Failed optimistic edits, deletes and reactions are never undone**
      (`app.rs:1337,1460`). Return per-request result events with the
      original state, as `Sent` already does.
- [x] **Sign-out does not cancel `boot` or `unread_sweep`.** A late
      `WorkspaceReady` re-adds the signed-out workspace
      (`worker.rs:204,1044`).
      - Store a cancellation handle per team, or drop events for unknown
        teams.
- [x] **Commands for an unknown team fail silently.** The optimistic send
      then stays pending forever (`worker.rs:371,409,549`). Emit
      `Sent { Err }` or `Event::Error`.
- [x] **Settings: one invalid field resets everything**, and the next save
      overwrites the file (`settings.rs:110`).
      - Back up the bad file before saving.
      - Deserialize leniently, field by field.
      - Add a `version` field.
- [x] **Manifest scopes are missing.** `conversations.mark` needs
      `channels:write`, `groups:write`, `im:write` and `mpim:write`, and the
      stars calls need `stars:read` and `stars:write`. Without these,
      marking as read fails for OAuth users.
      - Remove `reactions:read`, which is unused.

### Real-time, polling and rate limits

- [x] **One global `socket_up` flag covers every workspace and both socket
      kinds** (`worker.rs:66,822-896`). An RTM connection for workspace A
      stops polling for workspace B. Track liveness per team.
- [x] **Polling piles up under rate limits** (`worker.rs:893`,
      `client.rs:243`).
      - A new history task starts every 6 s with no in-flight check.
      - Requests hold their semaphore permit while sleeping on
        `Retry-After`, so sends starve.
- [x] **RTM gives up for good after 3 quick failures** (`rtm.rs:86`), for
      example while Wi-Fi reconnects after sleep. Keep retrying network
      errors with capped backoff.
- [x] **A stale RTM `Unavailable` can kill the replacement socket**
      (`worker.rs:854`). Tag each socket with a generation id.
- [x] **Socket Mode `disconnect` reconnects immediately**, which can loop
      hot (`socket.rs:125`).
      - Treat `link_disabled` as fatal.
      - Apply backoff to short-lived connections.
      - Honour `Retry-After` in `open_url`.

### Tokens and keyring

- [x] **Failed token refresh** (`client.rs:195`):
      - It retries on every API call.
      - `invalid_refresh_token` and `invalid_grant` are not in `is_auth`,
        so the user never reaches the signed-out state.
- [x] **Rotated tokens can be saved out of order** (`worker.rs:179`).
      - Refreshes call `tokio::spawn(save_token)` and don't wait.
      - `SaveApp` builds new `Client`s with a separate `refresh_lock`.
      - Save inside the refresh critical section, and share one token and
        lock per team.
- [x] **Keyring calls block the worker loop** (`worker.rs:118,272`), for
      example while a Secret Service unlock prompt is open. Spawn them.
- [x] **A keyring error at boot leaves the remaining workspaces in limbo**
      (`worker.rs:160`). Emit `SignedOut` for each one that was skipped.

### Files

- [x] **The 120 s total timeout breaks large uploads and downloads**
      (`client.rs:147`). Use a separate client or a per-request timeout
      for transfers.
- [x] **Uploads and downloads buffer up to 1 GB in memory**
      (`worker.rs:496,533`). Stream the body and write to a temp file
      followed by a rename.

### Unread state and counters

- [x] **`Event::Read` can move the read marker backwards** (`app.rs:795`).
      Use `max_ts`, and only clear counts when the marker is at or past
      `latest`.
- [x] **Unread counts stick** after reading on another device
      (`app.rs:1664`). Make `unread` an `Option`, or clear it when
      `last_read >= latest`.
- [x] **`message_changed` raises the reply count and mention count**
      (`app.rs:959,988`). Flag edits on the event.
- [x] **Reply counts after deletes:**
      - Deleting a reply doesn't lower the parent's `reply_count`.
      - An edit with `reply_count == 0` keeps the old count
        (`model.rs:451`).

### Scroll, threads and focus

- [x] **A global `scroll_to_bottom` flag** makes a thread reply yank the
      main list down (`app.rs:1287,938`). Key it by list.
- [x] **A stale `prepended` anchor** survives switching conversations
      (`conversation.rs:253`).
- [x] **Open threads outlive their source:**
      - An open thread isn't closed when its conversation disappears.
      - Deleting a parent leaves an empty thread panel open.
- [x] **The edit field traps focus and swallows Escape** before overlays
      (`message.rs:319,336`).
- [x] **The same message can get two edit fields with one Id** when its
      thread parent is shown in both panels (`message.rs:65,311`).

### Composer and pickers

- [x] **The suggestion popup can't be dismissed.** Enter always accepts,
      so a message like "@chan" can't be sent as typed
      (`composer.rs:157`).
- [x] **`to_wire` mention labels:**
      - Labels are replaced anywhere in the text: "@Ann" plus "@Annabel"
        gives `<@U1>abel`.
      - `replace_word` checks the boundary against `rest` instead of the
        full text.
- [x] **Enter in the emoji picker** with an empty query reacts with the
      first custom emoji (`overlays.rs:292`).
- [x] **The settings "Save" button** doesn't check `can_sign_in()`
      (`settings.rs:278`).

### Pagination and fetch caches

- [x] **Pagination stops silently** (worker.rs):
      - threads are capped at 2000 replies
      - `stars.list` reads one page
      - section `channel_ids_page` cursors are ignored
      - `users.list` stops at 40 pages

      Follow the cursors, or log when a cap is hit.
      *Done. Section `channel_ids_page` cursors have no call to follow; see
      the follow-ups.*
- [x] **`users_requested` and `bots_requested` are never cleared**, so a
      transient failure means that user is never fetched again
      (`worker.rs:615,648`).
- [x] **Failed images never retry** (`images.rs:182`). Add a backoff.

### Sidebar

- [x] **Local section ids can collide** (`sidebar.rs:334`), and
      `local-*` ids are sent to Slack.
- [x] **Shift and Star edge cases** (`sidebar.rs:356,113`):
      - Shift swaps with hidden neighbours.
      - Starring with no Starred section changes nothing locally but still
        calls Slack.

### Minor

- [x] **Download naming** (`worker.rs:1476`):
      - The `exists()` check is a TOCTOU race; use `create_new`.
      - Names aren't safe on Windows (`<>:"|?*`, `CON`, trailing dots).
      - Long names aren't truncated.
- [x] **The startup error log** comes before logger init and is lost
      (`main.rs:100`).
- [x] **`write_atomic`** (`paths.rs:299`) doesn't fsync, and its fixed
      `.tmp` name races between writers.
- [x] **mrkdwn edge cases:**
      - `<!foo>` is drawn as a broadcast.
      - Word boundaries are ASCII-only.
      - Double-backtick code isn't handled.
      - A quote is lost before a fenced block.
      - Emoji need no boundary, so `10:30:00` matches `:30:`.
- [x] **Small UI errors:**
      - The `short_time` doc comment is wrong (`ui/mod.rs:110`).
      - The rail badge shows "9" instead of "9+" (`sidebar.rs:75`).

## P2: Performance

- [x] **The mrkdwn parser is quadratic.** One 40 KB message of
      `"*a "` takes about 600 ms to parse (release build), and that
      happens every frame (`mrkdwn.rs:190,222`). Make it linear.
- [x] **The whole history is laid out every frame** (`conversation.rs:332`,
      `rich.rs:80`, `message.rs:813`):
      - Use `show_viewport` with cached row heights.
      - Cache parsed blocks per `(ts, edited)`.
      - Build reaction hover text lazily.
- [x] **Composer autocomplete** rescans and re-lowercases every user on
      every frame (`composer.rs:61`). Cache results by query.
- [x] **The emoji picker** filters and paints about 1,900 emoji every frame
      (`overlays.rs:342`). Use `show_rows` and a per-query cache.
- [x] **Settings are written on the UI thread every frame while dragging**
      (`sidebar.rs:268`, `thread.rs:188`, `app.rs:514`). Debounce the save
      and move it off-thread.
- [x] **`sidebar::layout` is recomputed every frame.** Memoise it by a
      generation counter.
- [x] **The disk image cache is never pruned.** Add an LRU size cap.
- [x] **The boot unread sweep** makes 1–2 sequential calls per
      conversation (`worker.rs:1350`). Use `client.counts` or
      `users.counts` where available.

## P2: Accessibility and i18n

- [x] **Custom-painted buttons and rows have no AccessKit info.** Call
      `widget_info` in `theme::icon_button` and the row helpers, and link
      fields with `labelled_by`.
- [x] **Message actions are mouse-only.** Add a focused-message state with
      shortcuts (r, e, Del).
- [x] **Keyboard issues:**
      - Alt+↑/↓ is consumed inside text fields (`keys.rs:30`).
      - Overlays re-grab focus every frame, so Tab can't reach their
        buttons.
      - The edit field saves on Enter even when "Enter sends" is off.
      - The filter's hint says "Ctrl+K", which opens the switcher.
- [x] **i18n gaps:**
      - Dates use English `strftime` names.
      - Some sentences are glued from pieces; use `{name}` placeholders.
      - Emoji group names come from `Debug`.
      - Shortcuts show "Ctrl" on macOS.
      - "Notify everyone here" is shown for @channel and @everyone too.

## P3: Architecture and maintainability

- [x] **Views call `app.backend.send` and `open::that_detached`
      directly** (`login.rs`, `settings.rs`). Route these through
      `Action`s.
- [x] **Split the large functions:**
      - `Worker::command` (about 320 lines)
      - `App::handle` and `App::apply` (220+ lines each)
      - `edit_sidebar`
- [x] **Shared helpers to add:**
      - an active-workspace lookup (copied 5 times)
      - `timelines_for(channel)` (6 copies with inconsistent filtering)
      - a `paginate` helper (4 hand-written loops)
      - `From<reqwest::Error>` for `SlackError`
- [x] **Auth error codes** live in `is_auth`, `describe` and
      `socket::is_fatal`, and the three lists disagree. Use one list.
- [x] **Make `App` testable without a backend**, for example by moving
      the event and action handlers onto `WorkspaceState`.

## P3: Tests

- [x] Hostile-URL tests for `image_uri`, `images::split` and `get_bytes`.
- [x] `redact_tokens`, `describe`, `safe_name` (Windows cases), `backoff`,
      `normalize_workspace`, and `parse_callback` with duplicate params.
- [x] Optimistic reconciliation, `Event::Read` ordering,
      `merge_conversation`, and the edit round-trip.
- [x] Composer `suggestions` and `replace_word` with multibyte text, and
      `keys::step`.
- [x] mrkdwn:
      - unterminated `<`, `<!subteam^…>` and `<!date^…>`
      - a fuzz or proptest target asserting that parsing never panics
      - a 40 KB time-budget test
- [x] Settings: a corrupt file, a newer-version file, and `write_atomic`.
- [x] Sidebar `apply` with local ids, and byte-cache eviction.

## P3: CI, dependencies and packaging

- [x] **CI triggers on `main`, but the working branch is `master`.**
      Rename the branch or add it to the triggers.
- [x] **Two toolchains get installed.** Use the pinned toolchain
      (`@1.98.0` or `rustup show`) instead of `dtolnay/...@stable`.
- [x] **Run clippy and the non-demo build on macOS and Windows too**,
      since the `cfg` code there is never linted.
- [x] **Add `cargo-deny`** for advisories, licenses and a `sources`
      allowlist for the git deps.
- [x] **Pin actions by SHA.**
- [x] **Add a release job** that builds the binaries and packages.
- [x] **Add a `LICENSE` file.** `Cargo.toml` says MIT, but the repo has
      none.
- [x] **Bump `sha1`, `sha2` and `rand`** to the versions the transitive
      deps use, to reduce duplicate crates.
- [x] **Consider not embedding 12.6 MB of emoji fonts**: load them from
      the system, or compress them.
      *Done: nothing on macOS, flags only on Windows, and the full font on
      Linux behind the `bundled-emoji` feature.*
- [x] **Packaging:**
      - The `.desktop` file has `Icon=cloud.yannick.NoSlacking`, but the
        icons are named `noslacking-*.png`.
      - Add AppStream metainfo.
      - Run `desktop-file-validate` in CI.
      - Add macOS and Windows packaging.
      *Done; the macOS bundle and Windows icon are untested off Linux.*

## Features

These are missing compared with the official client, roughly in order of
value. The backend's `Command`s cover sign-in, history, threads, send,
edit, delete, react, upload, download, mark-read, users and sidebar
sections. Nothing below exists yet.

### Expected of any chat client

- [x] **Desktop notifications** for DMs, mentions and keywords, with
      per-channel settings, plus a taskbar/dock unread badge (fastframe-shell
      may help). Without this the client can't replace Slack.
- [x] **Search**: `search.messages` and `search.files`, with
      `from:`/`in:`/`before:` filters and jump-to-message with context.
- [x] **Start conversations**: open a DM or group DM (`conversations.open`),
      browse and join or leave channels, create a channel.
- [x] **Presence and typing**: green dots, `user_typing` over RTM and Socket
      Mode, and setting yourself away.
      *Typing is for browser sign-ins only; Socket Mode sends no typing
      events.*
- [x] **Your own status and DND**: set the status emoji, text and expiry,
      and snooze notifications (`dnd.setSnooze`).
- [x] **Mute and unmute channels**, honouring Slack's muted list so muted
      channels don't count as unread.
- [x] **Persistent drafts** per conversation and thread across restarts,
      plus a Drafts list.
- [x] **Jump to a message**: copy a message link, open `slack://` and
      `https://…/archives/C…/p…` permalinks inside the app, and jump to
      the unread line or to the newest message.

### Productivity views

- [x] **Activity / Mentions**: one list of mentions and replies across
      channels.
- [x] **All unreads**: one list of every unread message.
- [x] **Threads**: a list of threads you follow, with unread replies.
- [x] **Saved for later and reminders** (`reminders.add`, or the newer
      saved-items API on session tokens).
- [x] **Pins and bookmarks** in a channel header panel (`pins.list`,
      `bookmarks.list`).
- [x] **Scheduled messages** (`chat.scheduleMessage`) and "send later" in
      the composer.
- [x] **Channel details**: topic, purpose, members, files, and editing the
      topic.
- [x] **Profile card** on clicking a name: local time, title, status, and
      a "Message" button.

### Composer and rendering

- [x] **Syntax highlighting** in code blocks (e.g. `syntect`, behind a
      feature flag), and a copy button.
- [x] **Formatting toolbar and shortcuts** (Ctrl+B/I/Shift+X), a live
      preview of mrkdwn, and Up-arrow to edit your last message.
      *Done, without the live mrkdwn preview.*
- [x] **Paste images** from the clipboard and drag-and-drop files in the
      composer, with upload progress and cancel.
      *Done. On Wayland the clipboard is read through XWayland.*
- [x] **Spell checking** (system spellchecker, or `hunspell` behind a
      feature).
      *Uses the system's Hunspell dictionaries through `spellbook` (MPL-2.0).*
- [x] **Slash commands** (`/remind`, `/status`, `/invite`) and `#channel`
      autocomplete.
      *`/remind` and app commands need the browser sign-in (`chat.command`).*
- [x] **Rich unfurls** for link previews, video and audio file playback or
      a "play externally" option, and PDF thumbnails.
      *Video and audio open in the system player, not in the app.*
- [x] **Image viewer**: a full-size lightbox with zoom and arrow-key
      navigation through the channel's images.
- [x] **Emoji**: skin tones, recently used, and a frequent-reactions row.

### Desktop integration

- [x] **System tray** with an unread indicator and close-to-tray, and
      start on login.
- [x] **`slack://` URL handler** registration so links open in NoSlacking.
- [x] **Offline cache**: keep recent history on disk so start-up is instant
      and the client works read-only offline. Keep the cache encrypted, or
      at least per-team and wiped on sign-out.
- [x] **Proxy settings** (HTTP or SOCKS) for corporate networks.
- [x] **Multi-window**: pop a conversation out into its own window.
- [x] **Compact / IRC-style density**, and an option to hide avatars and
      images.

### Bigger bets

- [x] **Enterprise Grid / Slack Connect**: shared channels, external user
      badges, and org-level sign-in.
- [x] **Huddles and calls**: probably out of scope. At least show "huddle
      in progress" and open it in the browser.
      *Shows a huddle in progress and opens it in the browser; no calls in
      the app.*
- [x] **Plugins or scripting hooks** for keyword alerts and auto-replies,
      in the spirit of wee-slack.
      *Opt-in hooks that run a program with the message as JSON; see the
      README.*

## Follow-ups found along the way

- [x] **Browser sign-in, inspired by msga**: open `ssb/signin`,
      receive the `slack://` magic link (the handler is registered by
      default and used only during a sign-in you started) or paste it, and
      redeem it with `auth.loginMagicBulk`. Tested against a real
      workspace.
- [x] **Sidebar sections fetched ten times**: Slack repeats the same page
      with a new cursor; the walk now stops.
- [x] **Images answering 404 or 403 were retried**: they now fail for good.
- [x] **`channel_marked` was ignored**: reads on other devices now move the
      read marker here.
- [x] **Sidebar sections show only their first channels** when Slack sends
      a `channel_ids_page` cursor. *Slack has no call for the rest; the
      left-out channels show under Channels or Direct messages, and it is
      logged once.*
- [x] **Your own Slack app sign-in needs PKCE**: `pkce_enabled` in the
      manifest, an `http://localhost` redirect, no client secret.
- [ ] **macOS never receives `slack://` or `noslacking://` links**: the
      bundle declares them, but the open-URL event isn't handled. *It needs
      `unsafe` Objective-C (an app delegate); see CONTRIBUTING.md. The fix
      belongs in fastframe-macos.*
- [x] **Close DMs** (`conversations.close`).
- [x] **The new-message icon touches the sidebar edge.**
- [ ] **DND for your-own-app sign-ins** needs the `dnd:read` and
      `dnd:write` scopes. Adding them breaks apps made from the old
      manifest, so DND is local only for those sign-ins for now. The same
      goes for `usergroups:read`: without it, `@group` isn't suggested;
      and for `bookmarks:write`: without it, changing a bookmark is
      undone with a toast naming the permission.
- [ ] **No Dock or taskbar badge on macOS and Windows**: it would need
      `unsafe` platform calls. The unread count is in the window title.
- [ ] **Test on macOS and Windows**: the platform code is only compiled on
      Linux so far.
- [x] **Right-click menu on messages**, pictures and links, with Copy image.
- [x] **Files wait in the composer** until the message is sent.
- [x] **Slack's emoji names** (`:large_green_circle:` and the like).
- [ ] **Drag and drop on Wayland**: winit 0.30 only delivers dropped files
      on X11, Windows and macOS. Pasting a copied file works; real drops need
      a patch to the winit fork.
- [ ] **`conversations.info` answers `fatal_error`** for one conversation
      in a real workspace; check which kind it is.
- [ ] **Test the batch B features against a real workspace**: presence,
      typing, huddles, the Activity, Threads and Later views (internal
      methods), the proxy and the pop-out window.

## Next, by priority (2026-10-05)

From a review after batch B. Each item says why it sits where it does.
Code health (splitting `app.rs`, `worker.rs` and `ui/message.rs`, typed
errors, demo screenshots in CI) is being done first, so it is not listed.

### P1: Don't miss messages

- [x] **Notify while polling.** Notes come only from `Event::Message`, so
      when the socket is down, new DMs and mentions in `History` and
      `Newer` pages are silent. Run the same `note_for` and hooks over
      messages that a poll finds past the newest one already seen.
      *The client must not fail quietly at the one thing it is for.*
      Done for polls of the open conversation, at most three notes per
      poll. Every other conversation is watched too while the socket is
      down (`backend/poll.rs`): browser sessions ask `client.counts` every
      30 s and fetch the newest page of direct messages and of channels
      whose mention count rose (at most 8 a round), so their unread state
      and notes follow; OAuth sign-ins check 20 direct and group messages
      a minute with `conversations.history` `limit=1`, the busiest ones
      every round and the rest in turn. A conversation never opened
      announces only what is newer than its latest known message.
      Still left:
      - Threads: no poll reads replies, so they wait for the socket.
      - OAuth sign-ins see nothing of channels (unread state or
        mentions) until the socket is back: they have no `client.counts`,
        and a call per channel would cost too much.
      - A conversation the app has never seen (a first DM from someone)
        shows up, but its first messages do not notify while polling.
      - Messages from the first seconds of an outage at start-up
        (before the first round takes its record) do not notify.
- [x] **Hide Cancel during an upload's last step.** Once
      `completeUploadExternal` is sent, the upload can't be taken back, so
      Cancel should go away rather than promise something it can't do.
      *Small, and a broken promise in the UI.*
      Done: the worker says `UploadFinishing`, the row then reads
      "Finishing upload…" without Cancel, and a gate refuses a cancel that
      arrives later, so "Upload cancelled" only shows when nothing was
      posted.

### P2: Everyday gaps against the official client

- [x] **Mark unread** from the message menu and the keyboard (`u`):
      `conversations.mark` with the `ts` just before the message, and the
      read line moves locally. *The read-marker plumbing already exists.*
- [x] **User-group mentions in the composer.** `<!subteam^…>` is drawn and
      notified, but `@team` can't be typed. Fetch `usergroups.list`
      (OAuth needs `usergroups:read`; mind the manifest, as with DND) and
      add groups to the suggestions. *Sessions get them; your-own-app
      sign-ins only if the app happens to have `usergroups:read`, as the
      manifest does not ask for it yet (see the DND follow-up).*
- [x] **Share a message** to another conversation: a picker that posts
      the permalink with an optional comment, which Slack unfurls.
- [x] **Keyboard shortcut sheet** on `Ctrl+/` (⌘/) listing what
      `ui/keys.rs` and the message focus handle. *Many shortcuts, and
      nowhere they are listed.*
- [x] **Hide inactive conversations** in the sidebar, as the official
      client tidies it: after a week, a month (default) or three months
      without a new message, behind each section's "N more" row, which
      expands it until "Show less". Unread, mentioned, starred, open and
      drafted ones, apps and those whose newest message is unknown always
      show; the switcher, search and browse still find them all.
- [x] **Quote Slack permalinks inline.** A link to a message in a known
      conversation shows that message from local data (or one
      `conversations.history` call) instead of a bare link. Slack's own
      `is_msg_unfurl` attachment is drawn as the same quote, never both.

### P3: Worth having

- [x] **Interactive Block Kit buttons.** Browser sessions press an app's
      buttons through `blocks.actions`, as Slack's web client does
      (`backend/blocks.rs`), asking the app's `confirm` question first;
      OAuth sign-ins show them as not pressable with "Open in Slack".
      Still open: test against a real workspace and keep the answer as a
      fixture; static selects and overflow menus; forms an app opens in
      answer (`views.open`), which only Slack itself shows.
- [x] **Add and edit bookmarks** (`bookmarks.add`, `bookmarks.edit`,
      `bookmarks.remove`); today they can only be listed. *Sessions can;
      your-own-app sign-ins only if the app has `bookmarks:write`, which
      the manifest does not ask for yet (see the DND follow-up). Others'
      changes arrive as `bookmark_added`/`_changed`/`_removed` events.*
- [x] **Delete your own files** (`files.delete`) from file cards and the
      channel's Files tab. *Right-click a file you uploaded (a card, a
      picture, a video or a row of the Files tab), confirm, and it goes at
      once; a refusal brings it back with a toast. A deleted file stands as
      "This file was deleted.", as Slack's tombstone does, and `file_deleted`
      keeps it gone. `files:write` was already in the manifest. Not yet
      tried against a real workspace.*
- [x] **Upload custom emoji** (`emoji.add`, session sign-ins only).
      *The emoji picker's + opens "Add emoji": a PNG, JPEG or GIF up to
      128 KB, a name checked like Slack's (lowercase letters, digits, `-`,
      `_`, not taken), a preview. It posts a multipart `emoji.add` to the
      workspace's own address with `token`, `name`, `mode=data` and
      `image`, and the `d` cookie, as Slack's web client does (and as
      jackellenberger/emojme `lib/emoji-add.js` and
      smashwilson/slack-emojinator `upload.py` do). The new emoji shows at
      once and `emoji.list` is fetched again. OAuth sign-ins don't see the
      +. Untested against a real workspace: the request shape and Slack's
      error codes come from those tools and `admin.emoji.add`'s docs.*
- [x] **Send rich text as Slack's composer does.** *`chat.postMessage`,
      `chat.update` and `chat.scheduleMessage` send a `rich_text` block in
      `blocks` beside the mrkdwn `text` (`slack/rich_out.rs`): sections,
      bullet and ordered lists (nested, numbered on with `offset`, quoted
      with `border: 1`), quotes, code blocks, styled text, links, people,
      groups, channels, broadcasts and emoji. The block is read back
      before it goes; if it does not read as the text, or the text holds a
      date, the text goes alone, and if Slack refuses the blocks the call
      is made again without them. The sending copy and an edit show the
      same rich text, and editing starts from a message's rich text.
      `SEND_RICH_TEXT` switches it all off. Not yet tried against a real
      workspace.*
- [x] **Remind me about a message.** *The message menu's "Remind me":
      in 20 minutes, 1 or 3 hours, tomorrow at 9:00, next Monday at 9:00,
      or a time of your own. `reminders.add` has no message parameter, so
      the text quotes the message's first line and carries its permalink.
      `reminders:write` was already in the manifest; an app without it gets
      a toast naming it. The Later view's reminders are read again after.
      Not yet tried against a real workspace.*
- [x] **Follow and unfollow threads** (browser sessions). *The thread
      panel's header has a Follow / Following toggle. Slack has no public
      method; the web client's `subscriptions.thread.add` and `.remove`
      take `channel`, `thread_ts` and `last_read` (as emacs-slack
      `slack-thread.el` sends them). Whether you follow comes from the
      parent's `subscribed` (`conversations.replies` documents it) and the
      `thread_subscribed` / `thread_unsubscribed` socket events. The
      Threads list drops a thread you unfollow, takes in one you follow,
      counts replies past the parent's `last_read` when Slack leaves out
      `unread_replies`, and is read when a session's workspace is ready so
      the sidebar counts unread threads at once. OAuth sign-ins don't show
      the toggle. Not yet tried against a real workspace.*
- [x] **Command palette.** *Type `>` first in the quick switcher
      (Ctrl+K), as in VS Code, rather than learning another chord: mark all
      as read, set a status, away or active, always show as active, the
      theme, settings, the shortcut sheet, new message, browse channels,
      search, pause notifications for an hour, and the next "hide inactive
      conversations" choice. Each sends the action its menu sends; names
      match by start, word, substring or letters in order, and show the
      shortcut sheet's keys when there are any.*

### Blocked or needs a real workspace

These are the open follow-ups above, in the order to take them:

1. Test batch B against a real workspace, and keep the answers as
   fixtures for parse tests, so a change at Slack fails a test rather than
   the client.
2. `conversations.info` and `fatal_error`: log the conversation's kind
   and flags (shared, archived, external) when it happens.
3. Test on macOS and Windows; the demo screenshots in CI cover start-up and
   drawing only.
4. macOS link events, Dock and taskbar badges, Wayland drops: blocked on
   `unsafe` code in fastframe or a winit patch.
5. DND scopes for your-own-app sign-ins: wait for a manifest version bump
   that also brings `usergroups:read` and `bookmarks:write`.

### Found while typing the errors

- [x] **Still untranslated:** `Event::Notice(String)` ("Uploaded {name}",
      "Saved {path}", the browser-step notice), `Event::KeyringError`, and
      `Socket::Disconnected` / `Rejected`. Give them typed payloads too.
- [x] **The catalog scan in `i18n.rs` misses wrapped calls.** It matches
      `t("` on one line, so a `tf(` that rustfmt breaks before its string
      is never checked (`convos.rs`, `views.rs`, `notify.rs`, several UI
      files).
- [x] **A `.po` entry with no blank line before a `#` comment** merges
      into the entry above it, and nothing catches that. Make `build.rs`
      reject it.

## Huddles (researched 2026-10-06)

Doable for browser sign-ins, but full audio is a project of months.
`rooms.join` (`channel_id`, `regions`, `multidevice=true`; the session
token and `d` cookie) answers Amazon Chime meeting and attendee
credentials (`call.free_willy.meeting` / `.attendee`); the rest is a
Chime SDK WebRTC session: a signaling WebSocket to the meeting's
`SignalingUrl` (protobuf `SdkSignalFrame`, from AWS's Apache-2.0
amazon-chime-sdk-js and C++ signaling client), TURN-only ICE and Opus.
HuddleFM (AGPL, read, don't copy) does this without a browser. Slack's
terms call undocumented methods unreliable and forbid reverse
engineering, as for the rest of the session sign-in.

- [ ] **Invitations and live state (days).** A `huddle_invite` becomes a
      notification with Join (opens `app.slack.com/huddle/T/C`) and
      Decline (`rooms.inviteResponse`); reconcile participants with
      `screenhero.rooms.info` after reconnects and now and then, as join
      and leave events go missing; "Open in Slack app" by the huddle
      indicator. First check these events reach the RTM socket at all
      (HuddleFM hears them on the desktop "flannel" gateway).
- [ ] **Listen-only spike (1–2 weeks), behind a `huddle-audio` feature:**
      join, receive Chime's mixed audio and play it (`str0m` or
      `webrtc-rs`, `opus`, `cpal`), to prove the path and judge echo
      cancellation (`webrtc-audio-processing`) before going further.
- [ ] **Two-way audio (4–8 weeks more, plus 2–4 hardening)**, only if the
      spike holds up: microphone with echo cancellation and noise
      suppression, mute, devices, who is talking, reconnects. Video and
      screen viewing after that (+4–8 weeks).

## Media and file previews in the app (researched 2026-10-06)

Slack already makes most previews; NoSlacking parses few of them.

- [x] **Use what Slack gives (small, no new crates).** Parse `mp4`,
      `mp4_low`, `aac`, `converted_pdf`, `duration_ms`,
      `audio_wave_samples`, `transcription`, `subtype`, `preview` /
      `preview_plain_text`, `lines_more`: snippet preview cards, voice
      clip cards with a waveform, duration and the transcript's start, a
      duration on video stills, and the smaller `mp4_low` / `aac` for
      "open in player".
      - Done: text and code cards (Slack's `preview`, at most 8 lines,
        coloured by `filetype` or extension, "N more lines"); "Open as
        PDF" for Office files with `converted_pdf`, whose `thumb_pdf`
        still now opens that PDF instead of failing; voice clip cards;
        video lengths; `mp4_low` / `aac` for the player. Every card has
        a height known before it is drawn. `mp4`, `hls`, `vtt` and the
        larger image thumbnails are left for the players and viewer
        below, which will need them.
- [x] **A viewer for spreadsheets, CSV, zip listings and whole text files
      (medium).** `calamine` (MIT) and `csv`; a read-only
      `egui_extras::TableBuilder` grid; caps on download size, rows,
      columns, zip entries and compression ratio, like
      `images::check_decoded_size`.
      - Done: "View" on a file's card opens `ui::viewer`, an overlay over
        the window: sheet tabs, column letters, row numbers and a frozen
        first row for `.xlsx`, `.xlsm`, `.ods`, CSV (separator sniffed)
        and TSV; a zip's listing from its central directory; text and
        code with line numbers, colour and find. The worker downloads at
        most 20 MB into memory and parses on a blocking thread
        (`src/viewer.rs`). Caps: 50,000 rows, 500 columns, 64 sheets, 2
        million cells, 1,000 characters a cell, 200,000 lines of 5,000
        characters, 10,000 entries listed (100,000 declared at most);
        workbooks are refused past 200 MB unpacked, a ratio of 200 in a
        part over 1 MB, a part unpacking to more than it declares
        (measured first), 2 million shared strings, or 4 million cells
        an `.ods` table's repeats would spell out. Not opened: `.xls`
        and `.xlsb`, whose calamine readers slice records and reserve
        memory as the file claims; with `panic = "abort"` one bad file
        would close the app. Reading them in a child process would
        make them safe.
- [ ] **Audio in the app (small–medium).** `rodio` + `symphonia` (aac,
      isomp4, mp3, vorbis, flac, wav; no Opus) with `cpal`; the packages
      need ALSA, the Flatpak `--socket=pulseaudio`.
- [ ] **Every page of PDFs and Office files (medium)** through Slack's
      `converted_pdf`, rendered with `hayro` (pure Rust, experimental),
      falling back to `thumb_pdf` and "open".
- [ ] **Video (large).** GStreamer (`gstreamer-rs`, the libraries LGPL
      and from the system) behind a `video` feature for Linux, the
      packages and the Flatpak (whose runtime has it); macOS and Windows
      keep the system player. Fetch with our own client so the token
      never leaves the process; prefer `mp4_low`. Not mpv (LGPL plus
      `unsafe` GL) nor FFmpeg bindings (WTFPL, painful builds).
