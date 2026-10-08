//! Which Slack permissions (scopes) a sign-in has, and what each feature
//! needs.
//!
//! A browser session may do anything Slack's web client does. A sign-in
//! through your own Slack app has the user scopes Slack granted it, which
//! `oauth.v2.access` lists in `authed_user.scope` and every Web API answer
//! repeats in its `x-oauth-scopes` header. Apps made from the first
//! manifest lack the scopes [`NEWER`] names, so features that need them
//! are offered only when the sign-in has them, rather than tried and
//! taken back on Slack's `missing_scope`.

use std::collections::BTreeSet;

/// The version of `slack-app-manifest.json`, which its description shows
/// as "manifest v2" so you can tell which one your app was made from.
/// Raise it, and the description, when the manifest asks for more.
pub const MANIFEST_VERSION: u32 = 2;

/// The user scopes manifest version 2 added. An app made from version 1
/// lacks them all.
pub const NEWER: [&str; 4] = [
    "dnd:read",
    "dnd:write",
    "usergroups:read",
    "bookmarks:write",
];

/// What NoSlacking can do only with one of the [`NEWER`] scopes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// Reading your Do Not Disturb state from Slack (`dnd.info` and the
    /// `dnd_updated` event).
    ReadDnd,
    /// Snoozing notifications in Slack too, not only here.
    SetDnd,
    /// `@group` suggestions in the composer (`usergroups.list`).
    GroupMentions,
    /// Adding, editing and removing a channel's bookmarks.
    EditBookmarks,
}

impl Feature {
    /// Every feature, in the order the settings note names them.
    pub const ALL: [Self; 4] = [
        Self::ReadDnd,
        Self::SetDnd,
        Self::GroupMentions,
        Self::EditBookmarks,
    ];

    /// The scope Slack wants for it.
    pub fn scope(self) -> &'static str {
        match self {
            Self::ReadDnd => "dnd:read",
            Self::SetDnd => "dnd:write",
            Self::GroupMentions => "usergroups:read",
            Self::EditBookmarks => "bookmarks:write",
        }
    }
}

/// The scopes Slack granted a token.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Scopes(BTreeSet<String>);

impl Scopes {
    /// Reads Slack's comma-separated list, as `authed_user.scope` and the
    /// `x-oauth-scopes` header give it. Spaces around names are dropped.
    pub fn parse(list: &str) -> Self {
        Self(
            list.split(',')
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Whether `scope` was granted.
    pub fn has(&self, scope: &str) -> bool {
        self.0.contains(scope)
    }

    /// The features these scopes leave out.
    pub fn lacking(&self) -> Vec<Feature> {
        Feature::ALL
            .into_iter()
            .filter(|feature| !self.has(feature.scope()))
            .collect()
    }

    /// Whether these are empty, which an answer without a list gives.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Whether a sign-in may use `scope`. A browser session may do anything;
/// an app sign-in whose scopes are not known yet (one from before they
/// were recorded) tries, and Slack's `missing_scope` stops it as before.
pub fn allows(scopes: Option<&Scopes>, session: bool, scope: &str) -> bool {
    session || scopes.is_none_or(|scopes| scopes.has(scope))
}

/// Which user scopes the sign-in page asks Slack for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Request {
    /// Everything [`crate::auth::USER_SCOPES`] lists.
    #[default]
    Full,
    /// Everything but the [`NEWER`] scopes, for an app made from manifest
    /// version 1 that Slack would not authorize with them.
    Older,
}

impl Request {
    /// What to ask for, given whether the app is known to be an older one.
    pub fn for_app(older_app: bool) -> Self {
        if older_app { Self::Older } else { Self::Full }
    }

    /// The scopes to put in the authorize URL's `user_scope`.
    pub fn scopes(self) -> Vec<&'static str> {
        crate::auth::USER_SCOPES
            .iter()
            .copied()
            .filter(|scope| self == Self::Full || !NEWER.contains(scope))
            .collect()
    }

    /// What to ask for next after Slack refused the sign-in with `error`:
    /// the older set, when the full one was refused for its scopes.
    /// Slack's documented refusals for scopes are `invalid_scope` and
    /// (for an app on the Marketplace) `unapproved_scope`.
    pub fn after_refusal(self, error: &str) -> Option<Self> {
        let about_scopes = matches!(error, "invalid_scope" | "unapproved_scope");
        (self == Self::Full && about_scopes).then_some(Self::Older)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from the `oauth.v2.access` answer Slack documents, with the
    /// user scopes an app made from manifest version 1 has.
    const OLDER_GRANT: &str = r#"{"ok":true,"app_id":"A0KRD7HC3",
        "authed_user":{"id":"U1234","scope":"channels:history,channels:read,channels:write,groups:history,groups:read,groups:write,im:history,im:read,im:write,mpim:history,mpim:read,mpim:write,chat:write,reactions:write,stars:read,stars:write,users:read,files:read,files:write,emoji:read,team:read,search:read,pins:read,pins:write,bookmarks:read,users.profile:write,users:write,reminders:read,reminders:write",
            "access_token":"xoxe.xoxp-1-a","token_type":"user","refresh_token":"xoxe-1-b","expires_in":43200},
        "team":{"id":"T9TK3CUKW","name":"Slack Pickleball Team"}}"#;

    #[test]
    fn granted_scopes_come_from_the_oauth_answer() {
        let access: crate::slack::types::OauthAccess =
            serde_json::from_str(OLDER_GRANT).expect("parses");
        let granted = Scopes::parse(&access.authed_user.scope);
        assert!(granted.has("channels:history") && granted.has("reminders:write"));
        assert!(!granted.has("dnd:read"));
        assert_eq!(granted.lacking(), Feature::ALL.to_vec());
        // The header lists them the same way, sometimes with spaces.
        let header = Scopes::parse("identify, dnd:read ,dnd:write,,usergroups:read");
        assert!(header.has("dnd:read") && header.has("identify"));
        assert_eq!(header.lacking(), vec![Feature::EditBookmarks]);
        assert!(Scopes::parse("").is_empty());
    }

    #[test]
    fn features_follow_the_granted_scopes() {
        let older = Scopes::parse("channels:read,bookmarks:read");
        let newer = Scopes::parse(&NEWER.join(","));
        for feature in Feature::ALL {
            let scope = feature.scope();
            assert!(!allows(Some(&older), false, scope), "{feature:?}");
            assert!(allows(Some(&newer), false, scope), "{feature:?}");
            // Sessions may do anything, and unknown grants still try.
            assert!(allows(Some(&older), true, scope));
            assert!(allows(None, false, scope));
        }
        let some = Scopes::parse("dnd:read,usergroups:read");
        assert_eq!(
            some.lacking(),
            vec![Feature::SetDnd, Feature::EditBookmarks]
        );
    }

    #[test]
    fn an_older_app_is_asked_for_what_it_has() {
        let full = Request::for_app(false);
        assert_eq!(full, Request::Full);
        assert_eq!(full.scopes(), crate::auth::USER_SCOPES);
        let older = Request::for_app(true).scopes();
        for scope in NEWER {
            assert!(full.scopes().contains(&scope), "{scope}");
            assert!(!older.contains(&scope), "{scope}");
        }
        assert_eq!(older.len(), crate::auth::USER_SCOPES.len() - NEWER.len());
        // A refusal for the scopes falls back once; anything else does not.
        assert_eq!(full.after_refusal("invalid_scope"), Some(Request::Older));
        assert_eq!(full.after_refusal("unapproved_scope"), Some(Request::Older));
        assert_eq!(full.after_refusal("access_denied"), None);
        assert_eq!(Request::Older.after_refusal("invalid_scope"), None);
    }

    #[test]
    fn a_workspace_names_what_its_app_lacks() {
        use crate::model::{SignInKind, Workspace};
        let older = Scopes::parse(&Request::Older.scopes().join(","));
        let workspace = |sign_in, scopes| Workspace {
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            sign_in,
            scopes,
        };
        let app = workspace(SignInKind::App, Some(older.clone()));
        assert_eq!(app.lacking(), Feature::ALL.to_vec());
        assert!(!app.can(Feature::EditBookmarks));
        // A session, or an app sign-in whose grant is not known, lacks
        // nothing it is told about.
        let session = workspace(SignInKind::Session, Some(older));
        assert!(session.lacking().is_empty() && session.can(Feature::SetDnd));
        let unknown = workspace(SignInKind::App, None);
        assert!(unknown.lacking().is_empty() && unknown.can(Feature::GroupMentions));
        let full = workspace(
            SignInKind::App,
            Some(Scopes::parse(&Request::Full.scopes().join(","))),
        );
        assert!(full.lacking().is_empty());
    }
}
