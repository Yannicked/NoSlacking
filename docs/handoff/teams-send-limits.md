# Brief: send within a Teams meeting's frame rate and picture size

**Done (2026-10-08)**, as suggested below: `SendControl::set_max_fps` and
`set_max_size`, the `Gate` paced by the lower rate, the helper's
`Request::SetMaxSize` (protocol 7; a new size starts with a keyframe, and
a still screen's held picture is put in again so it changes size too),
and `StreamControl::limit` reading `max-br`, `max-fps`, `max-fs` (a 16:9
box, `frame_box`) and `max-mbps`. The log says "Teams meeting: our screen
share to send at most 825 kbit/s, 15 a second, 1920x1080", then the
sender's "pictures at most …" and "… pictures a second" when they change.
Not yet seen against a real meeting.

**Also (2026-10-08): the bandwidth estimate in Teams calls.** Calls with
video turn on str0m's send-side estimate (`enable_bwe`) from the far
end's transport-cc feedback (or REMB); each estimate, less 80 kbit/s
for the audio, goes to what we send, two thirds to the share when the
camera goes too (`media::split_estimate`). A meeting's `max-br` is now
a ceiling over the estimate (`SendControl::set_max_bitrate`), and the
target too until an estimate comes. With no estimate 10 s into sending
video, a share goes at a fixed 1.5 Mbit/s (it stayed at the 600 kbit/s
start before). The log says "media: send bandwidth estimate N kbit/s",
or "no send bandwidth estimate" once.

For whoever picks this up. It needs changes to the sending side of the
video pipeline (`src/huddle_audio/camera_send.rs`, `share_send.rs`, the
helper's protocol in `crates/video-ipc` and the helper in
`crates/noslacking-video`). The Teams side, which reads the limits, is
done and only needs wiring to what you add.

## What the meeting asks

While we send video in a Teams meeting (our camera, or our screen share),
the meeting's media server pushes `call/controlVideoStreaming` about once a
second. Recorded in `docs/research/teams-calls.md` §H.10 and §H.11:

```json
{"controlVideoStreaming": {
  "sequenceNumber": 2,
  "controlInfo": [{
    "control": 0,
    "sourceId": 2293,
    "fmtParams": "max-mbps=135000;max-fps=1500;profile-level-id=42C02A;max-br=825;packetization-mode=1;max-fs=8160"
  }]
}}
```

| parameter | meaning | unit |
|---|---|---|
| `max-br` | the most bitrate | kbit/s |
| `max-fs` | the largest picture | 16×16 macroblocks (8160 = 1920×1088, 3600 = 1280×720, 920 = 640×360, 240 = 320×180) |
| `max-fps` | the most pictures a second | hundredths (1500 = 15, 3000 = 30) |
| `max-mbps` | the most macroblocks a second | macroblocks/s (size × rate) |

The values move during a share (recorded: `max-br` 2535, then 825, then
748 within three seconds). Nothing has to be answered; a 200 to the push
is the whole reply, and the app already sends it.

## What is done

- `src/teams/calling/types.rs`: `Push::ControlVideoStreaming`,
  `StreamControl` (with `max_bitrate()`).
- `src/teams/calling/call.rs`, `Push::ControlVideoStreaming` arm: matches
  `sourceId` to our own camera's or share's stream (from the roster, under
  our endpoint id: `InMeeting::own_camera`, `own_share`). It applies
  `max-br` through `MediaSession::limit(share, bitrate)`, which reaches
  `CallVideo::limit` and `SendControl::set_bitrate`. It logs and acts only
  when a value changes.
- `max-fs`, `max-fps` and `max-mbps` are read by nobody yet.

## What is missing, and why

`SendControl` (`camera_send.rs`) carries two things from the session to
the sending thread, a keyframe wish and a bitrate. The thread passes the
bitrate to the helper with `Request::SetBitrate` (`crates/video-ipc`,
re-tuned every `RETUNE_EVERY`). Frame rate and size are fixed at start:

- **Frame rate**: the thread's `Pace` (`Pace::CAMERA`: 30 a second;
  `share_send::PACE`: 15, `share::FPS`) decides when to ask the helper for
  the next picture (`Request::NextFrame`). It is a constant, read by
  `Gate`.
- **Size**: the helper decides when the capture starts.
  `Request::StartCamera` encodes 640×480 at most; `Request::StartShare`
  1920×1080 on the GPU, 1280×720 in software. There is no request to change
  it afterwards.

So far the limits seen have been above what we send. In the recording,
the share was asked for at most 1920×1088 at 15 a second, and we send
1728×1080 at 15. Under load the meeting may ask for less (a smaller tile
at the far end, a constrained link). Today that only lowers the bitrate,
so the picture gets blurrier rather than smaller, and its frame rate
stays the same.

## Suggested shape

1. **Frame rate (no helper change).** Give `SendControl` a ceiling:
   `set_max_fps(fps: Option<u32>)`, kept in an atomic like the bitrate. In
   the sending thread, have `Gate` take the lower of its `Pace::fps` and
   that ceiling when it works out the next picture's time (`frame` in
   `Gate::wait`). A still screen's keepalive should stay as it is.
2. **Size (helper change).** Add `Request::SetMaxSize { id, width, height }`
   to `crates/video-ipc`, answered `Reply::Done`. In the helper, scale
   captured pictures to fit within it before encoding, keeping their
   shape, from the next keyframe on. A change of size needs a new SPS, so
   force a keyframe. Give `SendControl` `set_max_size(Option<(u32, u32)>)`,
   and have the sending thread send `SetMaxSize` when it changes, as it
   sends `SetBitrate`. Raise `crates/video-ipc`'s `VERSION`, as each
   addition to the protocol has (app and helper must match).
3. **Teams wiring.** In `call.rs`'s `ControlVideoStreaming` arm, read
   `max-fps` (divided by 100) and `max-fs`. Turn `max-fs` into a box,
   16:9, `width = sqrt(fs × 256 × 16 / 9)` rounded down to a multiple of
   16, or the largest standard size within it (the table above). Extend
   `media::Command::Limit` (or add a command) and `CallVideo::limit` to
   pass them to `SendControl`. Check `max-mbps` too: if size × rate would
   exceed it, lower the rate.

Slack huddles share this code (`camera_send`, `share_send`, the helper).
With no ceiling set, behaviour must stay exactly as now. Chime describes
its stream once in SUBSCRIBE (`DESCRIPTOR`) and never pushes limits.

## Constraints (from `AGENTS.md`)

- `unsafe` only in the helper crate's platform modules.
- Clippy runs with `-D warnings`, `unwrap_used` included.
- Every public item documented; comments say why.
- Tests offline: unit-test the parsing, the box from `max-fs`, the
  `Gate` timing, the IPC round trip (`crates/video-ipc` has round-trip
  tests for each request).
- Builds on Linux, macOS and Windows.
- Checks: `cargo fmt --all --check`; `cargo clippy --locked --workspace
  --all-targets --all-features -- -D warnings`; `cargo test --locked
  --workspace --all-features`; `RUSTDOCFLAGS="-D warnings" cargo doc
  --locked --workspace --all-features --no-deps`.

## How to see it work

Join a Teams meeting (sidebar's video button → Meetings), share your
screen, and watch the log. "Teams meeting: our screen share to send at
most N kbit/s" shows the bitrate limit arriving now. With the change, a
debug line from `CallVideo::limit` should show the frame rate and size
applied, and the far end's `call/controlVideoStreaming` should settle as
it gets what it asked for.
