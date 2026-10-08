//! What a call's end and acknowledgement codes mean, in one place (C.4),
//! as `backend/api.rs` does for Slack's errors.
//!
//! The codes are SIP's. Only 0 (a normal end) and 487 (our own hang-up
//! while it rang) are recorded; 603 for a decline is what we send when we
//! decline, and the rest follow SIP's meaning, unverified.

use crate::failure::Failure;
use crate::teams::calling::types::{ConversationEnd, Outcome};

/// How a call ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Someone hung up, or the call was cancelled while it rang.
    Normal,
    /// It ended because something failed, or nobody took it.
    Failed(Failure),
}

/// What `code` means at the end of a call; `phrase` is Microsoft's
/// English, kept only for a failure nothing else names.
fn ending(code: i64, phrase: &str) -> Ending {
    match code {
        // 487: the call was cancelled while it rang, by us (recorded) or
        // by the caller.
        0 | 487 => Ending::Normal,
        // Declined; busy, which a person's other device may say for them.
        603 | 486 | 600 => Ending::Failed(Failure::CallDeclined),
        // Not answered in time, or nobody reachable.
        408 | 480 => Ending::Failed(Failure::CallNotAnswered),
        _ => Ending::Failed(Failure::CallFailed(if phrase.is_empty() {
            code.to_string()
        } else {
            phrase.to_owned()
        })),
    }
}

/// What a `call/end` push means.
pub fn call_end(end: &Outcome) -> Ending {
    ending(end.code, &end.phrase)
}

/// What a `conversation/conversationEnd` push means: the call's own end,
/// if it carries one, else the conversation's code.
pub fn conversation_end(end: &ConversationEnd) -> Ending {
    match &end.call_controller_transaction_end {
        Some(call) => call_end(call),
        None => ending(end.code, &end.phrase),
    }
}

/// Whether a `call/mediaAcknowledgement` took our answer.
pub fn acknowledgement(ack: &Outcome) -> Result<(), Failure> {
    if ack.code == 0 {
        Ok(())
    } else {
        log::warn!(
            "Teams refused our media answer: {} {} ({})",
            ack.code,
            ack.sub_code,
            ack.phrase
        );
        Err(Failure::CallFailed(if ack.phrase.is_empty() {
            ack.code.to_string()
        } else {
            ack.phrase.clone()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(code: i64, phrase: &str) -> Outcome {
        Outcome {
            code,
            phrase: phrase.into(),
            ..Outcome::default()
        }
    }

    #[test]
    fn hang_ups_are_normal_ends() {
        let end: crate::teams::calling::types::CallEndPush =
            serde_json::from_str(include_str!("fixtures/call_end.json")).expect("reads");
        assert_eq!(call_end(&end.call_end), Ending::Normal);
        let conversation: ConversationEnd =
            serde_json::from_str(include_str!("fixtures/conversation_end.json")).expect("reads");
        assert_eq!(conversation_end(&conversation), Ending::Normal);
        // Our own hang-up while it rang.
        assert_eq!(
            call_end(&outcome(487, "CallEndReasonLocalUserInitiated")),
            Ending::Normal
        );
    }

    #[test]
    fn refusals_and_silence_are_failures() {
        assert_eq!(
            call_end(&outcome(603, "")),
            Ending::Failed(Failure::CallDeclined)
        );
        assert_eq!(
            call_end(&outcome(480, "")),
            Ending::Failed(Failure::CallNotAnswered)
        );
        assert_eq!(
            call_end(&outcome(500, "Internal")),
            Ending::Failed(Failure::CallFailed("Internal".into()))
        );
        assert_eq!(
            call_end(&outcome(410, "")),
            Ending::Failed(Failure::CallFailed("410".into()))
        );
    }

    #[test]
    fn acknowledgements() {
        assert_eq!(acknowledgement(&outcome(0, "Success")), Ok(()));
        assert_eq!(
            acknowledgement(&outcome(488, "Not acceptable")),
            Err(Failure::CallFailed("Not acceptable".into()))
        );
    }
}
