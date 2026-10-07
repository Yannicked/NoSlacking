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
- [x] **DND for your-own-app sign-ins** needs the `dnd:read` and
      `dnd:write` scopes, `@group` suggestions `usergroups:read`, and
      bookmark editing `bookmarks:write`. *Manifest version 2 asks for all
      four (and the `dnd_updated` event); its description says "manifest
      v2". Sign-in asks for them too; if Slack refuses them
      (`invalid_scope` or `unapproved_scope`), it asks again without them
      and remembers the app is an older one, and the waiting sign-in offers
      that by hand in case Slack shows an error page instead. The scopes
      Slack grants (`authed_user.scope`, then each answer's
      `x-oauth-scopes` header) are kept per workspace in the settings, and
      DND, groups and bookmark editing are offered only when granted;
      `missing_scope` still undoes a change when the grant is not known.
      Settings → Workspaces says what an older app lacks, with the manifest
      to copy and Sign in again. Not yet tried against a real workspace:
      whether Slack refuses or quietly grants scopes the app lacks.*
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
      sign-ins when the app has `usergroups:read`, which manifest version 2
      asks for (see the DND follow-up).*
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
      fixture; forms an app opens in answer (`views.open`), which only
      Slack itself shows.
- [x] **Static selects, overflow menus and radio buttons** in app
      messages, in sections and `actions` blocks. *A choice goes through
      `blocks.actions` like a press, with `selected_option` (`text`,
      `value`), a select's `placeholder`, and `state` for selects and
      radio buttons, as emacs-slack, slack-user-cli and rho send it.
      Option groups, initial choices, descriptions, `confirm`, busy state
      and a failure toast; an overflow choice with a `url` opens it on any
      sign-in. OAuth sign-ins show them disabled with "Open in Slack".
      Selects fed by the app (`external_select`), user, channel and
      conversation selects, checkboxes, pickers and inputs show as working
      only in Slack. Not yet tried against a real workspace: the payload
      comes from those clients, not a capture of our own. Option text is
      always sent as `plain_text`, though radio buttons may use mrkdwn.*
- [x] **Add and edit bookmarks** (`bookmarks.add`, `bookmarks.edit`,
      `bookmarks.remove`); today they can only be listed. *Sessions can;
      your-own-app sign-ins when the app has `bookmarks:write`, which
      manifest version 2 asks for; without it the controls are hidden (see
      the DND follow-up). Others' changes arrive as
      `bookmark_added`/`_changed`/`_removed` events.*
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
- [x] **Custom emoji added elsewhere show without a restart.**
      *`emoji_changed` (`backend/translate.rs`) adds (`add`, a picture or
      an `alias:`), removes (`remove`, with the aliases of what is
      removed) and renames (`rename`, aliases following) in the
      workspace's emoji set; no subtype or an unknown one fetches
      `emoji.list` again, as Slack's docs ask. Not yet seen from a real
      workspace: the shapes are those of the docs' examples.*
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
5. Manifest version 2 against a real workspace: sign in with an app made
   from the first manifest and see whether Slack refuses the newer scopes
   or grants what the app has; keep the `oauth.v2.access` answer as a
   fixture.

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

- [x] **Invitations and live state (days).** A `huddle_invite` becomes a
      notification with Join (opens `app.slack.com/huddle/T/C`) and
      Decline (`rooms.inviteResponse`); reconcile participants with
      `screenhero.rooms.info` after reconnects and now and then, as join
      and leave events go missing; "Open in Slack app" by the huddle
      indicator. First check these events reach the RTM socket at all
      (HuddleFM hears them on the desktop "flannel" gateway).
      - Done, for browser sign-ins (`src/huddles.rs`,
        `src/backend/huddles.rs`): an invitation card with Join and
        Decline, a desktop notification whose click joins (off during Do
        Not Disturb, for muted conversations and while the window is in
        front), gone when answered, when the huddle ends or you join it
        elsewhere, or after 45 s (the event names no ring time). Room-only
        `sh_room_join` / `_leave` / `_update` change the participants;
        `screenhero.rooms.info` checks the huddle in the open conversation
        every 3 minutes (doubling after failures, at most 30) and every
        known huddle after an RTM reconnect. Socket Mode carries none of
        this.
      - Verified 2026-10-06: a real invitation reached NoSlacking's
        `rtm.connect` socket and showed its card, so the flannel gateway
        is not needed for invitations. The card offers
        Listen here (opens the conversation and listens) and Open in Slack.
      - Seen on the same socket in a real huddle: `huddle_invite_cancel`
        (the call stopped ringing: its card goes, and on Linux its
        notification; its fields are not documented, so the room is read
        from `call_id`, `room_id`, `room` or `huddle`, the conversation
        from `channel_id` or `channel`, and the keys are logged when none
        is there), `sh_room_update` (now read from the full `room` beside
        a short `huddle` object, or by room as the whole participant
        list; a room that names no participants ends nothing), and
        `user_huddle_changed`, `activity`, `badge_counts_updated`,
        `search_recents`, ignored quietly. To check against a real log:
        that `huddle_invite_cancel` carries one of those fields, and that
        `sh_room_update` updates the header's count (the log names the
        keys of one not understood).
      - Was unverified: `sh_room_*` reach `rtm.connect` sockets (slack-go's
        RTM maps them), but no open-source `rtm.connect` client handles
        `huddle_invite`; HuddleFM hears it on the flannel gateway
        (`wss-primary.slack.com`, `flannel=3`). If RTM never carries it,
        the next step is that gateway.
      - Skipped "Open in Slack app": after a browser sign-in NoSlacking
        stays the `slack://` handler, so `slack://channel?…` would come
        straight back here. It needs the claim given back first.
- [x] **Listen-only (1–2 weeks), first behind a feature, now in every
      build:**
      join, receive Chime's mixed audio and play it (`str0m` or
      `webrtc-rs`, `opus`, `cpal`), to prove the path and judge echo
      cancellation (`webrtc-audio-processing`) before going further.
      - Works against Slack (2026-10-06): a live huddle was heard in the
        app, 0 late or concealed frames. The path, as it ran:
        `rooms.join` → Chime signaling JOIN/JOIN_ACK → TURN relay over
        UDP → INDEX → SUBSCRIBE/SUBSCRIBE_ACK → ICE → DTLS (OpenSSL's) →
        SRTP_AEAD_AES_256_GCM → Opus. Then AUDIO_STREAM_ID_INFO (one
        stream per attendee, external user ids `TEAM-ROOM-USER`, ours
        with a device suffix), AUDIO_METADATA several times a second,
        INDEX again as people came and went (participants=2, then 1).
        Answered by the probe's open questions: `regions` takes one Chime
        region (the nearest, `eu-central-1` here); `free_willy` is Chime's
        PascalCase; Chime takes `str0m`'s rewritten offer, the muted
        SUBSCRIBE and `receive_stream_ids: [0]`; `rooms.leave` is not a
        method Slack takes (`invalid_arguments`), so leaving is Chime's
        LEAVE alone, as with HuddleFM, and the guess is gone.
      - DTLS: Chime's media servers refused dimpl's ClientHello (alert
        40, handshake_failure) and took OpenSSL's. Every other `str0m`
        backend uses dimpl (`wincrypto` always does, and `apple-crypto`
        depends on it), so huddles now use OpenSSL on every
        platform: the system's on Linux (`libssl-dev` to build), built
        from source (`vendored`, openssl-src: Perl, and NASM where the
        runner has it) on macOS and Windows, unproven there until CI
        builds it. aws-lc, rcgen and the RustCrypto duplicates left the
        tree. The log names the DTLS version, the SRTP profile and the
        media server's key (`src/huddle_audio/dtls.rs`); str0m-openssl
        does not hand out the cipher suite. That key and a ClientHello
        capture are what patching dimpl upstream would start from.
      - Built (`src/huddle_audio/`): `rooms.join` → `ChimeJoin` (join
        token redacted); Chime signaling (the vendored Apache-2.0 proto,
        compiled ahead of time by `tools/chime-protogen` with `protox`,
        so no `protoc`) as a state machine; our own TURN client (Allocate
        with the long-term credential, CreatePermission, Send/Data; UDP,
        then TLS, then TCP); `str0m` with the relay as its only
        candidate; Opus through a jitter buffer (60 ms, concealment,
        trimming past 300 ms) to the default device at the system's
        volume. Muted: SUBSCRIBE says so and only Opus silence goes out.
      - In the app: "Listen" beside a browser sign-in's "Huddle · N
        people", or Listen here on an invitation. A call bar at the foot
        of the sidebar (and of the settings page) shows the huddle from
        joining until it is left: where (a click opens the conversation),
        Joining…, Live with the time, or why it failed (Try again,
        Close); who is in it, the speaking ringed and the muted marked
        (Chime's attendees by the `U…` of their external id, speaking from
        AUDIO_METADATA, sent to the window at most four times a second
        and only when it changes); Leave (also Ctrl+Shift+H) and Open in
        Slack, to talk. One huddle at a time; left on sign-out and on quit
        (the app waits up to 4 s for LEAVE_ACK), by itself when no one
        else has been in it for a minute (INDEX and the streams agree)
        with "Everyone else left the huddle", and when Chime ends the
        meeting (close 4410, audio status 410) with "The huddle ended".
        `--demo --demo-view listening` shows the bar.
      - To check in a real huddle: the speaking ring follows who talks
        (Chime's volume is read as decibels below full scale, 0 the
        loudest, silent from 42 as in its JS SDK; if it rings the silent,
        that reading is upside down); the bar's faces match the header's
        count; leaving alone after a minute; "The huddle ended" when the
        last other person ends it.
      - Checked offline: frame round trips, the join as data, RFC 5769's
        STUN vectors, the SDP both ways through a second `str0m`, ICE,
        DTLS and Opus through the relay against a pretend TURN server,
        the roster and alone decisions, and (ignored by default, loopback
        sockets) the whole session against a pretend Chime: `cargo test
        --all-features -- --ignored loopback`.
      - WebRTC: **`str0m`**. Sans-IO, so the TURN relay is ours to put
        under it, and its SDP and ICE are small enough to read. Neither it
        nor `webrtc-rs` (0.21, now on its sans-IO `rtc` crates) relays
        over TCP or TLS (`webrtc-rs` skips non-UDP TURN URLs), and a TURN
        client is a few hundred lines with RFC test vectors, so that
        decided it.
      - Opus: **`opus-decoder`** (pure Rust, `forbid(unsafe_code)`, no C,
        MIT/Apache, passes the 12 RFC 8251 vectors by its own account,
        but young: 0.1, March 2026). libopus through the `opus` crate
        builds its bundled C with CMake on Windows and macOS (or needs
        the system's library on Linux, which the Flatpak runtime has);
        it is the fallback if the decoder sounds wrong. It has not.
      - Cost: str0m, str0m-openssl, OpenSSL, prost and the Opus decoder;
        `cargo deny` passes as is. Every build carries it (there is no
        feature to leave it out). The Linux x86-64 release binary is
        49.5 MB with it (47.2 MiB; libssl and libcrypto linked from the
        system), against 52.7 MB with aws-lc and dimpl before.
      - Try it from the command line: `cargo run --release --
        --huddle-probe TEAM CHANNEL [--seconds 30]
        [--huddle-region REGION]`, with the browser sign-in saved for
        TEAM. Without `--huddle-region` the region is the nearest one AWS
        names at `nearest-media-region.l.chime.aws`, else `us-east-1`. It
        joins (and so starts one, if none is going on: use a quiet channel
        or a DM), plays for N seconds, leaves (also on Ctrl+C), and ends
        with a summary and "probe: OK" or "probe: FAILED at <step>". More
        detail: `--verbose`, and
        `NOSLACKING_LOG=noslacking=debug,str0m=debug,info` for ICE and
        DTLS. The log is also in the state folder's `noslacking.log`.
      - Not done: reconnecting, the TURN control URL (JOIN_ACK carries
        the credentials in SDK 3.31), TURN through a proxy,
        zlib-compressed SDP, how Chime's clock drifts against the
        device's over a long call (the jitter buffer trims, never
        stretches).
- [ ] **Two-way audio (4–8 weeks more, plus 2–4 hardening)**, next now
      that listening works: the microphone (an Opus encoder, the SUBSCRIBE
      unmuted, Chime's AUDIO_CONTROL for mute), echo cancellation and
      noise suppression, devices, reconnects. Who is talking is done (the
      call bar). Video and screen viewing after that (+4–8 weeks).
      - Built, unproven against Slack (steps 1 and 2):
        microphone → 48 kHz mono → WebRTC's audio processing → Opus → the
        audio track we already send silence on. `src/huddle_audio/`:
        `microphone` (cpal's default input, opened on a thread of its own
        only while unmuted, closed on mute and on leaving; `MicControl`
        is that rule, tested against a pretend device), `processing`
        (**`sonora`** 0.2, WebRTC M145 in pure Rust: high-pass, AEC3,
        noise suppression at High, AGC2's adaptive digital gain; the far
        end tapped in `speaker` as it is decoded; stream delay guessed
        from cpal's input latency + 40 ms, AEC3 measures the rest; the
        sinc resampler for 44.1 kHz microphones; AGC2's RNN VAD for DTX),
        `encoder` (**`opus-rs`** 0.1.34, libopus 1.6 in pure Rust, VOIP,
        32 kbit/s VBR, behind an `Encoder` trait), `uplink` (10 → 20 ms
        framing, DTX outside the encoder: 200 ms hangover, then one frame
        in 20; RTP time +960 a frame counting the frames left out, the
        marker bit after a gap; RFC 6464 levels, which `str0m` writes
        when the answer takes `ssrc-audio-level`, as its offer asks).
        Muting is signalled as the JS SDK does (`DefaultSignalingClient
        .mute`: an AUDIO_CONTROL frame with `muted`, on every mute and
        unmute; SUBSCRIBE carries the state at the time), while silence
        keeps flowing. In the app: a mute button in the call bar beside
        Leave (red while live; Cmd+Shift+Space), joined muted; your own
        face there shows the microphone as it really is.
        `--huddle-probe … --send-tone` joins unmuted and sends a quiet
        440 Hz tone instead of the microphone, logging what was sent and
        Chime's RTCP receiver reports every 5 s. The loopback test has the
        pretend Chime decode our Opus and hear the mute.
      - In-band FEC is off, an upstream `opus-rs` bug to report: with
        0.1.34, `OpusEncoder::new(48000, 1, Application::Voip)`,
        `use_inband_fec = true` and `packet_loss_perc = 10`, one second of
        a 0.3 sine at 440 Hz decodes to 3.9 times the energy put in (5.2
        times for a 120 Hz harmonic "vowel"; 2.7 to 3.3 with CBR; 1.8 to
        4.6 across 16–40 kbit/s), frame levels swinging from -1 to -50
        dBFS where -14 went in. `opus-decoder` and `opus-rs`'s own
        decoder agree sample for sample, so the encoder's LBRR is at
        fault. With FEC off: 0.95 to 0.98, as it should be; white noise
        is fine either way. Turn it on when fixed;
        `encoded_speech_decodes_back_with_both_decoders` shows it.
        Playing stays on `opus-decoder`: `opus-rs`'s decoder conceals a
        loss but cannot decode FEC.
      - Cost: `opus-rs` (no dependencies; its unsafe is SIMD and
        unchecked indexing), `sonora` and its six crates (unsafe only in
        SIMD behind runtime CPU detection), `derive_more`; all
        BSD-3-Clause or MIT/Apache, `cargo deny` passes. Talking added
        4.6 MB to the release binary (49.3 to 54.0 MB, Linux x86-64,
        measured before the move to OpenSSL).
      - Packaging: macOS's Info.plist has `NSMicrophoneUsageDescription`
        (ad-hoc signed without the hardened runtime, so no entitlement);
        the Flatpak's `--socket=pulseaudio` carries recording too; on
        Windows, a refused microphone says to check the privacy settings.
      - Try it: first `cargo run --release -- --huddle-probe TEAM
        CHANNEL --seconds 60 --send-tone` (the others
        should hear a quiet 440 Hz tone; the log shows what was sent and
        Chime's receiver reports), then `cargo run --release`, Listen
        and Unmute: with headphones (is the voice
        clear, the level right?), then without (does the far end hear
        itself back?).
      - Not done: choosing the input device (the system's default for
        now), a level meter, reconnects, a microphone that fails while
        open (it logs and goes quiet; mute and unmute again).
- [ ] **File upstream: opus-decoder's collapse mask overflows.** A real
      huddle stopped playing in a debug build: opus-decoder panicked on
      the sound device's thread. Fixed in our patched copy
      (`vendor/opus-decoder`, see its VENDORED.md; still unfixed on
      Rusopus main `ecb22cf` as of 2026-10-07); drop the copy once a
      fixed release is out. To file at
      <https://github.com/TadeuszWolfGang/Rusopus/issues>:

      > **`extract_collapse_mask` overflows its `u8` with 16 short blocks
      > (attempt to shift left with overflow, celt/vq.rs:118)**
      >
      > opus-decoder 0.1.1, `src/celt/vq.rs`:
      >
      > ```rust
      > fn extract_collapse_mask(iy: &[i32], n: usize, b: usize) -> u8 {
      >     ...
      >     let mut mask = 0u8;
      >     for i in 0..b {
      >         ...
      >         if nonzero != 0 {
      >             mask |= 1 << i; // line 118
      > ```
      >
      > `b` can be 16: a transient 20 ms CELT frame has 8 short blocks,
      > and a negative `tf_change` doubles them once more (the
      > `time_divide` step before `quant_partition_mono`, undone later by
      > `post_tf_collapse_mask`). With overflow checks on
      > (any debug build) `1 << i` panics for `i >= 8`:
      >
      > ```
      > thread 'cpal_alsa_out' panicked at
      > opus-decoder-0.1.1/src/celt/vq.rs:118:21:
      > attempt to shift left with overflow
      > ```
      >
      > libopus keeps this mask in an `unsigned` (`celt/vq.c`,
      > `static unsigned extract_collapse_mask(int *iy, int N, int B)`,
      > and `alg_unquant` returns `unsigned`); the bits above 8 are folded
      > down by `quant_band`'s `cm |= cm >> B` after the time-divide
      > Haar steps, and only the final mask is stored as `unsigned char`.
      > This crate's `post_tf_collapse_mask` already works on a `u32`, so
      > the fix is to make `AlgUnquantResult::collapse_mask` and
      > `extract_collapse_mask` `u32` (the `as u32` at bands.rs:1301 then
      > goes).
      >
      > Reproduce: decode this 55-byte packet with
      > `OpusDecoder::new(48_000, 2)` and `decode_float(.., false)` in a
      > debug build:
      >
      > ```
      > f8 75 d5 48 6a 8c cf b8 7d b1 d2 3b 43 7d 6b 6b 17 c1 14 fe 7d
      > a5 ae 93 56 58 c4 69 d1 30 da 3f 75 ab 8e ab 1c 2c 0e f2 e7 e0
      > 6b f5 08 84 7d 51 67 f9 16 30 90 1d 29
      > ```
      >
      > About 1.4% of random 20 ms CELT packets hit it. In release builds
      > the shift wraps to bit `i % 8`; after the fold that gives the same
      > final mask, so the output matched a `u32` build bit for bit over
      > 20,000 random packets. The panic is the only harm, but it takes
      > down the audio thread of whatever is decoding.
- [ ] **File upstream: a malformed hybrid packet panics opus-decoder in
      release builds too.** Fixed in our patched copy
      (`vendor/opus-decoder`, libopus's `len*8 < ec_tell` check; still
      unfixed on Rusopus main `ecb22cf`). Found by
      fuzzing while fixing the above: about 1 in 80,000 random packets.
      Our release builds abort on a panic, so any huddle participant (or
      Chime) sending such a packet closes NoSlacking for everyone
      listening; SRTP rules out corruption on the way, not a hostile
      sender. Debug builds catch it now (`audio::guard`). Until upstream
      fixes it we carry the copy. To file:

      > **Hybrid redundancy longer than the frame: `range start index
      > out of range` (lib.rs:676 and :724)**
      >
      > In `OpusMode::Hybrid`, `redundancy_bytes = ec.dec_uint(256) + 2`
      > is read from the packet and then used as
      > `&frame[frame.len() - redundancy_bytes..]` without checking it
      > against `frame.len()`. A frame shorter than the redundancy it
      > claims underflows the subtraction and panics ("range start index
      > 18446744073709551534 out of range for slice of length 76" in a
      > release build).
      >
      > libopus (`src/opus_decoder.c`, `opus_decode_frame`) checks
      > right after reading it:
      >
      > ```c
      > len -= redundancy_bytes;
      > /* This is a sanity check. It should never happen for a valid
      >    packet, so the exact behaviour is not normative. */
      > if (len*8 < ec_tell(&dec))
      > {
      >    len = 0;
      >    redundancy_bytes = 0;
      >    redundancy = 0;
      > }
      > ```
      >
      > Reproduce with a fresh `OpusDecoder::new(48_000, 1)` and
      > `decode_float(.., false)`, release or debug, on this 153-byte
      > packet (TOC 0x69, two 76-byte hybrid frames):
      >
      > ```
      > 69 af 0e e6 85 96 57 1b b5 8c c1 a2 21 35 a5 94 3d 0d 19 d1 c8 e7
      > 7c d0 63 03 bd c4 31 df 4e 76 64 f8 27 93 e6 c1 2e 44 06 6c 2e a8
      > df a2 5d 8f b0 e1 a8 8f ce 1f d7 8a 47 af 68 f8 71 37 f5 9e 65 a3
      > 2a 18 28 26 82 e1 88 a7 b8 27 ad 60 fa 63 9d 18 42 b4 b7 92 e3 60
      > 34 5e 40 7e 7c ee 8b 98 8f 1c de 63 ad 44 ce 75 0b 2f 15 f7 be 4f
      > 2d a3 9e 57 bb a8 dd bc f9 0a 3a 14 c2 73 f7 34 98 bb 54 28 c4 db
      > fe 4d 7f 97 0a 58 92 0b 47 59 37 15 85 30 e0 b1 78 d8 9f 26 72
      > ```
      >
      > A decoder fed by the network should turn this into an error (or
      > libopus's behaviour), never a panic.

## Research notes

- **Huddle video:** [docs/research/huddle-video.md](docs/research/huddle-video.md)
  (2026-10-07). Watching is realistic on the audio path; sending is much
  more.
  - [x] Stage 0 built, unproven against Slack: every `--huddle-probe`
        run logs INDEX (sources, `#content` shares, the codec
        intersection), PAUSE/RESUME, BITRATES and DATA_MESSAGE topics;
        `--video N` renegotiates `recvonly` m-lines, re-SUBSCRIBEs and
        logs codec, SPS, frames, keyframes and gaps per stream, and
        whether audio kept flowing; `--video-h264-only` offers no VP8;
        `--video-dump DIR` keeps the first 300 frames. What to run is in
        the research note's Stage 0.
  - [x] Run it against Slack (2026-10-07): shares are `#content`
        sources in H.264 constrained baseline, 1080p at about 12 fps;
        cameras H.264 CB 480×480; the re-SUBSCRIBE works with audio
        going on; keyframes come after a PLI.
  - [x] Stage 1 built, behind `huddle-video` (off by default), not yet
        tried against Slack: the call bar lists who shares with Watch;
        the call window (a native window) shows the share fitted, with
        tabs for two shares; only the share watched is received; H.264
        decoded in pure Rust (`rusty_h264-decoder`, chosen over
        `rust_h264` after a spike: both bit-exact, rusty about 3 ms per
        1080p frame against 10) on a thread of its own, the newest
        picture kept, a PLI on loss or error. The comparison and timings
        are in the research note's Stage 1.
  - [ ] Try Stage 1 with a colleague sharing a screen
        (`cargo run --release --features huddle-video`, press Watch):
        how soon the picture comes, minutes of clean decoding, Watch
        switching between two shares, closing and the share ending, CPU.
  - [x] Stage 2 built, behind `huddle-video`, not yet tried against
        Slack: camera tiles in the call window (up to 9, as many as fit),
        recent speakers first with stable places, the layer by tile size,
        paused cameras shown by their face, one decoder thread for all
        cameras (3 % of a core for 4, 8 % for 9). Details in the
        research note's Stage 2.
  - [ ] Try Stage 2 with colleagues' cameras on (press Video in the
        call bar): faces match names, tiles follow who speaks, cameras
        turned off and on, pause and resume, 5 or more cameras.
  - [x] Stage 3 built, behind `huddle-camera` (off by default), not yet
        tried against Slack: a Video button beside Mute (Ctrl+Shift+O), the
        camera opened only while on (nokhwa: V4L2, AVFoundation, Media
        Foundation), H.264 constrained baseline 640×480 at 30 fps (15 at
        first; raised 2026-10-07) from
        `rusty_h264-encoder` (pure Rust, about 4 ms a picture, chosen
        after a spike) on its own thread, slot 0 `sendrecv` with a DUPLEX
        re-SUBSCRIBE, keyframes on PLI/FIR and every 4 s, bitrate from
        str0m's estimate, view only (206) turning it off with a toast,
        a mirrored self-preview in the bar and a "you" tile in the call
        window. Details in the research note's Stage 3.
  - [ ] Try Stage 3 against Slack: first the probe with the test
        picture (`cargo run --release --features huddle-camera --
        --huddle-probe TEAM CHANNEL --send-test-video`; does Slack's
        desktop, web and mobile app show it, sharp, with the clock
        moving?), then the app (`cargo run --release --features
        huddle-camera`, press Video). Check the light goes out on turning
        it off and on leaving, a camera in use or not allowed, the
        macOS permission prompt, Windows' privacy settings, and the
        log's bandwidth estimate (is TWCC or REMB there?).
  - [ ] The camera in the Flatpak: the Camera portal (`ashpd`'s
        `desktop::camera`, a PipeWire fd read with the `pipewire` crate,
        which needs libpipewire and libclang to build) instead of raw
        V4L2, which would need `--device=all`.
  - [ ] Choosing the camera (Settings) when there is more than one;
        today the first is taken.
- **Microsoft Teams:** [docs/research/microsoft-teams.md](docs/research/microsoft-teams.md)
  (2026-10-06). Not being built: the official Graph route can't do live
  updates or calls, and the route other clients take signs in as
  Microsoft's own Teams app to get past an organisation's app controls.

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
        an `.ods` table's repeats would spell out. In 0.1.0.
- [ ] **Read viewer files in a child process (0.2).** Release builds
      abort on a panic, so a calamine panic on a crafted xlsx/ods from
      someone in the workspace closes the whole app; the caps bound memory
      but not panics. A helper process (the same binary with a
      `--parse-file` mode, bytes on stdin, the `Document` back on stdout,
      a time and memory limit) turns a panic into "could not read this
      file".
      - With it, `.xls` and `.xlsb` can open too. They are left out now
        because calamine reserves memory from their declared sizes and
        slices their records unchecked.
- [x] **Audio in the app (small–medium).** `rodio` + `symphonia` (aac,
      isomp4, mp3, vorbis, flac, wav; no Opus) with `cpal`; the packages
      need ALSA, the Flatpak `--socket=pulseaudio`.
      - Done (`src/audio.rs`, in every build): voice
        clips and sound files play on their cards: play/pause, the
        waveform (or a bar) fills in and seeks on click, position /
        length. The worker fetches the sound into memory (50 MB cap);
        a thread of its own decodes and plays it, opening the device
        only while something plays. One sound at a time; signing out
        stops it. Opus/WebM, larger files, no sound device and
        undecodable files open in the system's player with a toast.
- [ ] **Every page of PDFs and Office files (medium)** through Slack's
      `converted_pdf`, rendered with `hayro` (pure Rust, experimental),
      falling back to `thumb_pdf` and "open".
- [ ] **Video (large).** GStreamer (`gstreamer-rs`, the libraries LGPL
      and from the system) behind a `video` feature for Linux, the
      packages and the Flatpak (whose runtime has it); macOS and Windows
      keep the system player. Fetch with our own client so the token
      never leaves the process; prefer `mp4_low`. Not mpv (LGPL plus
      `unsafe` GL) nor FFmpeg bindings (WTFPL, painful builds).
