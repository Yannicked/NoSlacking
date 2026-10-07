//! Where call pushes go: the live connection hands every push for a call
//! (`…/callAgent/{agent}/…`) to the call that made that callback, an
//! incoming call's notification to the ringer, and tells calls the
//! address their callbacks must name.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize as _;
use tokio::sync::mpsc;

use super::call::Incoming;
use super::links::read_push_path;
use super::types::{IncomingNotification, Push};

/// Who hears of an incoming call: the worker, which rings.
pub type Ringer = Arc<dyn Fn(Incoming) + Send + Sync>;

/// The open calls of one account and its live connection's address.
/// Cheap to clone; every clone is the same router.
#[derive(Clone, Debug, Default)]
pub struct Router {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// The live connection's address (`surl`), while it is connected.
    surl: Option<String>,
    /// Each open call's inbox, by its call agent id.
    calls: HashMap<String, mpsc::UnboundedSender<Push>>,
    /// Who hears of incoming calls, once set.
    ringer: Option<Ringer>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("connected", &self.surl.is_some())
            .field("calls", &self.calls.len())
            .field("ringer", &self.ringer.is_some())
            .finish()
    }
}

impl Router {
    /// The live connection connected at `surl` (or, `None`, dropped).
    pub fn set_surl(&self, surl: Option<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.surl = surl;
        }
    }

    /// The address callbacks must name, while connected.
    pub fn surl(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|inner| inner.surl.clone())
    }

    /// Who hears of incoming calls from now on.
    pub fn set_ringer(&self, ringer: Ringer) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.ringer = Some(ringer);
        }
    }

    /// Hands an incoming call's notification (`gp` decoded) to the
    /// ringer. Answers whether it was a call that rang.
    pub fn ring(&self, body: &serde_json::Value) -> bool {
        let call = match IncomingNotification::deserialize(body) {
            Ok(note) => Incoming::read(&note),
            Err(error) => {
                log::warn!("could not read a call notification: {error}");
                None
            }
        };
        let Some(call) = call else {
            log::info!("a call notification that is not a 1:1 call; not rung");
            return false;
        };
        let ringer = self
            .inner
            .lock()
            .ok()
            .and_then(|inner| inner.ringer.clone());
        match ringer {
            Some(ringer) => {
                ringer(call);
                true
            }
            None => false,
        }
    }

    /// Opens an inbox for the call with `call_agent_id`.
    pub fn register(&self, call_agent_id: &str) -> mpsc::UnboundedReceiver<Push> {
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Ok(mut inner) = self.inner.lock() {
            inner.calls.insert(call_agent_id.to_owned(), sender);
        }
        receiver
    }

    /// Closes a call's inbox: later pushes for it are dropped.
    pub fn forget(&self, call_agent_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.calls.remove(call_agent_id);
        }
    }

    /// Hands a push to its call. Answers whether a call took it; a push
    /// for a call that ended, or one that does not read, is logged by its
    /// event name only (its body and path hold ids and links).
    pub fn deliver(&self, path: &str, body: &serde_json::Value) -> bool {
        let Some(pushed) = read_push_path(path) else {
            log::debug!("a call push with an unreadable path");
            return false;
        };
        let push = match Push::read(&pushed.event, body) {
            Ok(push) => push,
            Err(error) => {
                log::warn!("could not read the call push {}: {error}", pushed.event);
                return false;
            }
        };
        let inbox = self
            .inner
            .lock()
            .ok()
            .and_then(|inner| inner.calls.get(&pushed.call_agent_id).cloned());
        match inbox {
            Some(inbox) => inbox.send(push).is_ok(),
            None => {
                log::debug!("a call push ({}) for no open call", pushed.event);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_reach_the_call_that_made_the_callback() {
        let router = Router::default();
        let mut inbox = router.register("agent-1");
        let ended = serde_json::json!({
            "callEnd": {"reason": "noError", "code": 0, "subCode": 0, "phrase": "LocalUserInitiated"}
        });
        assert!(router.deliver("/v4/f/T1/callAgent/agent-1/ab12cd34/call/end/", &ended));
        assert!(matches!(inbox.try_recv(), Ok(Push::CallEnd(_))));
        // Another call's push, and a forgotten call's, go nowhere.
        assert!(!router.deliver("/v4/f/T1/callAgent/agent-2/ab12cd34/call/end/", &ended));
        router.forget("agent-1");
        assert!(!router.deliver("/v4/f/T1/callAgent/agent-1/ab12cd34/call/end/", &ended));
    }

    #[test]
    fn the_address_follows_the_connection() {
        let router = Router::default();
        assert_eq!(router.surl(), None);
        router.set_surl(Some("https://t.example/v4/f/T1/".into()));
        assert_eq!(router.surl().as_deref(), Some("https://t.example/v4/f/T1/"));
        router.set_surl(None);
        assert_eq!(router.surl(), None);
    }
}
