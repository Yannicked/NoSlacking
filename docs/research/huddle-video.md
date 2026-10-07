# Huddle video: research (2026-10-07)

Read-only research. NoSlacking at `e7e58a899e4b2548c0e54a577352e4f526aa3109`
(main). amazon-chime-sdk-js at `dea69d268c623ab2006169d3899981fea766fa8a`
(3.31.0, 2026-05-14, Apache-2.0). HuddleFM
sources (AGPL) read only for behaviour, nothing
copied. Line refs are `file:line` at those commits.

## TL;DR

- **Yes, receiving video is realistic**, and the path is the one audio
  already walks: Chime's SFU forwards each sender's video as its own RTP
  stream; we add `recvonly` video m-lines to the offer, list the wanted
  stream ids in SUBSCRIBE's `receive_stream_ids` (one per video m-line, in
  m-line order, 0 for the send/inactive one), re-SUBSCRIBE whenever INDEX
  changes, depacketize with str0m (already in the tree, already offering
  H.264 + VP8), decode, convert YUV→RGBA and draw in egui.
- **Screen shares are almost certainly Chime content share**: Slack's
  limits match Chime's exactly (25 cameras = Chime's 25 videos per
  meeting; "up to two people can share their screen at a time" = Chime's
  two content shares). In the JS SDK a share is a *second attendee*
  `attendeeId#content` joining with `joinToken#content`. For receiving we
  need none of that: a share shows up in our INDEX as one more video source
  whose `attendee_id` ends in `#content`. To be confirmed by a probe.
- **The hard parts are codecs and capture, not Chime.** Decoding needs a C
  library (OpenH264 or libvpx) or platform decoders — there is no mature
  pure-Rust H.264 or full VP8 decoder. Sending needs an encoder (same
  story) plus camera/screen capture, which on Wayland/Flatpak means the
  PipeWire portals. *(Stage 1 update: the spike found rusty_h264, pure
  Rust, bit-exact and fast enough for 1080p shares; it is what Stage 1
  uses. Real Slack sends H.264 CB, so VP8 is not needed.)*
- **Plan**: probe (days) → receive screen shares (2–3 wk) → camera tiles
  (2–3 wk) → send camera (3–5 wk) → share screen (3–5 wk, Wayland the
  risk). Drawing, stickers and effects: not realistic (undocumented,
  probably data messages); reactions possibly later.

## 1. What "video" means in a Slack huddle

From Slack's help article ("Use huddles in Slack",
https://slack.com/help/articles/4402059015315) and the AWS docs
(https://docs.aws.amazon.com/chime-sdk/latest/dg/meetings-sdk.html):

| Feature | What it is | Transport (known / likely) | Realistic for NoSlacking? |
|---|---|---|---|
| Camera tiles | Up to 25 people with video on (paid plans, 50 people) | Chime video, SFU: one stream per sender; Chime's own limit is "25 simultaneous videos per meeting", JS SDK up to 1280x720@30 (15 fps with simulcast) | **Yes** to watch; yes to send, with more work |
| Screen sharing | "Up to two people in a huddle can share their screen at a time" | Chime **content share**, almost certainly: Chime allows two content shares per meeting; content share is 1280x720@15 fps by default; the JS SDK joins a second attendee `attendeeId#content` (`DefaultContentShareController.ts:38-42`). Unverified for Slack: probe the INDEX for `#content` sources | **Yes** to watch (the best first stage: one stream, still pictures, high value); sharing your own screen is the hardest stage |
| Drawing on a share | Desktop (Mac/Windows) only; "Draw Together"; paid plans | Unknown. Chime's docs suggest data messages (≤ 2 KB, "whiteboarding, … emoji reactions") for this kind of thing; Slack may also use its own websocket. Overlays drawn by the sharing client (not in the video) | **Not now.** Undocumented protocol; at best show others' strokes after a probe decodes DATA_MESSAGE topics |
| Reactions, effects, GIFs, stickers | Shown to everyone for a while (stickers stay) | Unknown: DATA_MESSAGE or Slack's RTM/flannel. Effects/backgrounds are rendered into the sender's video by their client | Receiving reactions maybe (after the probe); effects/backgrounds no |
| Background blur/replace | Sender-side processing | Our encoder side; needs segmentation (ML) | No |
| Huddle thread & canvas | A normal message thread (and a canvas) attached to the huddle | Slack Web API / RTM, not Chime | **Yes, cheaply**, independent of video: it is a thread NoSlacking can already render, once the huddle message's `ts` is known (HuddleFM reads `conversations.replies` to find the huddle message, `src_slack-huddle.ts:538-544`) |
| Captions | Slack's transcription | Unknown (likely Chime's transcription events or Slack's) | Out of scope |

## 2. The Chime protocol for video

All proto refs: `protocol/SignalingProtocol.proto` in the JS SDK (vendored
in NoSlacking under `src/huddle_audio/chime/`).

### 2.1 What the server tells us: INDEX

- `SdkIndexFrame` (`proto:147-153`): `sources` (repeated
  `SdkStreamDescriptor`), `paused_at_source_ids`, `num_participants`,
  `supported_receive_codec_intersection`.
- `SdkStreamDescriptor` (`proto:165-177`): `stream_id`, `group_id`,
  `framerate`, `max_bitrate_kbps`, `avg_bitrate_bps`, `attendee_id`,
  `external_user_id`, `media_type` (`AUDIO=1`, `VIDEO=2`, `proto:122-125`),
  `width`, `height`, `track_label`.
- A *group* is one sender's video; several streams in a group are its
  simulcast layers. The JS SDK's `AllHighestVideoBandwidthPolicy` (the
  default when neither simulcast nor SVC is on,
  `DefaultAudioVideoController.ts:643-648`) picks the stream with the
  highest `max_bitrate_kbps` per group, excluding self and non-video
  (`DefaultVideoStreamIndex.ts:263-283`), and stops adding once the sum
  passes 15,000 kbps (`AllHighestVideoBandwidthPolicy.ts:64-100`).
- The JS SDK drops its own `#content` source from INDEX
  (`ReceiveVideoStreamIndexTask.ts:95-103`) — this is where a screen share
  shows up: a source with `attendee_id` = `<attendee>#content`. Slack's
  external user id on it should still name the `U…` (to verify).
- NoSlacking today: `signaling.rs:344-347` uses INDEX only to leave the
  `Indexing` phase; `media.rs:1019` reads `num_participants`. Sources are
  only counted in `chime.rs:281-283` (`describe`). Nothing stores them.
- PAUSE / RESUME (`proto:155-158`, frame types 7/8) say a sender paused;
  BITRATES (type 13, `proto:160-163`) gives per-stream average bitrates
  every ~4 s; REMOTE_VIDEO_UPDATE (type 24, `proto:453-456`) changes
  subscriptions *without* SDP renegotiation, only with server-side network
  adaptation (`VideoPriorityBasedPolicy`). Not needed for a first version.

### 2.2 Choosing what to receive: SUBSCRIBE

- `SdkSubscribeFrame` (`proto:127-137`): `duplex` (RX=1/TX=2/DUPLEX=3,
  `proto:116-120`), `send_streams`, `receive_stream_ids`, `sdp_offer`,
  `audio_host`, `audio_muted`, `compressed_sdp_offer`,
  `video_subscription_configuration` (mid → attendee/stream/group,
  priority, target bitrate; `proto:464-472`).
- The JS SDK's transceiver model (`DefaultTransceiverController.ts:277-327`):
  "Subscription index 0 is reserved for transmitting camera. We mark
  inactive slots with 0 in the subscription array." For each wanted stream
  it adds a `recvonly` video transceiver; on later changes it reuses
  inactive ones (sets `recvonly` again) rather than adding
  (`:396-455`). So `receive_stream_ids = [0, s1, s2, …]` where position *i*
  is the *i*-th video m-line in the offer, and must line up exactly
  ("Our backends currently expect the video subscriptions passed in
  subscribe to precisely line up with the media sections",
  `SubscribeAndReceiveSubscribeAckTask.ts:166-198`).
- Every INDEX change may mean a new offer + SUBSCRIBE + answer
  (`ReceiveVideoStreamIndexTask.ts:112, 152-161`: resubscribe when the
  downlink policy wants it). The connection (ICE/DTLS) stays; only the SDP
  changes.
- SUBSCRIBE_ACK (`proto:139-145`) carries `tracks` (`SdkTrackMapping`:
  `stream_id`, `ssrc`, `track_label`), which the JS SDK turns into
  SSRC → stream and SSRC → group maps (`DefaultVideoStreamIndex.ts:470-485`).
  That is how we would know whose video an SSRC is, independent of mids.
- **What NoSlacking must change**: `chime::subscribe` (`chime.rs:185-209`)
  hard-codes `receive_stream_ids: vec![0]` and `duplex: Rx`;
  `Handshake::subscribe` only subscribes in the `Offering` phase
  (`signaling.rs:230-240`) and INDEX is only handled while `Indexing`
  (`:344`). Video needs a *Live → Resubscribing → Live* cycle: str0m
  renegotiation (`sdp_api()` again, add or re-enable `recvonly` m-lines),
  SUBSCRIBE with the new list, `accept_answer` of the new answer. Audio
  must keep flowing during it (it does in browsers; to verify with
  str0m — it keeps the transport, so it should).

### 2.3 The SDP and codecs

- Our offer today (`media.rs:307-326`): audio `sendrecv`, one video
  `inactive`; str0m is built with `enable_vp8(true)` and `enable_h264(true)`
  (`media.rs:280-293`).
- Chime's real answer (NoSlacking log, 2026-10-07T07:36:44Z, video
  m-line): `108 H264` + `109 rtx`, `96 VP8` + `97 rtx`, `127 H264` + `121
  rtx`, `123 H264` + `119 rtx`, `114 H264` + `115 rtx`, one `a=ssrc`. The
  four H.264 entries mirror the profiles str0m offers (the JS SDK's list:
  `42001f` baseline, `42e01f` constrained baseline, `4d001f` main,
  `64001f`/`640c1f` high; `VideoCodecCapability.ts:108-184`), so Chime
  answers whatever we offer; it does not transcode. NoSlacking already
  works around Chime repeating `rtpmap:109` (`sdp.rs:133-165`).
- **What senders actually send** is decided by the sender, from its own
  preferences intersected with INDEX's
  `supported_receive_codec_intersection` (the codecs *every* receiver
  supports; `ReceiveVideoStreamIndexTask.ts:115-116, 223-250`). Defaults
  (`DefaultAudioVideoController.ts:212-229`): cameras prefer **VP9 profile
  0, then H.264 constrained baseline, then VP8**; content share on Chromium
  (Slack's desktop app is Electron) prefers **AV1, VP9, H.264 CB, VP8**.
  So as long as we only advertise H.264 + VP8, Chime should tell the
  others to fall back to H.264 CB or VP8 — at the cost of everyone else's
  quality/efficiency, and only if Slack keeps the SDK's intersection
  logic. **Must verify**: what codec a Slack desktop share arrives in once
  we are in the meeting (the probe logs payload types per SSRC).
  If senders ignore the intersection and send VP9/AV1, we need VP9/AV1
  decoders too (dav1d/libvpx); that is the biggest protocol risk.
- Must decode both H.264 (constrained baseline is enough; main/high only
  if offered) and VP8: a receiver cannot choose per sender.
- RTX (RFC 4588) and NACK: Chime answers rtx for every codec; str0m
  handles NACK/RTX on receive (see §3). Keyframes: on loss or join the
  receiver sends PLI (RTCP PSFB fmt 1) or FIR (fmt 4); HuddleFM's sender
  counts both (`src_native-media_rtp.ts:50-64`) and re-encodes a keyframe.
  JOIN has `disable_periodic_keyframe_request_on_content_sender`
  (`proto:102`), which tells us Chime itself asks content senders for
  periodic keyframes — handy for a late joiner.

### 2.4 Sending video

From HuddleFM's working sender (behaviour only, AGPL):
- SUBSCRIBE `duplex: 3` (DUPLEX) while sending video, else `1`
  (`chime-link.ts:444`); `send_streams` gets a second descriptor:
  `media_type: 2`, `track_label: "AmazonChimeExpressVideo"`, `stream_id:
  2`, `group_id: 2`, `framerate`, `max_bitrate_kbps`, `attendee_id`,
  `width`, `height` (`:456-469`); `receive_stream_ids: [0]` (`:472`).
  The JS SDK does the same from `videoStreamIndex.localStreamDescriptions()`
  (`SubscribeAndReceiveSubscribeAckTask.ts:72-104`;
  `DefaultSignalingClient.ts:179-187`).
- The video m-line flips between `sendrecv` and `inactive` on start/stop,
  renegotiated over SUBSCRIBE (`chime-link.ts:224-260, 395-402`).
- Single layer, H.264 `packetization-mode=1;profile-level-id=42e01f`
  (constrained baseline 3.1), PT read back from the answer (`:36-37,
  517-519`). FU-A fragmentation (`src_native-media_rtp.ts:122-140`).
- Chime may refuse a video slot: SUBSCRIBE_ACK `duplex: RX` with error 206
  "VideoCallSwitchToViewOnly", audio carries on (`chime-link.ts:497-506`).
  JOIN `flags` must be HAS_STREAM_UPDATE (2) and not 0, or the server
  reports capacity and refuses video (`src_native-media_signaling.ts:229-233`);
  NoSlacking already sends 2 (`chime.rs:147`).
- Simulcast is optional (deprecated in favour of SVC in the JS SDK,
  `MeetingSessionConfiguration.ts:78-82`). The JS SDK's single-layer
  policy, `NScaleVideoUplinkBandwidthPolicy`, targets 1,500 kbps ideal
  (`:60`) and scales height by how many people publish (720p for few,
  540/480p for more, `:23-32, 252-266`). A single 720p (or 540p) H.264 CB
  layer at 15–30 fps is what we would do.

### 2.5 Screen share specifics

- JS SDK: `DefaultContentShareController` builds a second
  `MeetingSessionConfiguration` with `attendeeId + "#content"` and
  `joinToken + "#content"` (`DefaultContentShareController.ts:30-45`,
  `ContentShareConstants.ts`), its own downlink policy
  `NoVideoDownlinkBandwidthPolicy` (receives nothing), and runs a *whole
  second session*: its own signaling WebSocket, JOIN, SUBSCRIBE, peer
  connection, TURN. 15 fps by default (`ContentShareMediaStreamBroker.ts:11`).
- **Receiving** a share needs nothing of this: it is one more source in
  our INDEX. It deserves a large tile and higher priority.
- **Sending** a share: a second `huddle_audio::media` session (no audio
  receive, video send only) with the `#content` credentials derived from
  the ones `rooms.join` already gives. Whether Slack's join token takes
  the `#content` suffix is the JS SDK's design, so likely, but unverified.
  Slack's UI then probably needs a Slack-side signal too (e.g. that the
  share started, for the drawing toolbar) — unknown.

## 3. Rust building blocks

Versions and dates were taken from the crates.io API on 2026-10-07. Crate
sources were read locally, and str0m was read
from `~/.cargo/registry/src/*/str0m-0.24.1/`.

### 3.1 RTP and WebRTC: str0m 0.24.1 (already used, MIT/Apache)
- **Whole frames, already depacketized.** `Event::MediaData` carries one
  full frame (`src/media/event.rs:217-285`): `data`, `contiguous` (false
  means "request a keyframe"), `pt`, `rid`, `time` (90 kHz) and
  `is_keyframe()`. Depacketizers live in `src/packet/`: `h264.rs` handles
  STAP-A and FU-A and outputs Annex B (`:281`); `vp8.rs` (`:608`), `vp9.rs`
  and `av1.rs` are there too. NoSlacking's `rtc_event` ignores any frame
  whose mid is not audio (`media.rs:780-783`). Video frames would arrive
  as they are, so no depacketizer needs writing.
- **Keyframes.** As a receiver we call
  `Writer::request_keyframe(rid, KeyframeRequestKind::Pli | Fir)`
  (`src/media/writer.rs:294`). As a sender we get
  `Event::KeyframeRequest` (`event.rs:309-329`).
- **NACK, RTX, PLI, FIR, TWCC and REMB** are on by default for video
  codecs (`codec_config.rs:129-133`). Received loss triggers NACKs
  (`receive.rs:792-798`), and sent packets are kept in an RTX cache.
  Send-side bandwidth estimation (GCC over TWCC, with a pacer) is enabled
  with `enable_bwe` (`config.rs:468`).
- **H.264 payload types.** `enable_h264` (`codec_config.rs:366-384`)
  offers 127/121 `42001f`, 108/109 `42e01f`, 123/119 `4d001f` and 114/115
  `64001f` with packetization-mode 1, plus mode-0 variants. These are
  exactly the 108/127/123/114 that Chime answered. Profile matching
  follows RFC 6184 (`payload_params.rs:427-460`).
- **Sending.** `rtc.writer(mid).write(pt, wallclock, rtp_time, frame)`
  packetizes for us (`writer.rs:165`). Simulcast works through `rid` and
  `SimulcastLayer` (`event.rs:112-180`), though we don't need it.
- **Several recvonly m-lines.** Each `add_media` call adds a mid.
  NoSlacking's `sdp::Mids` (`sdp.rs:67-104`) maps the mids by position, so
  it already generalises to N m-lines as long as their order stays fixed,
  which renegotiation keeps. Chime's answer carries an `a=ssrc` line per
  m-line (the 07:36 log shows one even on the inactive video line), and
  str0m uses those lines to map SSRCs to mids. SUBSCRIBE_ACK `tracks` can
  cross-check the mapping.
- **Unknown: renegotiation against Chime.** Chime probably expects the
  browser habit of reusing inactive transceivers rather than adding new
  ones forever; str0m can't remove an m-line, only set it inactive. The
  pretend-Chime loopback test can cover this offline.

### 3.2 Decoding

*(Stage 1 chose `rusty_h264-decoder` after a spike; the comparison is
under Stage 1 in §5.)*

| Option | Licence | C/unsafe | Build cost | Notes |
|---|---|---|---|---|
| **`openh264` 0.9.8** (2026-08, ralfbiedert) | BSD-2 crate, BSD Cisco source | C++ compiled by `cc` (`source` feature); optional nasm, "up to 3x" faster | C++ toolchain on all 3 OSes, which CI already has for OpenSSL. nasm is optional (`OPENH264_NO_ASM`) | Decodes 1080p in 2.8 ms with nasm and 5.7 ms without (Ryzen 7950X3D), so roughly **1.5–3 ms per 720p frame on desktops and 3–6 ms on old laptops**. Officially supports Constrained Baseline; the crate's CABAC bench suggests Main works as well. **Patents:** Cisco's MPEG LA cover applies only to Cisco's binary downloaded at install time. Built from source, there is no cover. The `libloading` feature loads a Cisco blob we supply, with its hash checked. The Flatpak runtime dropped the `org.freedesktop.Platform.openh264` extension from 25.08 on, so the blob has no easy source there |
| `rusty_h264-decoder` 0.16 (June 2026) / `rust_h264` 0.4 | BSD-2 / MIT-Apache | Pure Rust, core `forbid(unsafe)` | Just cargo | Promising but months old with single-digit stars. rusty_h264 claims bit-exact output on 35 openh264 conformance streams and about 5.8 ms per 720p frame. **Worth a spike** as the no-C option, with openh264 as the fallback (the same approach as `opus-decoder` vs libopus) |
| Platform decoders: VideoToolbox (`objc2-video-toolbox`), Media Foundation (`windows`), VA-API (`cros-libva`/`cros-codecs`) | various | Raw APIs need `unsafe` in our crate, which is forbidden | One backend per OS | Skip for now. They would need a separate crate that is allowed `unsafe` |
| `ffmpeg-next` 9.0 | WTFPL wrapper, LGPL/GPL FFmpeg | system or built C | Heavy, painful on Windows and macOS | TODO already rules it out for media playback. On Linux and Flatpak, FFmpeg's H.264 sits in the `codecs-extra` extension |
| `gstreamer` 0.25 | LGPL | system libs | Needs the GStreamer runtime on macOS/Windows | Same objections as in the TODO's media item |
| **VP8: libvpx** via `env-libvpx-sys` 5.1.3 (MPL-2.0, links system libvpx) / `vpx-rs` 0.2.1 (needs libclang) / `shiguredo_libvpx` | BSD-3 plus Google's patent grant | C | System libvpx on Linux (the freedesktop runtime most likely ships it; unverified). Vendored on macOS/Windows: configure, make, yasm, and MSYS2 on Windows | **There is no credible pure-Rust inter-frame VP8 decoder.** `image-webp` decodes keyframes only (`vp8.rs:904`, inter frames return `UnsupportedFeature`), and `oxideav-vp8` is a scaffold |

**Avoiding VP8 entirely** (recommended to try first): if our offer lists
only H.264, the `supported_receive_codec_intersection` drops VP8, VP9 and
AV1, and every sender should send H.264 CB, which the JS SDK falls back
to. The probe has to confirm this. The cost is that everyone else in the
huddle loses VP9/AV1 while we are in it, and possibly simulcast too,
because the SDK only simulcasts some codecs. It is also worth checking
whether our current offer (VP8 + H.264, video inactive) already narrows
others' codecs today: log INDEX's `supported_receive_codec_intersection`.

### 3.3 Encoding (sending)
- **H.264 CB with `openh264`'s encoder.** It takes about 8 ms per 1080p
  frame with nasm (24 ms without), so a 720p camera at 15–30 fps fits on
  one thread. The patent note above applies. This is what HuddleFM sends
  (`42e01f`, packetization-mode 1).
- `rusty_h264-encoder` (pure Rust) claims about 13 ms per 720p inter
  frame. It is young.
- libvpx VP8 needs the same C build as decoding.
- x264 is GPL, so no.
- rav1e (AV1) is real-time only at fast presets, and Chime would only
  take AV1 if every receiver could decode it. Not useful here.

### 3.4 Camera capture
- **`nokhwa` 0.10.11** (2026-05, Apache-2.0) covers V4L2, AVFoundation and
  MSMF. It has no PipeWire or portal backend, ships releases slowly, and
  has 67 open issues. It works for native Linux (V4L2) and for
  macOS/Windows, but not inside the Flatpak without `--device=all`.
- **The Flatpak path:** `ashpd` 0.13 `desktop::camera` gives a
  `request_access()` and a PipeWire fd, which the `pipewire` crate 0.10
  reads. That needs system libpipewire plus libclang for bindgen at build
  time.
- macOS needs `NSCameraUsageDescription` in Info.plist, next to the
  microphone key we already have.

### 3.5 Screen capture
- **`xcap` 0.9.8** (2026-08, Apache-2.0, active) has a `VideoRecorder`.
  On Wayland it goes through the ScreenCast portal over zbus plus pipewire
  0.10, with a restore token; on X11 it uses xcb; on macOS objc2; on
  Windows the `windows` crate. It is the best single crate.
- `scap` 0.0.8 has gone quiet and uses old dependencies.
- Native alternatives: `screencapturekit` 11 (needs swiftc and macOS 13+)
  and `windows-capture` 2.0.1 (active).
- The ScreenCast portal works inside the Flatpak sandbox with no extra
  permission. The user picks a window or screen in the portal dialog.

### 3.6 Drawing frames in egui (glow)
- eframe runs on **glow** (`Cargo.toml:34`). Uploading YUV planes and
  converting them in a shader would mean glow's `HasContext` calls, which
  are all `unsafe fn` and forbidden in our crate. Under the current rules
  the path is **CPU I420→RGBA, then `TextureHandle::set`**.
- **`yuv` 0.8.19** (BSD-3/Apache, pure Rust with runtime SIMD dispatch;
  the unsafe is inside the crate) converts 2.66 Mpx in about 0.4–0.9 ms,
  so **about 0.15–0.35 ms per 720p frame**. openh264's own `write_rgba8`
  is slower, about 1.4 ms per 1080p frame.
- Upload: a 720p RGBA frame is 3.7 MB. egui_glow re-specifies the whole
  texture (`tex_image_2d`, `egui_glow/src/painter.rs:510-628`) on every
  set. At 15–30 fps for one share plus a few small tiles this is fine.
  25 tiles at 720p is not, so downscale before upload to the tile's
  on-screen size (from the `yuv` crate or a cheap box filter) and
  subscribe to fewer or lower layers.
- **Budget for one 720p30 stream:** about 2–3 ms decode + 0.3 ms convert
  + about 1 ms upload, roughly 10 % of one core. A 720p15 share costs
  half that. Six camera tiles at 360p are about the same as one 720p
  stream. Decoding has to run off the UI thread.

## 4. Fit and architecture

- **Feature flag:** `huddle-video = ["dep:openh264", "dep:yuv"]`
  (built as `rusty_h264-decoder`, `yuv` and `bytemuck` instead of
  openh264), plus `huddle-video-send` later for the encoder and capture.
  It stays off in releases until proven, as huddle audio did until it
  became part of every build.
- **Receive pipeline:**
  - `huddle_audio/` would gain `video_index.rs`: a pure module for INDEX
    sources by group, the `#content` flag, the choice of which streams to
    receive (shares first, then the people speaking or most recently
    speaking, at most N), and the mapping from m-line slots to stream ids.
    It is testable offline like `roster.rs`.
  - `signaling.rs` gains a Live → Resubscribing → Live phase and stores
    INDEX sources.
  - `chime::subscribe` takes `receive_stream_ids` and `duplex`.
  - `media.rs` keeps a slot table (mid → stream, attendee) and sends
    `MediaData` for video mids into a `VideoSink`. It asks for a keyframe
    (PLI) when `!contiguous` or when a slot is first subscribed, rate
    limited.
- **Decode thread:** a `video.rs` decoder thread, one per huddle, owns one
  decoder per stream. It decodes, converts to RGBA at display size, and
  keeps only the newest frame per tile in a shared slot (a
  `Mutex<Option<Frame>>` per tile). Old frames are dropped, never queued,
  and the `Waker` is called at most once per frame. The tokio task must
  never block on decoding.
- **UI:** `app.rs` turns the slot into a `TextureHandle` per tile with
  `set` each frame. Views read model types such as
  `model::VideoTile { user, is_share, texture }`; no Slack or Chime type
  crosses over. Layout options:
  - In the call bar: a small "N cameras · M sharing" line plus a button.
  - **A call window** using the existing pop-out machinery
    (`src/app/popout.rs:72`, `show_viewport_immediate`): a grid of tiles,
    the share large with tiles beside it, names on tiles, the speaking
    ring from `roster`. This is the natural home, since the sidebar is too
    small.
  - The share's tile should allow fit, 1:1 and a pop-out to its own
    window.
- **Sending:** capture thread → encoder thread → `Uplink`-like queue →
  `writer.write` on the video mid. The send m-line is always the first
  video m-line, slot 0, flipping between `sendrecv` and `inactive` with a
  re-SUBSCRIBE using `duplex` DUPLEX and the send descriptor (§2.4).
  `KeyframeRequest` forces an IDR. A view-only refusal (206) shows a
  toast.
- **Screen share sending:** a second session object, the content
  attendee, running the same `media::listen` machinery minus audio
  decode. `media.rs`'s session loop would need to be shareable or
  generic.
- **Packaging:**
  - Linux packages: openh264 compiles in, with no system dependency.
  - Flatpak: the Camera portal and ScreenCast portal need no
    finish-args. Raw V4L2 (nokhwa) would need `--device=all`.
  - macOS: Info.plist needs `NSCameraUsageDescription`; Screen Recording
    permission is granted at runtime.
  - Windows: privacy settings, as with the microphone.
- **Binary size** (guesses): openh264 adds about 1–2 MB, `yuv` under
  1 MB. libvpx would add about 1–2 MB static. Capture crates
  (xcap/pipewire/ashpd) add another 1–3 MB. Measure as for audio.
- **CI:** openh264's `cc` C++ build and optional nasm on all three OSes.
  `pipewire` needs `libpipewire-0.3-dev` plus clang on Linux runners for
  the send stages.
- **Rules:** none of the above needs `unsafe` in our crate (the C sits
  behind safe wrappers, as with OpenSSL now). Tests stay offline:
  INDEX → subscription decisions, the slot table, re-SUBSCRIBE frames, a
  committed H.264 fixture of a few frames decoded to a known size, and
  the pretend-Chime loopback test extended with a video m-line carrying
  an encoded test pattern.

## 5. Recommendation: a staged plan

Effort is in focused weeks, on the evidence of the audio work.

**Stage 0: probe, no decoding (2–4 days).** Extend `--huddle-probe`:
- Log every INDEX source (stream, group, attendee or `#content`, the
  external id's `U…`, media type, width×height, fps, kbps) and
  `supported_receive_codec_intersection`.
- Log PAUSE/RESUME, BITRATES, and DATA_MESSAGE topics and sizes (no
  payloads).
- With `--video N`, subscribe to up to N video streams (recvonly m-lines
  and `receive_stream_ids`). Log per mid: the payload type and codec, the
  frame count, keyframes, the first keyframe's SPS profile and level
  (parsed with `h264-reader` or by hand), the resolution from the SPS,
  and the `contiguous` gaps. Send a PLI at start.
- Optionally dump the first 300 frames of a stream to an Annex B `.h264`
  file in the state folder, to replay offline and use as test fixtures
  (strip identifying content first).

The user verifies against real Slack, in a huddle with a Slack desktop
user who turns on their camera and then shares a screen:
- Does the share appear as a `#content` source?
- Which codec arrives, H.264 CB or VP8? Does it change when the probe
  offers H.264 only?
- Does Chime accept extra recvonly m-lines and a re-SUBSCRIBE mid-call
  without disturbing audio?
- Do keyframes come after a PLI?
- What do the DATA_MESSAGE topics look like while someone draws or
  reacts?

**Stage 0: built (2026-10-07); what to run.** In every build, in
`src/huddle_audio/video.rs` (INDEX, the choice of streams, the slot
table, SSRC → stream), `watch.rs` (renegotiation and counting in the
session), `bitstream.rs` (a small SPS reader, the VP8 keyframe header,
IVF) and `probe.rs`. No new crate; the SPS is read by hand.
- Every probe run now logs, at info level and only when it changes (at
  most every 2 s), each INDEX: its sources (stream, group, attendee
  shortened and whether it is `#content`, the `U…`, media type, size,
  fps, max/avg kbps, track label), paused ids, head count and
  `supported_receive_codec_intersection`. Also PAUSE/RESUME, BITRATES
  (every 20 s), DATA_MESSAGE topics, sizes and senders (never the
  payload), REMOTE_VIDEO_UPDATE, and SUBSCRIBE_ACK's tracks and
  allocations.
- `--video N`: 2 s after DTLS is up, picks up to N streams (shares first,
  then the highest-bitrate stream of each group, never ours), adds
  `recvonly` m-lines with `str0m`, re-SUBSCRIBEs with
  `receive_stream_ids = [0, s1, …]` and takes the new answer; again when
  INDEX changes the choice (at most every 3 s; a re-SUBSCRIBE without an
  answer in 10 s is given up). A stream that goes frees its m-line
  (`inactive`), which a later stream reuses. Per stream it logs the
  payload type and codec (with `profile-level-id`), the SSRC and whose it
  is by SUBSCRIBE_ACK's tracks, the first keyframe's SPS (profile,
  level, size) or VP8 header, frames, keyframes, gaps and PLIs sent (one
  when a slot starts, one per gap, at most one a second), every 5 s.
- `--video-h264-only` builds the peer without VP8, so every video m-line
  offers only `str0m`'s H.264 profiles.
- `--video-dump DIR` writes the first 300 frames of each stream to
  `DIR/<stream>-<attendee>.h264` (Annex B) or `.ivf` (VP8). These hold
  people's faces and screens.
- The summary gives each stream (codec, frames, keyframes, size, gaps,
  share or camera), each re-SUBSCRIBE with the audio frames counted from
  it until 2 s after its answer, and whether audio stayed live through
  all of them.
- Offline: the loopback test plays a Chime that announces a share and our
  own camera, answers the re-SUBSCRIBE with a track mapping and sends a
  320×180 H.264 test pattern (`fixtures/test-pattern-320x180.h264`, made
  with ffmpeg's `testsrc` and OpenH264); the probe receives only the
  share, asks for a keyframe, reads the SPS, and audio goes on.

Runs, in a huddle where a colleague on Slack's desktop app turns the
camera on after about 20 s and starts sharing a screen after about 50 s
(and, for the last question, reacts or draws during the share):

```
cargo run --release -- --huddle-probe TEAM CHANNEL --seconds 90
cargo run --release -- --huddle-probe TEAM CHANNEL --seconds 90 --video 4
cargo run --release -- --huddle-probe TEAM CHANNEL --seconds 90 --video 4 --video-h264-only
cargo run --release -- --huddle-probe TEAM CHANNEL --seconds 90 --video 4 --video-dump probe-dumps
```

Not yet known, and what the logs will settle: whether Chime takes a
re-SUBSCRIBE that adds m-lines (the JS SDK only ever adds or reuses
them, as this does); whether its answer's `a=ssrc` per m-line matches
SUBSCRIBE_ACK's tracks; whether `str0m` follows a slot whose SSRC
changes; and whether the first keyframe arrives whole (the SPS is looked
for until found, and keyframes without one are counted).

**Stage 1: watch screen shares (2–3 weeks).**
- Work: INDEX tracking, re-SUBSCRIBE, H.264 decode (openh264 from source,
  or a rusty_h264 spike first), the `yuv` conversion, and a call window
  with the share.
- Verify: the share is readable at full size and arrives within about
  2 s of joining or starting; CPU is reasonable; audio is unaffected by
  renegotiation; things recover after packet loss (PLI).

**Stage 0: what real Slack showed (2026-10-07).** A share is its own
INDEX source, attendee `…#content`, external user id carrying the
sharer's `U…`. It arrives as H.264 constrained baseline
(`profile-level-id=42e01f`, packetization-mode 1, pt 108), 1920×1080,
level 4.2, about 12 fps and 200–900 kbit/s, with a keyframe on PLI.
Cameras: H.264 CB 480×480 at about 22 fps. Once we receive video the
codec intersection drops VP9 and AV1 and senders pick H.264 CB (VP8 stays
listed, unused). Re-SUBSCRIBE with `recvonly` m-lines works, audio keeps
flowing, slots are freed and reused.

**Stage 1: built (2026-10-07), behind `huddle-video`; not yet tried
against Slack.**

*Decoder: pure Rust, `rusty_h264-decoder` 0.16.* Spike in a scratch
crate, release build, on an AMD Ryzen AI 7 350, single thread. Fixtures
made with ffmpeg and libopenh264, constrained baseline: a 1080p "screen"
(the app's own screenshot with a moving cursor and a frame counter,
36 frames at 12 fps, 500 kbit/s, IDR every 24; level 4.0, as OpenH264
writes it), a 480×480 "camera" (testsrc2 with a moving Mandelbrot, 66
frames at 22 fps, 400 kbit/s) and, for timing only, a 1080p whole-screen
scroll at 2 Mbit/s (the worst case for a share). Compared with ffmpeg's
own decode of the same files.

| | `rusty_h264-decoder` 0.16 (`std`+`asm`) | the same, no `asm` | `rust_h264` 0.4 |
|---|---|---|---|
| Correct | bit-exact, every frame of all three | bit-exact | bit-exact |
| 1080p screen, ms/frame (mean / p95 / max) | **3.4 / 10.1 / 16.9** | 5.7 / 17.9 / 34.5 | 9.9 / 25.7 / 42.7 |
| 1080p scrolling, 2 Mbit/s | **4.9 / 7.6 / 17.3** | 6.9 / 12.7 / 35.5 | 13.8 / 21.8 / 52.9 |
| 480×480 camera | **0.43** / 0.64 / 1.8 | 0.78 | 1.8 |
| Broken input (truncated, bit flips, garbage tails, random AUs; release and overflow-checked debug; about 30,000 frames each) | errors, **no panic, no hang** | | errors, no panic, no hang |
| After an error | refuses every later frame, **even the next IDR**, until a new `Decoder` is made: the wrapper makes one at each keyframe after a loss | | conceals: decodes on with damaged references; a dropped frame is not noticed |
| Latency | the picture of the access unit given | | a picture comes out when the next one starts (`flush` after each unit to avoid it) |
| `unsafe` | decoder and common crate `forbid(unsafe_code)`; `asm` adds rusty_h264-accel, about 245 `unsafe` (SSE2/AVX2/NEON intrinsics, AVX2 and SSE4.1 detected at run time; no assembly, no C, no build script) | none | 6, NEON only; 83 `unwrap`, 21 `panic!`/`unreachable!` in the source |
| Licence | BSD-2-Clause (cargo deny passes) | | MIT OR Apache-2.0 |
| Maintenance | since 2026-06, 16 releases, 0.16.0 2026-09-07, pushed 2026-10-04, 9 stars | | since 2026-02, 0.4.0 2026-04-20, pushed 2026-09-08, 8 stars |
| Gotcha | its default features install `rusty_alloc` as the process's **global allocator**: depend with `default-features = false, features = ["std", "asm"]` | | |

Also looked at: `h264-decoder` (xiu) only parses headers; `rumpeg-h264`
wraps rusty_h264. **Chosen: rusty_h264-decoder** (fastest by 2–3×, fuzzed
by its authors and here, errors instead of guessing). Its weak points: a
young, small project, and the poisoned state after an error, which
`decode::H264` works around. Patents: as with any H.264 built from
source, no licence comes with it (risk 2 below).

*What was built.*
- `huddle_audio::decode`: `H264` (waits for a keyframe at the start,
  after a gap and after an error, then starts a fresh decoder), the
  shrink by whole steps to the window's size (box filter), and I420 →
  RGBA with `yuv` (BT.601 studio range), written straight into egui's
  pixels.
- `huddle_audio::screen`: `Screen`, the newest-picture slot shared with
  the interface (an older picture not yet taken is dropped, never
  queued; the window is woken only when it took the last one), and
  `Decoding`, a thread per session fed by the session (frames queue up
  to 36, then are dropped and a keyframe asked for). It logs its
  timings every 10 s while it works.
- `video::shares`, `share_stream` and `wanted`: who shares (others'
  only, one per sharer, keyed by attendee id), and what to receive:
  the share the call window shows, nothing else (`--video N` still adds
  its picks). `watch` follows the window (`Viewer`: the shares sent out,
  the watched key coming in), re-SUBSCRIBEs as before, starts and stops
  decoding as the slot is answered, and asks for a PLI when a slot
  starts, on a gap, and when the decoder wants a keyframe (at most one a
  second). With `huddle-video` the offer is H.264 only: VP8 was never
  chosen by senders, and it could not be decoded.
- The app: the call bar lists each share ("Ana is sharing their
  screen") with Watch / Stop watching; Watch opens the call window, a
  native window through egui's immediate viewports (as the pop-outs), the
  share fitted and centred on a dark stage, the sharer's name, a tab per
  share when two people share, Close. Closing it, leaving, a failure or
  the share ending (a toast says so) stops receiving it. The picture is
  uploaded with `TextureHandle::set`. Demo: `--demo-view sharing` (the
  call bar) and `--demo-view call-window` (drawn inside the main window
  there, as eframe cannot screenshot a second window; the pretend share
  plays the 1080p fixture at 12 fps).
- Logging: INDEX's heading is logged when its shape changes (sources
  come, go, resize or pause; not bitrates or frame rates), each source,
  BITRATES, DATA_MESSAGE, tracks and stream counts only at debug level
  unless it is the probe or `--video`.

*Measured in the app* (release, the demo's 1080p share at 12 fps, the
call window 1280×780 at scale 2, so pictures are converted at full
size), headless under Xvfb with Mesa's software GL (llvmpipe), so the
machine was busy rendering: the decoder thread used **7 % of one core**
(8–9 ms decoding and 1.7 ms converting a picture, against 3.4 ms
decoding on a quiet machine in the spike); the interface thread 11 %,
with the demo repainting every frame for its screenshot and uploads
going through software GL. A real GPU and a real share (mostly still,
smaller P frames) should cost less; to be measured on a desktop.

*Binary size:* `--features huddle-video` adds 0.99 MB to the release
binary (50.32 → 51.31 MB, both with huddle audio).

*Not yet known:* whether real shares keep decoding cleanly for minutes
(SEI, multiple slices, Chrome's hardware encoders), how long the first
picture takes after Watch (re-SUBSCRIBE at most every 3 s, then a PLI),
whether a share's stream id changes mid-share (the key is the attendee,
so it would be followed), and whether Slack's VUI ever says full range
or BT.709 (the conversion assumes BT.601 studio range).

**Stage 2: camera tiles (2–3 weeks).**
- Work: multiple slots, a selection policy (at most 4–9 tiles, active
  speakers first, layer choice by tile size), PAUSE/RESUME (a paused tile
  shows the avatar), a tile grid, and VP8 only if Stage 0 shows it can't
  be avoided (+1–2 weeks for libvpx builds on macOS/Windows).
- Verify: faces match people, tiles come and go with the camera, and
  behaviour holds with 5 or more cameras on.

**Stage 3: send camera (3–5 weeks).**
- Work: capture (nokhwa natively, the ashpd Camera portal in the
  Flatpak), the openh264 encoder (720p or 540p, 15–30 fps, about
  1–1.5 Mbit/s with BWE), DUPLEX SUBSCRIBE, keyframe requests, a camera
  button and preview, device choice, and macOS's camera key.
- Verify: others see us in Slack's desktop, web and mobile clients; a
  206 view-only case is handled; quality holds under loss; there is no
  green or corrupt frame at start (SPS/PPS sent with every IDR).

**Stage 4: share screen (3–5 weeks, Wayland the main risk).**
- Work: a second `#content` session, xcap's recorder (the portal on
  Wayland, xcb on X11, macOS, Windows), encoding at 15 fps and up to
  1080p or 1280×720, and window or screen picking.
- Verify:
  - Slack accepts `joinToken#content`.
  - The share shows as ours in Slack's UI (it may need a Slack-side call
    as well).
  - It works in the Flatpak on GNOME and KDE.
  - The two-share limit is reported cleanly.

**Not recommended:** drawing on shares, effects, backgrounds, stickers.
Reactions only if Stage 0 shows they are plain data messages.

**Main risks:**
1. Codecs Chime forwards: if Slack's senders ignore the intersection and
   send VP9 or AV1, we would need libvpx's VP9 or dav1d, which are more C.
   Stage 0 settles this.
2. Codec licensing: openh264 built from source carries no patent licence.
   The Cisco binary is the covered option, but it is fetched at install
   time, gone from the Flatpak runtime, and awkward to ship. Pure-Rust
   H.264 is young. Patent exposure is the user's call; many FOSS clients
   ship OpenH264 from source or rely on the platform.
3. Renegotiation against Chime with str0m: the m-line order and reuse
   rules (§2.2), and the SSRC-to-mid mapping when Chime switches layers on
   a slot.
4. CPU at many tiles. Mitigated by subscribing to fewer streams and lower
   layers, by downscaling, and by decoding off the UI thread.
5. Wayland screen capture: portal and PipeWire build dependencies (libclang),
   and differences between compositors.
6. The rules: no GPU YUV shader under glow without `unsafe`, so the CPU
   conversion path stands. Moving eframe to wgpu would allow a safe
   shader later.
7. Slack's terms and undocumented behaviour, as for audio: the
   `#content` join is the JS SDK's design, unproven with Slack.

**Total:** about 1–1.5 months for watching (Stages 0–2) and another
1.5–2.5 months for sending (Stages 3–4), plus hardening. This is in line
with the TODO's "+4–8 weeks" for video and screen viewing.

## Sources
- amazon-chime-sdk-js @ dea69d268c623ab2006169d3899981fea766fa8a (Apache-2.0), files as cited.
- HuddleFM (AGPL, read only): `src_native-media_chime-link.ts`, `src_native-media_rtp.ts`, `src_native-media_signaling.ts`, `src_slack-huddle.ts`.
- NoSlacking @ e7e58a8: `src/huddle_audio/{media,chime,signaling,sdp,roster}.rs`, `TODO.md` §Huddles, `Cargo.toml`, `packaging/flatpak/cloud.yannick.NoSlacking.yml`, and the log line of 2026-10-07T07:36:44Z.
- https://slack.com/help/articles/4402059015315-Use-huddles-in-Slack
- https://docs.aws.amazon.com/chime-sdk/latest/dg/meetings-sdk.html
- https://aws.amazon.com/blogs/business-productivity/customers-like-slack-choose-the-amazon-chime-sdk-for-real-time-communications/
- https://www.engadget.com/slack-huddles-video-screen-sharing-130033179.html
- https://www.openh264.org/faq.html
- https://bbhtt.in/posts/closing-the-chapter-on-openh264/
- crates.io / GitHub for: str0m, openh264, rusty_h264, rust_h264, image-webp, oxideav-vp8, env-libvpx-sys, vpx-rs, shiguredo_libvpx, nokhwa, ashpd, pipewire, xcap, scap, screencapturekit, windows-capture, yuv (awxkee/yuvutils-rs), dcv-color-primitives, rav1e, ffmpeg-next, gstreamer.
