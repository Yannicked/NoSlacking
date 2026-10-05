//! Signing in and out: saving the Slack app, pasting a token, the
//! browser and OAuth flows, and forgetting a workspace.
//!
//! A second `impl Worker`, kept apart so the worker's loop stays
//! readable.

use serde_json::Value;

use super::{BROWSER_SIGN_IN_WINDOW, Internal, Worker};
use crate::auth::{self, Flow, SignedIn};
use crate::backend::api::describe;
use crate::backend::{Event, SignIn};
use crate::credentials::AppCredentials;
use crate::settings::Redirect;
use crate::slack::magic::TeamResult;
use crate::slack::{Client, Token, types};

impl Worker {
    pub(super) fn save_app(&mut self, app: AppCredentials) {
        // Saved in the background; the new app is used at once.
        let credentials = self.credentials.clone();
        let sink = self.sink.clone();
        let saved = app.clone();
        tokio::spawn(async move {
            if let Err(error) = credentials.save_app(&saved).await {
                sink.send(Event::KeyringError(error.to_string()));
            }
        });
        // Clients pick up the new client secret for refreshes. They keep
        // their token and refresh lock, which every clone shares, so a
        // refresh in flight cannot race a second one.
        let oauth = app.oauth();
        for team in self.teams.values() {
            team.client.set_app(oauth.clone());
        }
        self.app = Some(app);
        self.restart_socket();
    }

    pub(super) fn cancel_sign_in(&mut self) {
        self.flow = None;
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
    }

    pub(super) fn paste_token(&mut self, token: String) {
        let http = crate::slack::net::api();
        let internal = self.internal.clone();
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        tokio::spawn(async move {
            let result = validate(&http, Token::plain(token.trim())).await;
            let _ = internal.send(Internal::SignedIn(result));
        });
    }

    pub(super) fn sign_in_session(&mut self, cookie: String, workspace_url: &str) {
        let Some(workspace_url) = crate::slack::session::normalize_workspace(workspace_url) else {
            self.sink.send(Event::SignIn(SignIn::Failed(
                "Enter your workspace's Slack address, such as acme.slack.com.".into(),
            )));
            return;
        };
        let internal = self.internal.clone();
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        tokio::spawn(async move {
            let result = crate::slack::session::derive(cookie.trim(), &workspace_url)
                .await
                .map(|signed| SignedIn {
                    team_id: signed.team_id,
                    user_id: signed.user_id,
                    token: signed.token,
                })
                .map_err(|e| describe(&e));
            let _ = internal.send(Internal::SignedIn(result));
        });
    }

    /// Opens Slack's sign-in page in the browser and starts
    /// accepting the link it hands back.
    pub(super) fn start_browser_sign_in(&mut self) {
        self.browser_sign_in = Some(std::time::Instant::now());
        if let Err(error) = open::that_detached(crate::slack::magic::SIGN_IN_URL) {
            log::warn!("could not open the browser: {error}");
            self.sink.send(Event::SignIn(SignIn::Failed(
                "Could not open the browser. Open app.slack.com/ssb/signin yourself.".into(),
            )));
        }
    }

    /// Whether a browser sign-in the user started is still waiting for its
    /// link.
    pub(super) fn browser_sign_in_pending(&self) -> bool {
        self.browser_sign_in
            .is_some_and(|started| started.elapsed() < BROWSER_SIGN_IN_WINDOW)
    }

    /// Signs in to every workspace a pasted `slack://` sign-in link names:
    /// redeems its tokens for the account's session cookie, then signs in to
    /// each team with that cookie like [`Self::sign_in_session`].
    pub(super) fn sign_in_link(&mut self, link: &str) {
        let Some(sets) = crate::slack::magic::parse_link(link) else {
            self.sink.send(Event::SignIn(SignIn::Failed(
                "That is not a Slack sign-in link. Copy the slack:// link the browser offers to open."
                    .into(),
            )));
            return;
        };
        let internal = self.internal.clone();
        let sink = self.sink.clone();
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        tokio::spawn(async move {
            let mut signed = 0;
            let mut problems = Vec::new();
            for set in sets {
                let redeemed = match crate::slack::magic::redeem(&set).await {
                    Ok(redeemed) => redeemed,
                    Err(error) => {
                        problems.push(describe(&error));
                        continue;
                    }
                };
                for team in redeemed.teams {
                    match team {
                        TeamResult::SignedIn { url } => {
                            let Some(cookie) = redeemed.cookie.as_deref() else {
                                problems.push("Slack signed in but set no session cookie".into());
                                continue;
                            };
                            let result = crate::slack::session::derive(cookie, &url)
                                .await
                                .map(|session| SignedIn {
                                    team_id: session.team_id,
                                    user_id: session.user_id,
                                    token: session.token,
                                })
                                .map_err(|e| describe(&e));
                            match result {
                                Ok(session) => {
                                    signed += 1;
                                    let _ = internal.send(Internal::SignedIn(Ok(session)));
                                }
                                Err(error) => problems.push(error),
                            }
                        }
                        // SSO and similar: the workspace wants the browser
                        // again; its page then offers a new link to paste.
                        TeamResult::Browser { url } => {
                            if crate::mrkdwn::is_openable(&url)
                                && let Err(error) = open::that_detached(&url)
                            {
                                log::warn!("could not open the browser: {error}");
                            }
                            sink.send(Event::Notice(
                                "A workspace needs one more step in the browser; paste the new link it offers."
                                    .into(),
                            ));
                        }
                        TeamResult::Failed { reason } => problems.push(reason),
                    }
                }
            }
            if signed == 0 {
                let error = if problems.is_empty() {
                    "Slack signed in to no workspace with that link.".to_owned()
                } else {
                    format!("Could not sign in: {}", problems.join("; "))
                };
                let _ = internal.send(Internal::SignedIn(Err(error)));
            } else if !problems.is_empty() {
                log::warn!("some workspaces did not sign in: {}", problems.join("; "));
            }
        });
    }

    pub(super) fn start_sign_in(&mut self, redirect: Redirect, port: u16) {
        let Some(app) = self.app.clone().filter(AppCredentials::can_sign_in) else {
            self.sink.send(Event::SignIn(SignIn::Failed(
                "Enter the Slack app's client ID first.".into(),
            )));
            return;
        };
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
        let flow = Flow::start(&app, redirect, port);
        match redirect {
            Redirect::Scheme => {
                if let Err(error) = auth::register_scheme() {
                    log::warn!("could not register noslacking:// links: {error}");
                    self.sink.send(Event::Error(format!(
                        "Could not register noslacking:// links ({error}). Try the loopback redirect in Settings."
                    )));
                }
            }
            Redirect::Loopback => {
                let internal = self.internal.clone();
                let state = flow.state.clone();
                self.listener = Some(tokio::spawn(async move {
                    match auth::loopback(port, &state).await {
                        Ok(url) => {
                            let _ = internal.send(Internal::Callback(url));
                        }
                        Err(error) => {
                            let _ =
                                internal.send(Internal::SignInListenerFailed(error.to_string()));
                        }
                    }
                }));
            }
        }
        if let Err(error) = open::that_detached(&flow.url) {
            log::warn!("could not open the browser: {error}");
        }
        self.sink
            .send(Event::SignIn(SignIn::Waiting(flow.url.clone())));
        self.flow = Some(flow);
    }

    pub(super) fn callback(&mut self, url: String) {
        if url.starts_with("slack:") {
            // Only the link of a sign-in the user started here counts; any
            // other slack:// link handed over is not a sign-in for us.
            if self.browser_sign_in_pending() && crate::slack::magic::parse_link(&url).is_some() {
                self.browser_sign_in = None;
                self.sign_in_link(&url);
            } else if let Some(link) = crate::links::parse_deep(&url).filter(|link| {
                link.team
                    .as_ref()
                    .is_some_and(|t| self.teams.contains_key(t))
            }) {
                // A link to a conversation of a workspace signed in here.
                self.sink.send(Event::DeepLink(link));
            } else {
                log::info!("ignoring a slack:// link with no browser sign-in in progress");
            }
            return;
        }
        let Some(flow) = self.flow.clone() else {
            log::info!("ignoring a sign-in link with no sign-in in progress");
            return;
        };
        let code = match auth::parse_callback(&url, &flow.state) {
            Ok(code) => code,
            Err(error) => {
                self.sink.send(Event::SignIn(SignIn::Failed(error)));
                return;
            }
        };
        self.flow = None;
        let Some(app) = self.app.clone() else {
            return;
        };
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        let http = crate::slack::net::api();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = auth::exchange(&http, &app, &flow, &code)
                .await
                .map_err(|e| describe(&e));
            let _ = internal.send(Internal::SignedIn(result));
        });
    }

    /// Removes the unencrypted lists older builds kept for `team`.
    pub(super) fn remove_plain_cache(&self, team: &str) {
        for path in [
            self.dirs.users_cache(team),
            self.dirs.conversations_cache(team),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("removed the unencrypted {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("could not remove {}: {error}", path.display()),
            }
        }
    }

    pub(super) fn sign_out(&mut self, team: &str) {
        if let Some(live) = self.rtm.remove(team) {
            let _ = live.stop.send(true);
        }
        self.people.forget(team);
        if let Some(removed) = self.teams.remove(team) {
            // Before SignedOut goes out: nothing from a task still running
            // for this workspace can follow it and bring the workspace back.
            removed.shut();
            let credentials = self.credentials.clone();
            let team = team.to_owned();
            // A session token belongs to the browser login; revoking it would
            // sign the browser out too, so only OAuth tokens are revoked.
            let revoke = !removed.client.token().is_session();
            tokio::spawn(async move {
                if revoke && let Err(error) = removed.client.act::<Value>("auth.revoke", &[]).await
                {
                    log::info!("auth.revoke: {error}");
                }
                if let Err(error) = credentials.delete_token(&team).await {
                    log::warn!("could not delete the token: {error}");
                }
            });
        }
        self.images.remove_client(team);
        // Nothing read in the workspace stays on disk after signing out.
        self.cache.wipe(team);
        self.remove_plain_cache(team);
        // A later sign-in to the same workspace fetches everyone afresh.
        self.users_requested.retain(|(t, _)| t != team);
        self.bots_requested.retain(|(t, _)| t != team);
        self.sink.send(Event::SignedOut {
            team: team.to_owned(),
            reason: None,
        });
        if self.teams.is_empty() {
            self.restart_socket();
        }
        self.report_socket();
    }
}

/// Checks a pasted token and finds out whose it is.
async fn validate(http: &reqwest::Client, token: Token) -> Result<SignedIn, String> {
    if !token.access.starts_with("xox") {
        return Err("That does not look like a Slack token (it should start with xoxp-).".into());
    }
    if token.access.starts_with("xoxb-") {
        return Err("That is a bot token. NoSlacking needs the User OAuth Token (xoxp-).".into());
    }
    let client = Client::new(http.clone(), token.clone());
    let test: types::AuthTest = client
        .call("auth.test", &[])
        .await
        .map_err(|e| describe(&e))?;
    Ok(SignedIn {
        team_id: test.team_id,
        user_id: test.user_id,
        token,
    })
}
