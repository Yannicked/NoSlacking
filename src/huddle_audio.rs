//! Listening to a huddle: the `huddle-audio` feature.
//!
//! A huddle is an Amazon Chime meeting. `rooms.join` (a browser session's
//! method, like the rest of [`crate::huddles`]) hands out the meeting's
//! addresses and this attendee's join token (`join`). From there it is
//! Chime's own protocol, the one AWS publishes in amazon-chime-sdk-js: a
//! signaling WebSocket carrying protobuf frames (`chime`,
//! `signaling`), a TURN relay, since Chime's media servers are reached
//! only through one (`turn`), a WebRTC session for the audio
//! (`media`, with `str0m`), and Opus decoded into the sound device
//! (`jitter`, `speaker`).
//!
//! Talking: joined muted, the microphone closed and Opus silence going
//! out, as a muted browser sends. Unmuting opens the microphone
//! (`microphone`), cleans it up with WebRTC's echo cancellation, noise
//! suppression and gain control against what the huddle plays
//! (`processing`), encodes it (`encoder`) and sends it on the same audio
//! track (`uplink`); Chime hears of each mute and unmute in an
//! AUDIO_CONTROL frame. `probe` runs the whole path from the command line
//! (`noslacking --huddle-probe`, with `--send-tone` to be heard without
//! talking) so it can be tried against a real huddle and its log sent
//! back. Listening has been heard working against Slack, talking not yet;
//! what was checked and what is next is in TODO.md.
//!
//! Video (docs/research/huddle-video.md): `video` reads INDEX and chooses
//! streams, `watch` renegotiates `recvonly` m-lines for them and counts
//! what arrives, `bitstream` reads just enough of H.264 and VP8 to name
//! the codec, profile and size. The probe and `--video N` look at any
//! stream (Stage 0). With the `huddle-video` feature (Stage 1) the app
//! watches screen shares: only the share the call window shows is
//! received, `decode` turns its H.264 into pictures and `screen` runs
//! that on a thread of its own, keeping only the newest picture.
//!
//! Secrets: the join token and the TURN password never reach the log; the
//! types holding them print `<redacted>`. Chime's URLs are logged by host
//! only.

#![warn(missing_docs)]

pub mod bitstream;
pub mod chime;
#[cfg(feature = "huddle-video")]
pub mod decode;
pub mod dtls;
pub mod encoder;
pub mod jitter;
pub mod join;
pub mod media;
pub mod microphone;
pub mod probe;
pub mod processing;
pub mod region;
pub mod roster;
#[cfg(feature = "huddle-video")]
pub mod screen;
pub mod sdp;
pub mod signaling;
pub mod speaker;
pub mod turn;
pub mod uplink;
pub mod video;
pub mod watch;

/// The host of `url`, for the log: what a protocol mismatch needs to
/// know, without the path or query a URL could carry a secret in.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    // Credentials before an `@` are never shown.
    host.rsplit_once('@').map_or(host, |(_, host)| host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_logged_by_host_alone() {
        assert_eq!(
            host_of("wss://signal.m1.ue1.app.chime.aws/control/abc?x=1"),
            "signal.m1.ue1.app.chime.aws"
        );
        assert_eq!(
            host_of("https://user:pw@example.com:443/x"),
            "example.com:443"
        );
        assert_eq!(
            host_of("turn:1.2.3.4:3478?transport=udp"),
            "turn:1.2.3.4:3478"
        );
        assert_eq!(host_of(""), "");
    }
}
