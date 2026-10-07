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
- **All decoding in a helper (§6.9, 2026-10-07):** the app decodes no
  video itself any more; `noslacking-video` does, on the GPU when it can
  and in software (rusty_h264, moved there) otherwise. Without the
  helper there is no video. A decoder panic now ends the helper, which
  restarts, not the app.
- **Hardware decoding (§6, 2026-10-07):** a helper process,
  `noslacking-video`, decodes H.264 with VA-API on Linux, bit-exact,
  with software as the fallback for anything it cannot do or any crash
  (in the app until §6.9, in the helper since).
  Pictures are scaled on the GPU to the size shown before they are
  copied back: for a share shown at half size that is 6 times less CPU
  than software; at full size software still wins. Off by default for
  now. Vulkan Video, V4L2,
  VideoToolbox and Media Foundation are planned behind the same trait.
  Our camera is encoded on the GPU too (§6.8): 1 ms and 0.4 ms of CPU a
  640×480 picture through the helper against 4.3 ms in software.
- **Sharing your screen (Stage 4, 2026-10-07):** built behind
  `huddle-share`: the ScreenCast portal (ashpd) and PipeWire on Wayland,
  x11rb on X11, xcap on macOS/Windows; a second `#content` Chime session
  that only sends; 1080p at 15 fps from the GPU (5 ms, 1.9 ms of CPU a
  picture here), 720p in software (25 ms a 1080p picture is too much).
  Untried against Slack.
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
  frame. It is young. *(Stage 3 chose it after a spike: about 4 ms a
  640×480 picture, bit-exact under ffmpeg; see Stage 3 in §5.)*
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

### 3.5 Screen capture (see Stage 4 in §5 for what was chosen)
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

**Stage 2: built (2026-10-07), behind `huddle-video`; not yet tried
against Slack.**

- *Selection* (`huddle_audio::cameras`, pure, tested offline): nothing is
  received while the call window is closed. Open, it says in a `Wish`
  whether it shows a share, how many tiles it has room for (1–9, from
  its size: as many as fit at least 200 points wide, 4:3) and a tile's
  size in pixels (rounded up to 32). Cameras are others' non-`#content`
  video, one per attendee with all its layers. If everyone fits,
  everyone gets a tile; otherwise those with a tile keep it in its
  place, free places go to whoever spoke last (from AUDIO_METADATA via
  the roster's `Voices`, by attendee id), then INDEX order, and someone
  speaking takes the place of the longest-silent person with a tile
  unless that person also spoke in the last 5 s. Our own camera never.
  Per camera, the smallest layer covering the tile, else the largest.
  A new choice waits until it has stood still 400 ms, and re-SUBSCRIBE
  stays at most every 3 s; slots freed by a camera turning off are
  reused by the next one.
- *PAUSE/RESUME*: paused streams are INDEX's `paused_at_source_ids`,
  then PAUSE/RESUME frames (by stream or group) until the next INDEX. A
  paused camera keeps its tile (showing the avatar or initials, "camera
  paused") and its m-line while nobody else needs them, so it returns at
  once: RESUME (or an INDEX without it) asks for a keyframe at once. Any
  camera that is on takes a paused one's place when there is no room.
- *Decoding* (`huddle_audio::gallery`): one thread for every camera,
  a `decode::H264` each (fresh decoder per keyframe after errors), the
  picture shrunk to the tile before RGBA, the newest per camera kept in
  the `Gallery` (older dropped, the window woken once for any number),
  a keyframe asked for at a tile's start, on gaps, decoder errors and a
  queue over a second per camera. The share keeps its own thread.
- *Measured* (release, AMD Ryzen AI 7 350, the 480×480 fixture, tiles
  320×240, `gallery::tests::cameras_cost`): 0.38 ms a picture decoding
  and converting with 4 cameras, 0.43 ms with 9: **3 % of one core for
  4 cameras and 8 % for 9** at 22 fps. Uploads (a 480×480 texture each)
  are on the interface thread, not measured separately.
- *The app*: the call bar says "N cameras on" with Video (opens the call
  window) / Close video. The call window shows the tiles in a grid
  without a share, or the share large with the tiles in a column beside
  it (wide window) or a row below (tall), each tile filled by its
  picture (cropped), the name on a plate, the muted mark, the speaking
  ring, "+N more cameras" in the bar for those without room. The tiles
  are built from a list (`TileView`), so a self-preview is one more
  entry. A share ending leaves the window on the cameras. Demo: five
  cameras (the fixture, tinted per person, Dev's paused) and the share;
  `--demo-view cameras` (the grid) and `call-window` (share and tiles).
- *Not yet known*: whether Slack's PAUSE/RESUME are sender pauses or
  the server's bandwidth pauses (handled alike), whether senders
  simulcast to us (the layer choice is ready, untried), how the camera
  attendee id lines up with the audio one for the speaking order (it is
  assumed the same, as in the JS SDK), and the cost of 9 texture uploads
  a frame on a slow GPU.

**Stage 3: send camera (3–5 weeks).**
- Work: capture (nokhwa natively, the ashpd Camera portal in the
  Flatpak), the openh264 encoder (720p or 540p, 15–30 fps, about
  1–1.5 Mbit/s with BWE), DUPLEX SUBSCRIBE, keyframe requests, a camera
  button and preview, device choice, and macOS's camera key.
- Verify: others see us in Slack's desktop, web and mobile clients; a
  206 view-only case is handled; quality holds under loss; there is no
  green or corrupt frame at start (SPS/PPS sent with every IDR).

**Stage 3: built (2026-10-07), behind `huddle-camera`; not yet tried
against Slack.**

*Encoder: pure Rust, `rusty_h264-encoder` 0.16* (the decoder's project).
Spike in a scratch crate, release build, AMD Ryzen AI 7 350, one
thread, `EncoderConfig::baseline` (constrained baseline, CAVLC, one
reference, no B-frames, no lookahead: one access unit out per picture
in), 15 fps, a keyframe every 1000 frames plus two forced. Two 150-frame
clips per size made with ffmpeg: a photo panned and zoomed with camera
noise (`noise=alls=6`), and `testsrc2` with noise. Every output was
decoded by ffmpeg and by `rusty_h264-decoder`, **bit-exact with each
other, every frame**; ffprobe reads "Constrained Baseline", level 3.1.

| | Preset | Target | ms/frame mean / p95 / max | Got (kbit/s) | Luma PSNR mean / min |
|---|---|---|---|---|---|
| 640×480 photo | **Fast** | 600 | **4.1** / 5.4 / 10.8 | 642 | 34.5 / 32.7 |
| 640×480 photo | Fast | 1200 | 4.8 / 5.6 / 8.9 | 1202 | 35.3 / 33.6 |
| 640×480 photo | Balanced | 600 | 6.5 / 9.6 / 12.9 | 654 | 35.7 / 31.6 |
| 640×480 testsrc2 | Fast | 600 | 3.3 / 3.5 / 6.3 | 603 | 37.1 / 33.7 |
| 640×480 testsrc2 | Fast | 1200 | 3.7 / 4.1 / 6.5 | 1200 | 41.0 / 39.0 |
| 1280×720 photo | Fast | 600 | 10.4 / 13.4 / 21.5 | 784 | 33.8 / 31.5 |
| 1280×720 photo | Fast | 1200 | 11.0 / 12.3 / 22.0 | 1345 | 35.0 / 33.1 |
| 1280×720 photo | Balanced | 1200 | 16.4 / 25.2 / 32.8 | 1380 | 36.0 / 32.5 |
| 1280×720 testsrc2 | Fast | 1200 | 10.6 / 12.7 / 19.8 | 1245 | 36.0 / 34.1 |

- Rate control holds 600–1,200 kbit/s from the second second on (each
  one within about ±10 % at 640×480); the first second overshoots by
  the first IDR (85 KB at 640×480, 236 KB at 720p). The app starts it at
  QP 30 rather than 26 to soften that.
- `request_keyframe()` makes the next picture an IDR with SPS and PPS in
  front, as does every IDR; it also restarts the keyframe interval.
- Robustness, release and overflow-checked debug: odd, zero and 1×1
  sizes are refused by `Encoder::new` with an error; 2×2, 16×16, 18×10,
  640×360, 642×362, 1920×1080, 4096×2304, 10×6000 and 6000×10 encode
  random noise without a panic; a frame of the wrong size or with short
  planes is an error. `encode()` itself `expect`s, so the app calls
  `encode_planes`, which returns errors.
- No runtime bitrate change: a new bitrate means a new encoder (and a
  keyframe), so the app moves in steps and at most every 8 s.
- Licence BSD-2-Clause; `forbid(unsafe_code)`; the `asm` feature is the
  same `rusty_h264-accel` the decoder already uses (intrinsics, no
  build script, no C). Default features off (they install a global
  allocator). Young: 0.16, June 2026, 16 releases, one maintainer.
- Others looked at: `less-avc` 0.1.5 (pure Rust, but lossless I_PCM
  only: 640×480 at 15 fps is about 55 Mbit/s, unusable),
  `oxideav-h264` 0.1.8 ("no decode/encode functionality yet"),
  `wedeo-codec-h264` (decoder only, LGPL). openh264 was not tried: the
  rule is pure Rust. **Chosen: rusty_h264-encoder, preset Fast, 640×480
  at 15 fps** (720p would fit the 20 ms budget at about 11 ms on this
  machine, but not on older laptops).

*Capture: `nokhwa` 0.10.11* (Apache-2.0, May 2026; 570 k downloads),
`default-features = false, features = ["input-native"]`, so no mozjpeg:
MJPEG goes through `image`'s pure-Rust JPEG decoder. YUYV, NV12, MJPEG,
RGB and grey become I420 (`yuv` for RGB, by hand for the others) and are
shrunk to fit 640×480 by area averaging. Build needs:
- Linux: V4L2 through `v4l` 0.14, whose `v4l2-sys-mit` runs **bindgen
  at build time: libclang** (and the kernel headers) must be there; no C
  is compiled. CI's Linux jobs install `libclang-dev`.
- macOS: AVFoundation through `objc`/`cocoa` 0.20-era crates;
  `objc_exception` **compiles a one-file Objective-C shim** with
  Xcode's clang (as `mac-notification-sys` already does). Untested here.
- Windows: Media Foundation through the `windows` crate, nothing to
  build. Untested here.
- `paste` (a build-time macro of nokhwa) is unmaintained; `deny.toml`
  lists the advisory with a reason. `nokhwa`'s `Camera` drop `unwrap`s
  `stop_stream`, so the capture thread stops the stream itself first.
- Flatpak: V4L2 needs `--device=all`; the Camera portal (ashpd +
  PipeWire, libclang again for `pipewire`) is left for later (TODO).
- Also looked at: `cameras` 0.3.2 (objc2, but pulls retina/tokio
  unconditionally, 5 months old), `oximedia-capture` (weeks old),
  `linuxvideo` (pure Rust V4L2, Linux only).

*What was built.*
- `huddle_audio::camera`: the `Camera` trait and `CameraControl`, the
  microphone's rule as a state machine (opened only when turned on,
  closed when off, on leaving and on failure); `Nokhwa` (a thread per
  open camera, asking for 640×480 at 30 fps, closest raw format; 15 at
  first, raised to 30 once tried in a real huddle);
  `TestPattern` (moving colour bars, a bouncing square and a mm:ss.t
  clock with the frame number); `Latest`, the newest-frame slot between
  capture and encoder; the conversions and the scaler.
- `huddle_audio::video_encoder`: the encoder set up as above (Fast, CB,
  CAVLC, level 3.1, an IDR at least every 4 s and on request, ABR from
  150 to 1,800 kbit/s, 1,200 at first), refusing sizes it cannot send.
- `huddle_audio::camera_send`: the encoder thread (newest picture wins;
  an encoded frame the session cannot take is dropped and the next made
  a keyframe; a keyframe request at most every 500 ms, kept until its
  turn; a new encoder on a new size or bitrate step), the 90 kHz RTP
  time from the capture instant, and the self-preview (every picture,
  shown before it is encoded, at most 320 wide, mirrored; at first every
  other picture after encoding, which looked laggy).
- The session (`media`, `watch`, `chime`): slot 0, the first video
  m-line, turns `sendrecv` by the same re-SUBSCRIBE machinery (alone or
  with receive changes), SUBSCRIBE goes DUPLEX with a second send stream
  (`AmazonChimeExpressVideo`, stream and group 2, 640×480, 30 fps,
  1,800 kbit/s), `receive_stream_ids[0]` stays 0. Frames go out only
  once the answer has the line sending, from a keyframe (a keyframe is
  asked for until one comes), on the H.264 `42e01f` mode-1 payload type.
  `Event::KeyframeRequest` on our line asks the encoder for an IDR.
  `str0m`'s BWE is on when the session has a camera (start 700 kbit/s,
  desired 1.28 Mbit/s); its estimate less 80 kbit/s sets the encoder.
  SUBSCRIBE_ACK with 206 or receive-only service, for a SUBSCRIBE that
  asked to send, stops sending at once, turns the line `inactive` again
  and tells the app (a toast; the camera closes).
- The app: a Video button beside Mute while live (off: a struck-through
  red camera; on: filled green; Ctrl+Shift+O, ⇧⌘O, as Slack's own chord
  is the composer's paste without formatting here), off on joining,
  greyed until the camera is open; the mirrored self-preview above the
  buttons, and, with
  `huddle-video` too, a "you" tile in the call window (one place fewer
  for others). Toasts for no camera, not allowed (macOS and Windows
  name where to allow it), in use, and view only. macOS's Info.plist
  has `NSCameraUsageDescription`.
- Probe: `--send-test-video` sends the test picture as our camera from
  the start (a build with `huddle-camera`); the log says what was sent,
  keyframe requests and the bandwidth estimate every 5 s.
- Demo: `--demo-view camera` (the bar with the preview, the test
  picture as the camera).

*Measured*: encoding 640×480 is about 4 ms a picture (6 % of one core
at 15 fps, about 12 % at 30) on this machine, from the spike's numbers with the app's
configuration. *Binary size:* `--features huddle-camera` adds
1.04 MB to the release binary (50.44 → 51.48 MB): nokhwa, the encoder
and the camera code.

*Not yet known*: whether Slack shows our video (in the desktop, web
and mobile apps) and with which profile-level-id it is happy (the
encoder writes constraint_set1 only, `42401f`, which is constrained
baseline; the SDP says `42e01f`); whether Chime's answer offers TWCC (if
not, BWE falls back to REMB or stays at its start); whether senders
switch codec once we send; the 206 path has only been tested offline;
nokhwa on macOS and Windows (built and tried on Linux only, and never
against a real camera here).

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

**Stage 4: built (2026-10-07), behind `huddle-share` (brings
`huddle-camera`); not yet tried against Slack.**

*Capture: the comparison.* The rule is pure Rust where it can be;
bindings to the system's own capture APIs are allowed if what they pull
in is said. Read from the crates' sources (registry copies), and built
here where possible.

| | Wayland (portal) | X11 | macOS | Windows | Frames | What it builds |
|---|---|---|---|---|---|---|
| **`xcap` 0.9.8** (Apache-2.0, active) | its `VideoRecorder`: ScreenCast portal over zbus, but monitors only (`types: 1`), no cursor mode, **no restore token**, and it connects to PipeWire's default socket (`connect_rc(None)`) instead of the portal's `OpenPipeWireRemote` fd, so not in the Flatpak; "video recording (WIP)" | xcb (links libxcb) | `objc2` CoreGraphics / AVFoundation; screenshots per call | `windows` crate (GDI, DXGI) | RGBA images (`capture_image`), or the recorder's frames | Linux: `pipewire` (bindgen + C shims, below), `xcb`, `libwayshot-xcap` (wayland-client, drm); macOS/Windows: bindings only, nothing compiled |
| **`ashpd` 0.13.13** (MIT) + **`pipewire` 0.10.1** (MIT) | the whole portal: monitors and windows, cursor modes, persist mode and restore token, `OpenPipeWireRemote` (works in the Flatpak with no finish-args) | – | – | – | PipeWire buffers: SHM/MemFd mapped (`MAP_BUFFERS`), DMA-BUF if modifiers are offered; any raw format asked for; max frame rate negotiable | ashpd: pure Rust on the tree's zbus (async-io feature, as the tree's zbus). pipewire: `pipewire-sys`/`libspa-sys` run **bindgen (libclang)** against **libpipewire-0.3-dev's headers** (pkg-config), and libspa-sys **compiles five small C files** (wrappers for SPA's inline functions, plus libspa's test `pod.c`); the binary links `libpipewire-0.3.so.0` |
| `pipewire-native` 0.2.0 (MIT, PipeWire's own pure-Rust client, WIP) | – | – | – | – | "Further work is required for sending and receiving audio/video": no streams yet | and it still compiles C (`cc`, SPA support shims) |
| `scap` 0.1.0-beta.1 (MIT) | PipeWire + portal | – | ScreenCaptureKit | Windows Graphics Capture | | beta; pulls the same pipewire bindings |
| `lamco-pipewire` 0.8 (MIT/Apache) | PipeWire with DMA-BUF | – | – | – | | pipewire bindings again |
| **`x11rb` 0.13.2** (MIT/Apache, already in the tree) | – | GetImage on the root window per picture, RandR monitors by name; pure Rust connection (no libxcb) | – | – | 32-bit BGRX | nothing |
| Native: ScreenCaptureKit (`objc2-screen-capture-kit`), Windows Graphics Capture (`windows`) | – | – | yes | yes | IOSurface / D3D textures | bindings, but calls are `unsafe`: not in our crate (only the helper may) |

**Chosen:** Linux: **ashpd + pipewire** for the portal (Wayland, and X11
desktops whose portal does ScreenCast), the only option with restore
tokens, windows, cursor and the Flatpak's fd; **x11rb** when there is no
portal and an X server (screens only: without a compositor an X window's
own pixels are not kept). macOS and Windows: **xcap** (bindings, nothing
compiled; its per-call screenshots at 15 a second), behind its target
`cfg` so none of its Linux crates are built. No crate here compiles
C/C++ except libspa-sys's wrapper shims, which are bindings glue for
SPA's header-only API; a pure-Rust PipeWire stream client
(`pipewire-native`) is not there yet. Build needs per platform:
- Linux: libclang (already for nokhwa) and **libpipewire-0.3-dev**
  (CI's Linux jobs install it), a C compiler (already for OpenSSL).
  Run time: libpipewire-0.3.so.0 is linked; the portal
  (xdg-desktop-portal with a GNOME/KDE/wlr backend) and PipeWire must
  run for Wayland; the .deb recommends `xdg-desktop-portal, pipewire`.
- macOS: `objc2` crates (in the tree through xcap's deps), nothing
  compiled; Screen Recording permission asked with
  `CGPreflightScreenCaptureAccess`/`CGRequestScreenCaptureAccess`
  (objc2-core-graphics, safe functions) before the first capture; the
  Info.plist has `NSScreenCaptureUsageDescription`.
- Windows: the `windows` crate, nothing compiled.
- macOS and Windows were **not built here** (no cross targets); the xcap
  code was type-checked and linted against xcap 0.9.8's signatures
  transcribed into a stub crate. CI's macOS/Windows jobs will build it.
- Flatpak: the ScreenCast portal needs **no finish-args** (no
  `--filesystem=xdg-run/pipewire-0`: the portal hands over a PipeWire fd
  that sees only the chosen stream). The Flatpak does not build
  `huddle-share` yet; the freedesktop SDK has PipeWire's headers.
- A quick local test captured from Xvfb (`xvfb-run cargo test
  --features huddle-share -- --ignored x11_capture`); the portal path was
  not run here (it would show the desktop's dialog).

*The content session.* As the JS SDK's `DefaultContentShareController`
(`createContentShareMeetingSessionConfigure`, quoted on
`ChimeJoin::content`): same meeting and URLs, `attendeeId + "#content"`,
`joinToken + "#content"`, same external user id, and
`NoVideoDownlinkBandwidthPolicy`. A second `media::listen` with no
speaker, no microphone (Opus silence goes out, as the JS SDK
synthesizes a silent track for a share without sound), no viewer, and
the share as its "camera": the session code is shared as it was, the
uplink now carrying its own SUBSCRIBE description
(`CameraUplink::descriptor`: 1920×1080, 15 fps, 2,500 kbit/s) and the
bandwidth target following it. Its SUBSCRIBE is RX for the audio, then
DUPLEX with the video send stream, `receive_stream_ids = [0]` however
much video INDEX lists. The main session never subscribes to our own
`#content` (it never did: `Source::is_ours`), and the roster no longer
counts anyone's `#content` attendee as a person. **Slack-side
announcement:** none is sent. HuddleFM's sources make no Slack call
about shares (they only use `rooms.join`, `rooms.info`,
`screenhero.rooms.info` and `rooms.inviteResponse`), and Slack's own
shares were seen only as Chime `#content` sources. The log says so when
a share starts; the probe's summary says whether the `#content` join was
taken. Unknown until tried: whether Slack's join token accepts the
`#content` suffix, and whether Slack's UI shows a share that Slack
itself was not told about.

*Encoding.* Main's `Encoder` (GPU through the helper, software as the
fallback), with `Limits`: a share may be 1920×1080 on the GPU, at most
1280×720 in software. Measured on this machine (Ryzen AI 7 350, Radeon
860M, release, one thread):

| 1080p share, 15 fps, 2.5 Mbit/s | time a picture | CPU a picture |
|---|---|---|
| software (rusty_h264, Fast), mostly still screen | 24.9 ms (p95 30.9) | 24.9 ms |
| software, everything moving | 24.5 ms (p95 26.5) | 24.5 ms |
| software, the helper's bench (the 1080p fixture) | 24.5 ms | 24.5 ms |
| GPU (VA-API) in process | 4.3 ms | 1.0 ms |
| **GPU through the helper** (what the share uses here) | **5.0 ms** | **1.9 ms** |

| 720p share in software | 9.1 ms mostly still, 9.9 ms everything moving (p95 ≤ 11.3) |
|---|---|

Software 1080p would take 37 % of a core at 15 fps here and more than a
frame's time on older laptops, so software shares are 720p
(`share_encode_cost`, ignored test; `examples/encode` in the helper).
On this machine with "Use the graphics card for video" on, a share goes
out at **1080p at 15 fps from the GPU**. The encoder thread also: skips
pictures equal to the last (GNOME's PipeWire sends frames only on
damage anyway), sends a still screen's picture again once a second and
as an IDR every 4 s (and on PLI/FIR, at most every 500 ms; Chime asks
content senders every 10 s), follows the bandwidth estimate in steps up
to 2.5 Mbit/s (in place on the GPU), steps down to 720p if 1080p takes
over 45 ms a picture on average, and to 720p at once when no encoder
takes 1080p (no GPU, or the GPU failing).

*Rules and UI.* `ShareControl` keeps the camera's rule (nothing captured
until asked; stop, leaving, a failed or refused share session, the
capture ending by itself all stop it), tested with a pretend capturer.
Share sits beside Mute and Video in the call bar (icons alone there when
the bar is narrow) and in the call window, Ctrl+Shift+E (Teams' chord;
on the shortcut sheet); on, the bar says "You are sharing your screen"
with Stop sharing; without a system dialog the bar lists screens and
windows to pick; right click Share for "Share something else…" (asks
the portal again). Toasts: cancelled, not allowed (macOS names where),
no capture available, the source gone, capture failed, two shares
already (Chime's 206/509, or two in INDEX with `huddle-video`), the
share's connection refused or lost. Demo: `--demo-view sharing-self`,
`share-pick`, `share-window`.

*Probe.* `--send-test-share`: once the audio is live, shares a 1080p
test screen (colour bars, a moving clock and frame count) as the
`#content` attendee; the summary's "share" lines say how it went.

*Not yet known:* everything Slack-side (above); GNOME vs KDE portal
behaviour (frame rates, cursor, restore tokens), and DMA-BUF-only
compositors (frames arriving that are not in memory are logged once);
xcap's speed on real Macs and Windows machines.

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

## 6. Hardware decoding in a helper process (2026-10-07)

Built on `feat/hw-video`: the architecture and the first back end,
VA-API decoding on Linux. The decision behind it (TODO.md): the
platform APIs directly, in a separate process, `unsafe` allowed there
and nowhere in the app.

### 6.1 Design

- **Two crates in a workspace.** `crates/video-ipc`
  (`noslacking-video-ipc`, `forbid(unsafe_code)`, std only) holds the
  messages; `crates/noslacking-video` is the helper, a `[[bin]]` with a
  library for its tests and benchmark. The helper's crate denies
  `unsafe` too, except two modules that say so: `vaapi::va` (the libva
  calls) and `pipe` (one `fcntl`). The root `Cargo.toml` gained a
  `[workspace]`; `cargo build` at the root still builds only the app,
  so `--locked`, features and the existing commands keep working, and
  CI and the release builds name `-p noslacking -p noslacking-video` or
  `--workspace`.
- **Process.** The app (`src/huddle_audio/hardware.rs`) starts the
  helper the first time a stream starts (a keyframe after waiting), from
  beside its own executable (`current_exe`), else `PATH`; not found means
  software, said once in the log. One helper serves every stream; a
  writer thread and a reader thread talk to its stdin and stdout, so a
  helper that stops reading or answering cannot block the caller past
  the timeout (5 s for the hello, which opens the GPU; 1 s per frame).
  Its stderr goes to the app's debug log. It exits when its stdin
  closes, which happens when the app drops it or exits.
- **Protocol (version 2; §6.2 for what 2 added, §6.9 for 3).** Each frame is `u32` length, `u32` sequence
  number (the reply repeats it), a tag and fields, little-endian; byte
  strings carry a `u32` length; frames over 32 MiB and pictures over
  4096 a side are refused before anything is allocated. `Hello{magic,
  version}` → `Welcome{version, backend, capabilities[codec, decode |
  encode, max size]}`; `OpenDecoder{codec, size hint}` → `Opened{id}`;
  `Decode{id, keyframe, Annex B frame}` → `Picture{I420 planes}` |
  `NoPicture` | `Failed{kind, detail}`; `SetOutputSize{id, box}`; `Close{id}`; and for later
  `OpenEncoder{size, fps, bitrate}`, `Encode{id, force keyframe, I420}`
  → `Encoded{keyframe, NALs}`, `SetBitrate`. Failure kinds say what the
  app does next: `NeedKeyframe`, `Broken` (wait for a keyframe),
  `Unsupported` (software for this stream), `Device` / `Protocol` /
  `UnknownId` (the helper is in trouble: software). A version mismatch
  means software for the session.
- **Trust.** Every field is checked on reading (lengths against the
  bytes there are, enums against known values, a picture's three planes
  against its size), and the app also refuses a picture larger than the
  helper's welcome promised. Malformed messages, cut frames and lying
  plane lengths are tested.
- **Fallback is always software** (in the app until §6.9, which moved it into the helper and made a lost helper mean no video). Hardware is tried only when the
  setting is on, the helper is there and its welcome covers H.264 at the
  stream's SPS size. A stream whose helper crashes, hangs (timeout →
  killed) or garbles a reply goes on in software at once if the frame in
  hand is a keyframe, else asks for one (the decoder thread's
  `want_keyframe` → PLI). The helper is started again for the next
  stream start, at most 3 times; after the fourth failure hardware is
  off until the app restarts. `Unsupported` (B slices, fields,
  interlace, a profile the driver lacks) and a GPU failure on a keyframe
  keep that stream in software without counting against the helper.
- **Setting.** Settings → Huddles → "Use the graphics card for video"
  (first "Decode video on the graphics card"; `hardware_video`, which
  since §6.8 also covers encoding our camera), for streams that start
  afterwards; on by default
  since 2026-10-07, on the §6.3 numbers (off before GPU scaling, when
  it did not yet beat software).

### 6.2 Moving pictures: the pipe, carrying only what is shown

A 1080p I420 picture is 3.1 MB. Measured on this machine (Ryzen AI 7
350, Radeon 860M, Fedora 44, release build): a framed 1080p picture
through two pipes and `cat` takes 0.65 ms. The first version of the
helper path cost 6.6 ms a 1080p frame against 3.0 ms in process; two
changes brought it to 3.9 ms:

| change | 1080p through the helper |
| --- | --- |
| first version (64 KiB pipe, picture copied into one message) | 6.6 ms |
| helper's stdout pipe raised to 1 MiB (`F_SETPIPE_SZ`) | 5.3 ms |
| planes written and read straight from and into their vectors | 3.9 ms |

So the pipe now costs about 0.9 ms of latency a 1080p frame (nothing
measurable at 480×480), well inside a frame's 83 ms at 12 fps, but CPU
in the kernel too. Decided (2026-10-07): keep the pipe, no shared
memory, and send less: the helper shrinks each picture **on the GPU**
to the size the app shows it at, before reading it back, so both the
copy back and the pipe carry only that (§6.3).

- **Protocol 2** adds `SetOutputSize{id, width, height}`: the box the
  pictures should cover (0×0: their own size). The app sends it when
  the shown size changes (the call window's share; a camera tile's
  size, already in 32 px steps), not with every frame. The size is
  `noslacking_video_ipc::output_size`: scaled by the larger of the two
  ratios so it still covers the box both ways, the aspect ratio kept,
  never larger than the source, even each way. Version 1 and 2 do not
  mix; app and helper ship together, and a mismatch means software.
- **Scaling on the GPU:** VA-API video processing
  (`VAProfileNone`/`VAEntrypointVideoProc`, one context per stream,
  made on first need), a `VAProcPipelineParameterBuffer` scaling the
  decoded surface's visible region into an NV12 output surface (a pool
  of four, by size), BT.601 studio range in and out, the default
  scaling filter. radeonsi offers it (`vaQueryVideoProcPipelineCaps`:
  no special pipeline or filter flags, outputs from 16 to 10240 wide).
  The small surface is read back as before (`vaDeriveImage`, else
  `vaGetImage`). The structure's size and offsets (224 bytes, five
  padding holes written out) are clang-checked in tests like the rest.
  The scaled pictures are within a mean difference of 0.07 (share at
  960×540) and 0.23 (camera at 240×240) of the software decoder's,
  ffmpeg-exact, pictures shrunk on the CPU (ignored GPU test).
- **Without video processing** (a driver that lacks it, or a failure),
  the helper shrinks on its CPU by a whole step (`shrink.rs`, the app's
  box filter), so the pipe still carries the small picture.
- **The app** shrinks nothing more: a picture that already covers the
  shown size gives `decode::reduction` 1. The software path shrinks as
  before.

### 6.3 Measurements (this machine, release)

Ryzen AI 7 350, Radeon 860M, Fedora 44, Mesa 26.2.3. `examples/bench.rs`:
each fixture ten times, one frame at a time. "CPU" is user + system time
of the bench and the helper together, from /proc, per frame; software is
the app's path (rusty_h264, then its whole-step shrink).

| 1080p share (36 frames) | wall | CPU |
| --- | --- | --- |
| full size: software | 1.8 ms | 1.8 ms |
| full size: helper, GPU | 3.7 ms | 2.5 ms |
| shown 960 wide: software (decode + shrink) | 4.5 ms | 4.4 ms |
| shown 960 wide: helper, scaled on the GPU | **1.8 ms** | **0.7 ms** |
| shown 960 wide: helper, shrunk on its CPU | 6.2 ms | 4.6 ms |
| shown 640 wide: software | 4.0 ms | 4.0 ms |
| shown 640 wide: helper, scaled on the GPU | **1.4 ms** | **0.5 ms** |
| shown 640 wide: helper, shrunk on its CPU | 4.4 ms | 2.8 ms |

| 480×480 camera (66 frames) | wall | CPU |
| --- | --- | --- |
| full size: software | 0.43 ms | 0.42 ms |
| full size: helper, GPU | 0.76 ms | 0.33 ms |
| 240 tile: software | 0.85 ms | 0.85 ms |
| 240 tile: helper, scaled on the GPU | **0.64 ms** | **0.17 ms** |
| 240 tile: helper, shrunk on its CPU | 1.15 ms | 0.55 ms |

On the GPU in the bench's own process (no pipe) the 960-wide share is
1.4 ms and 0.36 ms of CPU, so the pipe now adds little. Before the
shrink (first version of this section) the helper cost 3.9 ms a 1080p
frame against software's 2.1 ms. Two surprises: the app's whole-step
shrink costs more than decoding (2.7 ms to halve a 1080p picture, a
plain per-pixel box filter); a faster one (a 2× special case, SIMD)
would narrow the gap for software too. And at full size, where nothing
is shrunk, software still wins on this machine.

The demo (`NOSLACKING_DEMO_HARDWARE_VIDEO=1 noslacking --demo
--demo-view call-window`, under Xvfb at 1600×1000: the 1080p share
shown at about 1624×914 and four cameras in 352 px tiles), CPU of the
decoding threads (and the helper) over 20 s: software 2.73 s (14 % of a
core), GPU 0.64 s in the app + 1.54 s in the helper = 2.18 s (11 %). In
that window the share is shrunk only a little, so the gain is small;
and each picture took longer (7.6–9.5 ms against 5.7–6.1 ms for the
share, 3.1–3.7 ms against 1.35 ms for a camera) because the share's and
the cameras' threads share one helper and wait for each other's
replies. Pipelining requests, or a helper per decoding thread, would
remove the wait.

Decoding is bit-exact with ffmpeg on both fixtures at full size.
Reading back with `vaDeriveImage` takes 1.35 ms a 1080p frame against
2.6 ms with `vaGetImage`. **Decision:** the setting is off by default
for now. On these numbers it should be turned on: where the picture is
shown smaller than it is (most windows and every camera tile) it takes
3 to 6 times less CPU and less time; it loses only for a share shown at
its full size, and costs latency when many streams share the helper.
The real win, a decoded surface shown as a GL texture with no copy back
(dmabuf), needs `unsafe` GL in the app or eframe on wgpu.

### 6.4 VA-API binding: libva opened at run time, declarations by hand

| option | build needs | runtime | verdict |
| --- | --- | --- | --- |
| `cros-libva` 0.0.13 (ChromeOS, BSD-3) | bindgen + libclang + libva headers (libva-dev), links libva | helper will not start without libva2 | good API, but the build and link dependency, and `cros-codecs`' decoder on top (rejected in TODO) |
| `libva-sys` 0.1.2 | bindgen + headers | links libva | unmaintained since 2021 |
| GStreamer / ffmpeg | large C stacks | | rejected in TODO |
| **chosen:** `libloading` (ISC) + ~25 functions and 6 structures copied from libva 2.23's `va.h` | nothing | `libva.so.2` / `libva-drm.so.2` opened at start; missing → "no hardware" | small, checkable |

The structures' sizes and field offsets were computed by clang from
libva 2.23's headers and are asserted in tests; their C padding is
written out as fields, so the bytes handed to libva are all
initialized. The stateless decoding state is ours (`src/h264.rs` in
the helper, ~650 lines without its tests): picture order count (types 0, 1, 2), sliding
window and adaptive reference marking (all MMCO), P-slice reference
lists with modifications, cropping; B slices, fields, interlace, data
partitioning and slice groups are refused as `Unsupported`. The H.264
parser is `cros-codecs`' (BSD-3), used with no features: its parser and
bit reader need only `log` (the crate's three other `unsafe` uses are
in its DPB, which we do not use, and its tests). Packaging: no build
dependency, so CI needs no libva-dev; the `.deb` recommends `libva2,
libva-drm2, mesa-va-drivers | intel-media-va-driver`, the `.rpm`
suggests `libva.so.2`; the Flatpak runtime ships libva 2.24 and its GL
extension Mesa's VA drivers (checked: `noslacking-video --probe` in
`org.freedesktop.Platform//26.08` finds H.264 decoding on radeonsi);
Intel's media driver is the `org.freedesktop.Platform.VAAPI.Intel`
extension.

### 6.5 Vulkan Video (`gpu-video`, formerly `vk-video`)

`gpu-video` 0.4.0 (Software Mansion, from Smelter; MIT; renamed from
`vk-video` in April 2026, releases monthly) decodes and encodes H.264
(and H.265) over Vulkan Video with a safe API, on Linux and Windows
(not macOS), into wgpu textures or, without its default `wgpu` feature,
NV12 bytes. Its dependencies are `ash`, `vk-mem` (which compiles AMD's
VMA, C++), `h264-reader` and small crates; wgpu and naga only with
features. About 170 `unsafe` uses inside, none needed from us, so it
could run in process, but a GPU driver crash would still take the app
down: it belongs in the helper too.

| | VA-API | Vulkan Video (`gpu-video`) |
| --- | --- | --- |
| platforms | Linux (Intel, AMD, some NVIDIA via nvidia-vaapi-driver) | Linux and Windows: NVIDIA, AMD (RADV, Mesa ≥ 24), Intel (ANV, Mesa ≥ 24) |
| this machine | H.264 decode (Fedora's `mesa-va-drivers-freeworld`) | **no H.264**: Fedora builds RADV without the patented codecs; `vulkaninfo` lists AV1 and VP9 decode only |
| our fixtures | bit-exact, 1.8 ms a 1080p decode | could not be tried |
| encode, IDR control | built (§6.8): CB, CBR, IDR on request | yes, with IDR on request |
| licence, upkeep | libva MIT; our code | MIT; active |
| what we own | ~650 lines of H.264 state, ~900 of libva wrapper (36 `unsafe` blocks) | none of the codec logic |

Chosen for the first back end: VA-API, because it is what Linux
distributions ship H.264 support in (Fedora and others strip it from
Mesa's Vulkan drivers but carry it in VA drivers from RPM Fusion and
the like), it works and is verified here, and it reaches older GPUs.
Vulkan Video is the plan for Windows (one back end for NVIDIA, AMD and
Intel instead of Media Foundation's three paths) and a second Linux
back end where VA-API is missing; the `Backend` trait takes it as it
is (frames in, I420 out). `ralfbiedert/vulkan_video` (0.1.0, 2023,
BSD-2) is early bindings with no decoder; not useful.

### 6.6 V4L2 memory-to-memory (ARM Linux)

Two kinds, both behind the same `Backend` trait:

- **Stateful** (Qualcomm Venus/Iris on Snapdragon, also Amlogic,
  Raspberry Pi's bcm2835-codec): the driver parses the stream; feed
  Annex B frames into the OUTPUT queue, take NV12 from CAPTURE. Like
  VideoToolbox in shape. `v4l2r` 0.0.8 (ChromeOS; MIT text under a
  `license-file`, so deny.toml needs a clarify entry; ~150 `unsafe`;
  bindgen against the kernel headers at build time, as nokhwa's V4L2
  already does) has a stateful decoder (`v4l2r::decoder::stateful`).
- **Stateless** (Rockchip rkvdec and Hantro, MediaTek, Allwinner
  cedrus): the media request API with H.264 controls
  (`V4L2_CID_STATELESS_H264_SPS/PPS/DECODE_PARAMS/SLICE_PARAMS`); we
  parse and manage the DPB, which is exactly what `src/h264.rs` in the
  helper already does for VA-API. `cros-codecs` 0.0.6 has a V4L2
  stateless H.264 back end (its `v4l2` feature) to read for the control
  layout.
- **Detection:** enumerate `/dev/video*`, keep devices whose
  capabilities have `V4L2_CAP_VIDEO_M2M(_MPLANE)`, and whose OUTPUT
  formats list `V4L2_PIX_FMT_H264` (stateful) or `H264_SLICE`
  (stateless) and CAPTURE formats `NV12`; none → "no hardware".
- Not built: this machine is x86 with no M2M decoder, and `vicodec`
  (the kernel's test codec, present here as a module) speaks FWHT, not
  H.264, and needs root to load. The plan is in TODO.md.

### 6.7 Back ends by platform

| platform | back end | crate | state |
| --- | --- | --- | --- |
| desktop Linux, Intel/AMD | VA-API | libloading + own FFI + own H.264 state | **decoding and encoding work** (§6.8) |
| desktop Linux without VA H.264, NVIDIA | Vulkan Video | `gpu-video` | planned |
| ARM Linux (Snapdragon, Raspberry Pi) | V4L2 stateful | `v4l2r` | planned |
| ARM Linux (Rockchip, MediaTek, Allwinner) | V4L2 stateless | `v4l2r` + `src/h264.rs` | planned |
| Windows (x86 and Snapdragon) | Vulkan Video, else Media Foundation / D3D11 video | `gpu-video`; `windows` | planned; the helper builds and reports no hardware |
| macOS | VideoToolbox | `objc2-video-toolbox`, `objc2-core-media` | planned; the helper builds and reports no hardware |
| everywhere | software | rusty_h264 (decode), rusty_h264-encoder | the default and the fallback |

### 6.8 Encoding on the GPU (VA-API, `feat/hw-encode`, 2026-10-07)

Our camera (and later a share) encoded by the GPU through the same
helper, with rusty_h264-encoder as the fallback.

**What the driver offers here** (radeonsi, Mesa 26.2.3, Radeon 860M):
constrained baseline, Main and High at `VAEntrypointEncSlice` only (no
`EncSliceLP`, Intel's low-power entry point, which is preferred where
it exists); rate control CBR, VBR, CQP and QVBR (`0x416`); packed
headers sequence, picture, slice, misc and raw (`0x1f`); two list-0
references; up to 4096×4096.

**Design** (`crates/noslacking-video/src/vaapi/encoder.rs`, bindings in
`vaapi/va/enc.rs`):

- Constrained baseline (`VAProfileH264ConstrainedBaseline`): CAVLC, one
  reference (each P from the picture before), no B-frames, one slice a
  picture (packetization-mode 1 splits it), picture order type 2 (no
  order count in slice headers), every picture a reference. Level 3.1
  up to 720p@30 (the software encoder's), then 3.2, 4.0 (1080p@15–30),
  4.2 (1080p@60, 2048×1088); past 4.2 refused. Sizes not whole
  macroblocks are cropped in the SPS, the surface padded by repeating
  the last row and column.
- **Headers are ours, packed**: the SPS and PPS together as
  `VAEncPackedHeaderSequence` at each IDR, and each picture's slice
  header as `VAEncPackedHeaderSlice` (`src/nal.rs`, ~120 lines and
  tests; parsed back by cros-codecs' parser in a test). Mesa needs them:
  without packed headers it wrote no SPS or PPS and a slice NAL header
  of `0x00` (type 0); with only SPS and PPS packed, the same. With both
  (what ffmpeg does there; seen with `LIBVA_TRACE`) Mesa writes our
  SPS/PPS as they are and rewrites the slice header with its QP. A
  driver that takes no packed headers writes its own, and ours go in
  front of an IDR that came without them. The SPS says `42e0xx`
  (constraint_set0–2, as WebRTC) and, in its VUI, timing and
  `max_dec_frame_buffering` 1, so decoders show each picture at once.
- **Rate control**: CBR where the driver has it, else VBR (target
  100 %), window 1 s, HRD buffer half a second, frame skipping and bit
  stuffing off (a still screen costs nothing). `SetBitrate` sends a new
  `VAEncMiscParameterRateControl` (with `reset`) with the next picture:
  no keyframe, no new context. Followed: 900 → 450 kbit/s mid-sequence
  came out 832 then 462.
- **IDRs** when asked and at least every 4 s (`fps × 4` pictures).
- **Pictures in**: the app's I420 is written into an NV12 surface
  through `vaDeriveImage` (else `vaCreateImage` + `vaPutImage`), checked
  against the image's pitches and size before a byte is written.
  **Out**: one coded buffer (raw size, at least 256 KiB) read with
  `vaMapBuffer`, its segments walked (at most 64, each checked to fit)
  and joined. Every picture's NAL units are checked (a slice; an IDR
  when one was asked, SPS and PPS before it) before they go out, in the
  helper and again in the app.
- **Bindings**: `VAEncSequenceParameterBufferH264` (1132 bytes),
  `…Picture…` (648), `…Slice…` (3140), the rate control (60), frame
  rate and HRD (24) misc payloads, the packed header parameter (28) and
  `VACodedBufferSegment` (48): sizes and offsets from clang over libva
  2.23's headers asserted in tests, padding written out, bit fields
  checked against what clang made of the same assignments. 11 more
  `unsafe` blocks (map/unmap, coded segments, `vaPutImage`, the coded
  buffer), each with its SAFETY note.
- **Protocol**: unchanged (version 2; `OpenEncoder`, `Encode`,
  `SetBitrate` were defined): the welcome now lists `Encode` for H.264
  (meaning constrained baseline) up to 2048×1088. A picture to encode
  is streamed from its planes into the pipe and read straight into its
  own (`write_request`/`read_request`), as decoded pictures come back.

**The app** (`src/huddle_audio/video_encoder.rs`): `Encoder` wraps
either rusty_h264 (`VideoEncoder`) or the helper's (`HwEncoder` in
`hardware.rs`). It takes the GPU when the setting is on ("Use the
graphics card for video", the `hardware_video` key kept) and the
helper's welcome covers the size; `retune(bitrate)` changes the GPU's
rate in place (the camera thread follows the bitrate steps within a
second; software still makes a new encoder at most every 8 s). Any
GPU failure (a crash, a hang past 1 s, a failure reply, a stream that is
not what was asked) encodes that same picture in software as a
keyframe, and the camera stays in software for the session. The
helper's crash counts against its restarts as for decoding.
`huddle-camera` now builds `hardware.rs` (and the protocol crate) too.

**Measurements** (release, `examples/encode.rs`, five rounds each:
the 480×480 camera fixture stretched to 640×480 at 30 fps and
900 kbit/s, the 1080p share fixture at 15 fps and 2.5 Mbit/s; CPU is
the bench's and the helper's; PSNR is luma against the source,
decoded by rusty_h264):

| 640×480@30, 900 kbit/s | per picture | CPU | out | PSNR |
| --- | --- | --- | --- | --- |
| software (rusty_h264, Fast) | 4.33 ms | 4.30 ms | 902 kbit/s | 35.3 dB |
| GPU, in process | 0.87 ms | 0.24 ms | 916 kbit/s | 38.0 dB |
| GPU, through the helper | 1.02 ms | 0.42 ms | 916 kbit/s | 38.0 dB |

| 1920×1080@15, 2.5 Mbit/s | per picture | CPU | out | PSNR |
| --- | --- | --- | --- | --- |
| software (level 4.0) | 31.0 ms | 30.7 ms | 2375 kbit/s | 56.8 dB |
| GPU, in process | 2.68 ms | 1.00 ms | 1447 kbit/s | 52.7 dB |
| GPU, through the helper | 5.2 ms | 2.2 ms | 1447 kbit/s | 52.7 dB |

At camera rates the GPU is better and 4× faster at a tenth of the
CPU. A share is mostly still: CBR without stuffing leaves the GPU well
under its rate and a little behind software's quality, which spends
the whole budget at 31 ms a picture (two thirds of a 15 fps frame's
time on one core). Through the pipe a 1080p picture costs 2.6 ms more
(3 MB in); a 1 MiB input pipe made it worse (11.8 ms, writer and reader
no longer overlap), so the helper's input stays at 64 KiB. Checked
with ffmpeg: both streams decode without an error as Constrained
Baseline (level 3.1 and 4.0), and against the sources give PSNR
36.4 dB (camera, whole stream) and 49.8 dB (share), as rusty_h264's
decoding of them does. Forced IDRs and the 4 s ones come where asked
(ignored GPU test `vaapi_encodes_what_the_software_decoder_reads_back`).

### 6.9 All decoding in the helper (step 1 of "All video in the helper", 2026-10-07)

The app no longer links a decoder: rusty_h264-decoder and the
whole-step shrink moved into `noslacking-video`, and every stream the
call window shows is decoded there, on the GPU or in software. Why
(TODO.md): release builds abort on any panic, and a decoder reads
strangers' network data, so its bug should end the helper (which the
app starts again) rather than the app; and one place holds the codec
and hardware choices.

- **Protocol 3.** `OpenDecoder` gains `hardware` (try the GPU: Settings
  → Huddles → Use the graphics card for video, now GPU against
  software *inside* the helper), and always opens. A picture
  (`Decoded`) carries its stream's own size before shrinking and
  whether the GPU decoded it; it is refused if larger than that
  source. The welcome's capabilities stay the GPU's; software decoding
  needs none. Version 2 and 3 do not mix: another version means no
  video.
- **In the helper** (`software.rs`, `backend::open_decoder`): software
  is rusty_h264 made afresh after an error and waiting for a keyframe,
  then `shrink.rs` to the box shown. With `hardware`, the GPU's decoder
  goes first and software takes over (for that decoder) on
  `Unsupported`, a device failure or a keyframe the GPU breaks on, the
  keyframe in hand decoded at once, any other frame answered
  `NeedKeyframe`. Bit-exact with ffmpeg on both fixtures (tests in the
  helper and, through the helper's code on a thread, in the app).
- **In the app** (`decode::H264`, `helper::RemoteDecoder`): each start
  (a keyframe after waiting) opens a decoder in the helper. A picture
  that came from software while the GPU was asked for marks the stream
  software-only, so the next start does not try the GPU again; so does
  a helper failure (crash, hang past 1 s, a garbled reply), since the
  stream may have caused it: the stream waits for a keyframe (PLI) and
  starts again, in software, in the restarted helper. After
  `MAX_RESTARTS` failures, or with no helper installed or one of
  another version, the call window says "No video" (translated) instead
  of waiting, and camera tiles show faces without spinners.
- **A helper per lane.** The share, the camera tiles and our own
  encoding each get their own helper process (`helper::Lane`), so the
  share's and the cameras' threads no longer wait for each other's
  replies (§6.3's 3.1 ms camera pictures) and keep the parallelism the
  two decoding threads had in the app. Each loads the GPU driver once
  (a few hundred milliseconds on its first stream, some MB of memory).
- **Builds.** `default-members` makes `cargo build` (and the CI demo
  build) build the helper beside the app; `cargo run` alone does not,
  so the demo shows "No video" until it is built. Every package
  already shipped the helper; the macOS bundle script now requires it.
  The app binary lost the decoder (52.3 → 51.4 MB, release, with demo
  and huddle-video); the helper grew from 0.6 to 1.5 MB. App tests run
  the helper's server on a thread (`helper::pretend::InThread`, the
  helper crate as a dev-dependency), never the program.

**Measured** (this machine as §6.3, release). `examples/bench.rs`, CPU
of the bench and the helper together, per frame:

| | in the app before (software) | helper, software | helper, GPU |
| --- | --- | --- | --- |
| 1080p share, full size | 2.0 ms (2.0 CPU) | 3.2 ms (3.5 CPU) | 5.1 ms (3.1 CPU) |
| 1080p share shown 960 wide | 5.0 ms (4.9) | 5.7 ms (5.8) | 1.6 ms (0.6) |
| 1080p share shown 640 wide | 3.5 ms (3.5) | 4.2 ms (4.2) | 1.4 ms (0.4) |
| 480×480 camera, full size | 0.30 ms (0.29) | 0.42 ms (0.44) | 0.75 ms (0.36) |
| 480×480 camera in a 240 tile | 0.60 ms (0.61) | 0.67 ms (0.68) | 0.70 ms (0.20) |

The demo's call window (Xvfb 1600×1000; the 1080p share at 12 fps
shown about 1580 wide, so not shrunk in software, and three 480×480
cameras at 22 fps in tiles too large to shrink), CPU over 20 s, two
runs each:

| | app, all threads | of it, video threads | helpers | video in all |
| --- | --- | --- | --- | --- |
| before: software in the app | 6.9–7.4 s | 2.6–2.8 s (decoding, converting) | — | 2.6–2.8 s |
| after: software in the helpers | 6.0–6.1 s | 1.3 s (converting 0.8, pipe 0.5) | 3.0 s | 4.3 s |
| after: GPU in the helpers | 5.3 s | 0.9 s | 2.0 s | 2.9 s |

So the app itself spends half what it did on video, and the rest of
its time is drawing (llvmpipe under Xvfb). In software the whole costs
about 1.6 s more per 20 s (8 % of a core), nearly all the pipe: a
1080p picture at full size is 3 MB each way through the kernel, read
on the app's `video-helper-out` thread (1.2 ms more a 1080p frame in the
bench; 0.1 ms for a camera). Where pictures are shown smaller, the pipe
carries the small one and the difference is small (0.7 ms at 960
wide). Each 1080p picture also takes longer to arrive (7 ms against
4.5 ms in the demo's log), still far inside a 12 fps frame.

**For steps 2 and 3** (capture and encoding in the helper): the lane
split already gives sending its own helper; a full-size 1080p picture
over the pipe is the expensive case, both ways, so capture should hand
the helper dmabufs or stay in the helper rather than send raw frames
(the share path's 3 MB pictures in, §6.8, cost the same 2.6 ms);
shared memory would also cut the full-size decoding cost above. A
faster shrink (a 2× special case) now pays in the helper. The software
encoder is the next thing to move, behind the same `hardware` switch.

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
- Hardware (§6): libva 2.23 `va/va.h`; crates.io for cros-libva, cros-codecs, libva-sys, gpu-video, vk-video, vulkan_video, v4l2r; `vulkaninfo`, `noslacking-video --probe` on this machine and in the Flatpak runtime.
- crates.io / GitHub for: str0m, openh264, rusty_h264, rust_h264, image-webp, oxideav-vp8, env-libvpx-sys, vpx-rs, shiguredo_libvpx, nokhwa, ashpd, pipewire, xcap, scap, screencapturekit, windows-capture, yuv (awxkee/yuvutils-rs), dcv-color-primitives, rav1e, ffmpeg-next, gstreamer.
