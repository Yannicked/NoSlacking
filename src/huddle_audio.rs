//! Listening to a huddle: the spike behind the `huddle-audio` feature.
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
//! Only listening: the microphone stays off and Chime is told this
//! attendee is muted. `probe` runs the whole path from the command line
//! (`noslacking --huddle-probe`) so it can be tried against a real huddle
//! and its log sent back. Nothing here is proven against Slack yet; see
//! TODO.md.
//!
//! Secrets: the join token and the TURN password never reach the log; the
//! types holding them print `<redacted>`. Chime's URLs are logged by host
//! only.

#![warn(missing_docs)]

pub mod chime;
pub mod join;
pub mod sdp;
pub mod signaling;
pub mod turn;

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
