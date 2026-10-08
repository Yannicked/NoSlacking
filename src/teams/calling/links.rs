//! The callback URLs of a call (§1.4): building the ones we hand the
//! server, and reading a push's path back into what it is for.
//!
//! Every link is `{surl}callAgent/{callAgentId}/{tag}/{scope}/{event}/`.
//! The call agent id picks the call and the last two segments say what
//! arrived, so the tags need not be remembered.

/// Which half of a call a callback belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The conversation: roster, conversation end.
    Conversation,
    /// The call: answer, acceptance, renegotiation, end.
    Call,
}

impl Scope {
    /// The scope as it stands in a link.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Call => "call",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "conversation" => Some(Self::Conversation),
            "call" => Some(Self::Call),
            _ => None,
        }
    }
}

/// A fresh callback tag: 8 lower-case hex digits, new for each link as
/// the web client makes them.
pub fn new_tag() -> String {
    use rand::Rng as _;
    format!("{:08x}", rand::rng().next_u32())
}

/// The link for `event` with `tag`. `updateMediaDescriptions` alone has
/// no trailing slash, as the web client sends it.
pub fn link(surl: &str, call_agent_id: &str, tag: &str, scope: Scope, event: &str) -> String {
    let slash = if event == "updateMediaDescriptions" {
        ""
    } else {
        "/"
    };
    format!(
        "{}/callAgent/{call_agent_id}/{tag}/{}/{event}{slash}",
        surl.trim_end_matches('/'),
        scope.as_str()
    )
}

/// The callbacks of one call: our Trouter base URL and the call's agent id.
#[derive(Clone, PartialEq, Eq)]
pub struct Callbacks {
    surl: String,
    call_agent_id: String,
}

// The links let anyone push into the call; they stay out of the log.
impl std::fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callbacks").finish_non_exhaustive()
    }
}

impl Callbacks {
    /// The callbacks under Trouter's `surl` for the call `call_agent_id`.
    pub fn new(surl: &str, call_agent_id: &str) -> Self {
        Self {
            surl: surl.to_owned(),
            call_agent_id: call_agent_id.to_owned(),
        }
    }

    /// A new link for `event`, with a fresh tag.
    pub fn link(&self, scope: Scope, event: &str) -> String {
        link(&self.surl, &self.call_agent_id, &new_tag(), scope, event)
    }

    /// New links for each of `events`, by name.
    pub fn links(
        &self,
        scope: Scope,
        events: &[&str],
    ) -> std::collections::BTreeMap<String, String> {
        events
            .iter()
            .map(|event| ((*event).to_owned(), self.link(scope, event)))
            .collect()
    }
}

/// What a push's path says: which call, and which callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushPath {
    pub call_agent_id: String,
    pub scope: Scope,
    pub event: String,
}

/// Reads a push's path, `/v4/f/{trouterId}/callAgent/{agent}/{tag}/{scope}/{event}/`
/// (or a whole link), into its call and callback.
pub fn read_push_path(path: &str) -> Option<PushPath> {
    let rest = path.split_once("/callAgent/")?.1;
    let rest = rest.split(['?', '#']).next()?;
    let mut parts = rest.split('/');
    let call_agent_id = parts.next().filter(|s| !s.is_empty())?;
    let _tag = parts.next()?;
    let scope = Scope::parse(parts.next()?)?;
    let event = parts.next().filter(|s| !s.is_empty())?;
    Some(PushPath {
        call_agent_id: call_agent_id.to_owned(),
        scope,
        event: event.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SURL: &str = "https://trouter.example:3443/v4/f/ID1/";
    const AGENT: &str = "00000000-0000-4000-8000-000000000001";

    #[test]
    fn links_have_the_recorded_shape() {
        assert_eq!(
            link(SURL, AGENT, "a000000c", Scope::Call, "mediaAnswer"),
            "https://trouter.example:3443/v4/f/ID1/callAgent/00000000-0000-4000-8000-000000000001/a000000c/call/mediaAnswer/"
        );
        assert_eq!(
            link(
                SURL.trim_end_matches('/'),
                AGENT,
                "a0000001",
                Scope::Conversation,
                "rosterUpdate"
            ),
            "https://trouter.example:3443/v4/f/ID1/callAgent/00000000-0000-4000-8000-000000000001/a0000001/conversation/rosterUpdate/"
        );
        assert!(
            link(
                SURL,
                AGENT,
                "a0000001",
                Scope::Call,
                "updateMediaDescriptions"
            )
            .ends_with("/call/updateMediaDescriptions")
        );
    }

    #[test]
    fn each_link_has_its_own_tag() {
        let callbacks = Callbacks::new(SURL, AGENT);
        let links = callbacks.links(Scope::Call, &["end", "acceptance"]);
        let tag = |l: &str| read_tag(l).to_owned();
        assert_ne!(tag(&links["end"]), tag(&links["acceptance"]));
        let one = tag(&links["end"]);
        assert_eq!(one.len(), 8);
        assert!(
            one.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(format!("{callbacks:?}"), "Callbacks { .. }");
    }

    fn read_tag(link: &str) -> &str {
        link.split("/callAgent/")
            .nth(1)
            .and_then(|r| r.split('/').nth(1))
            .unwrap_or_default()
    }

    #[test]
    fn a_push_path_reads_back() {
        let path = "/v4/f/ID1/callAgent/00000000-0000-4000-8000-000000000009/a0000016/conversation/rosterUpdate/";
        assert_eq!(
            read_push_path(path),
            Some(PushPath {
                call_agent_id: "00000000-0000-4000-8000-000000000009".into(),
                scope: Scope::Conversation,
                event: "rosterUpdate".into(),
            })
        );
        let built = link(
            SURL,
            AGENT,
            "a0000001",
            Scope::Call,
            "updateMediaDescriptions",
        );
        let read = read_push_path(&built).expect("reads");
        assert_eq!(
            (read.scope, read.event.as_str()),
            (Scope::Call, "updateMediaDescriptions")
        );
        assert_eq!(read.call_agent_id, AGENT);
    }

    #[test]
    fn other_paths_are_not_pushes_to_a_call() {
        assert_eq!(read_push_path("/v4/f/ID1/"), None);
        assert_eq!(read_push_path("/v4/f/ID1/messaging"), None);
        assert_eq!(
            read_push_path("/v4/f/ID1/callAgent/x/tag/elsewhere/end/"),
            None
        );
        assert_eq!(read_push_path("/v4/f/ID1/callAgent/x/tag/call/"), None);
    }
}
