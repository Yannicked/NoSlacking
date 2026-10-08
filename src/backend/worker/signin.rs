//! Signing in and out: saving the Slack app, pasting a token, the
//! browser and OAuth flows, and forgetting a workspace.
//!
//! A second `impl Worker`, kept apart so the worker's loop stays
//! readable.

use serde_json::Value;

use super::{BROWSER_SIGN_IN_WINDOW, Internal, Worker};
use crate::auth::{self, Flow, SignedIn};
use crate::backend::api::failure;
use crate::backend::{Event, SignIn};
use crate::credentials::{AppCredentials, Credentials};
use crate::failure::{Doing, Failure, Problem};
#[cfg(feature = "teams")]
use crate::model::Workspace;
use crate::scopes::Request;
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
                sink.send(Event::KeyringError(error.into()));
            }
        });
        // Clients pick up the new client secret for refreshes. They keep
        // their token and refresh lock, which every clone shares, so a
        // refresh in flight cannot race a second one.
        let oauth = app.oauth();
        for (_, team) in self.slack_teams() {
            team.client.set_app(oauth.clone());
        }
        self.app = Some(app);
        self.restart_socket();
    }

    pub(super) fn cancel_sign_in(&mut self) {
        self.end_browser_sign_in();
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

    /// Opens Slack's sign-in page in the browser and starts
    /// accepting the link it hands back.
    pub(super) fn start_browser_sign_in(&mut self) {
        let started = std::time::Instant::now();
        self.browser_sign_in = Some(started);
        // The page ends with a slack:// link, so NoSlacking borrows those
        // now, and only now: the rest of the time they stay with the
        // official app. Before the browser opens, so the link cannot come
        // back first; off the runtime, since it runs xdg-mime or reg.exe.
        let sink = self.sink.clone();
        let state = self.dirs.state.clone();
        let previous = self.claiming.take();
        self.claiming = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let claimed = tokio::task::spawn_blocking(move || crate::slack_links::claim(&state))
                .await
                .unwrap_or_else(|error| Err(Failure::Unexpected(error.to_string())));
            if let Err(error) = claimed {
                // The link can still be pasted by hand.
                log::warn!("could not register as the slack:// link handler: {error:?}");
            }
            if let Err(error) = open::that_detached(crate::slack::magic::SIGN_IN_URL) {
                log::warn!("could not open the browser: {error}");
                sink.send(Event::SignIn(SignIn::Failed(Failure::NoBrowser)));
            }
        }));
        // The links go back when the wait is over, if nothing came first.
        let internal = self.internal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(BROWSER_SIGN_IN_WINDOW).await;
            let _ = internal.send(Internal::BrowserSignInOver(started));
        });
    }

    /// Stops waiting for the browser sign-in's link, if one was awaited,
    /// and gives the `slack://` links back to whatever had them before:
    /// the link came, was pasted, the sign-in was cancelled or it ran out
    /// of time. Nothing after that needs them.
    pub(super) fn end_browser_sign_in(&mut self) {
        if self.browser_sign_in.take().is_none() {
            return;
        }
        let state = self.dirs.state.clone();
        let claiming = self.claiming.take();
        tokio::spawn(async move {
            if let Some(claiming) = claiming {
                let _ = claiming.await;
            }
            let released = tokio::task::spawn_blocking(move || crate::slack_links::release(&state))
                .await
                .unwrap_or_else(|error| Err(Failure::Unexpected(error.to_string())));
            match released {
                Ok(true) => log::info!("gave the slack:// links back"),
                Ok(false) => {}
                Err(error) => log::warn!("could not give the slack:// links back: {error:?}"),
            }
        });
    }

    /// Whether a browser sign-in the user started is still waiting for its
    /// link.
    pub(super) fn browser_sign_in_pending(&self) -> bool {
        self.browser_sign_in
            .is_some_and(|started| started.elapsed() < BROWSER_SIGN_IN_WINDOW)
    }

    /// Signs in to every workspace a pasted `slack://` sign-in link names:
    /// redeems its tokens for the account's session cookie, then signs in to
    /// each team with that cookie.
    pub(super) fn sign_in_link(&mut self, link: &str) {
        let Some(sets) = crate::slack::magic::parse_link(link) else {
            self.sink
                .send(Event::SignIn(SignIn::Failed(Failure::NotASignInLink)));
            return;
        };
        // Handed over or pasted, the link is here: the sign-in finishes
        // without the browser, so the slack:// links can go back now.
        self.end_browser_sign_in();
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
                        problems.push(failure(&error));
                        continue;
                    }
                };
                for team in redeemed.teams {
                    match team {
                        TeamResult::SignedIn { url } => {
                            let Some(cookie) = redeemed.cookie.as_deref() else {
                                problems.push(Failure::NoSessionCookie);
                                continue;
                            };
                            let result = crate::slack::session::derive(cookie, &url)
                                .await
                                .map(|session| SignedIn {
                                    team_id: session.team_id,
                                    user_id: session.user_id,
                                    token: session.token,
                                    scopes: None,
                                })
                                .map_err(|e| failure(&e));
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
                            sink.send(Event::Notice(crate::notice::Notice::BrowserStep));
                        }
                        TeamResult::Failed { reason } => {
                            problems.push(failure(&crate::slack::SlackError::Api(reason)));
                        }
                    }
                }
            }
            if !problems.is_empty() {
                log::warn!("some workspaces did not sign in: {problems:?}");
            }
            if signed == 0 {
                // The first reason stands for them all: mostly there is
                // only the one workspace.
                let error = problems.into_iter().next().unwrap_or(Failure::NoWorkspace);
                let _ = internal.send(Internal::SignedIn(Err(error)));
            }
        });
    }

    pub(super) fn start_sign_in(&mut self, redirect: Redirect, port: u16, request: Request) {
        let Some(app) = self.app.clone().filter(AppCredentials::can_sign_in) else {
            self.sink
                .send(Event::SignIn(SignIn::Failed(Failure::NoClientId)));
            return;
        };
        let previous = self.listener.take();
        if let Some(previous) = &previous {
            previous.abort();
        }
        let flow = Flow::start(&app, redirect, port, request);
        match redirect {
            Redirect::Scheme => {
                if let Err(error) = auth::register_scheme() {
                    log::warn!("could not register noslacking:// links: {error:?}");
                    self.sink
                        .send(Event::Error(Problem::new(Doing::RegisterLinks, error)));
                }
                open_browser(&flow.url);
            }
            Redirect::Loopback => {
                let internal = self.internal.clone();
                let state = flow.state.clone();
                let url = flow.url.clone();
                self.listener = Some(tokio::spawn(async move {
                    // The last attempt's listener lets go of the port once
                    // its task is gone; only then can this one take it.
                    if let Some(previous) = previous {
                        let _ = previous.await;
                    }
                    // Listen first: with nowhere to land, the browser would
                    // be sent off for nothing, or to whoever holds the port.
                    let listened = match auth::bind_loopback(port) {
                        Ok(listeners) => {
                            open_browser(&url);
                            auth::loopback(listeners, &state).await
                        }
                        Err(error) => Err(error),
                    };
                    match listened {
                        Ok(url) => {
                            let _ = internal.send(Internal::Callback(url));
                        }
                        Err(error) => {
                            log::warn!("the sign-in listener on port {port} failed: {error}");
                            let _ =
                                internal.send(Internal::SignInListenerFailed(error.to_string()));
                        }
                    }
                }));
            }
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
                self.sign_in_link(&url);
            } else if let Some(link) = crate::links::parse_deep(&url).filter(|link| {
                link.team
                    .as_ref()
                    .is_some_and(|t| self.workspaces.contains_key(t))
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
            // Slack would not authorize the newer scopes: the app was made
            // from an older manifest. Ask again for what it has, once.
            Err(Failure::Refused(error))
                if let Some(older) = flow.request.after_refusal(&error) =>
            {
                log::info!("Slack refused the scopes ({error}); asking for the older set");
                self.flow = None;
                self.sink.send(Event::OlderApp);
                self.start_sign_in(flow.redirect, flow.port, older);
                return;
            }
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
                .map_err(|e| failure(&e));
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
        self.huddle_audio.signed_out(team);
        #[cfg(feature = "teams")]
        self.teams_call.signed_out(team);
        self.stop_rtm(team);
        self.people.forget(team);
        let removed = self.workspaces.remove(team);
        // Before SignedOut goes out: nothing from a task still running for
        // this workspace can follow it and bring the workspace back.
        if let Some(removed) = &removed {
            removed.shut();
        }
        match removed {
            Some(super::Backend::Slack(removed)) => {
                Self::forget_slack(&self.credentials, team, removed);
            }
            #[cfg(feature = "teams")]
            Some(super::Backend::Teams(removed)) => {
                removed.client.stop_reporting();
                let credentials = self.credentials.clone();
                let team = team.to_owned();
                tokio::spawn(async move {
                    if let Err(error) = credentials.delete_teams_token(&team).await {
                        log::warn!("could not delete the Teams token: {error}");
                    }
                });
            }
            None => {}
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
        if self.slack_teams().next().is_none() {
            self.restart_socket();
        }
        self.report_socket();
    }

    /// Deletes a Slack sign-in from the keyring, and revokes it when it is
    /// the app's own.
    fn forget_slack(credentials: &Credentials, team: &str, removed: super::Team) {
        {
            let credentials = credentials.clone();
            let team = team.to_owned();
            // A session token belongs to the browser login; revoking it would
            // sign the browser out too, so only OAuth tokens are revoked.
            let revoke = !removed.client.token().is_session();
            // A token renewed on the way to revoking must not be saved
            // again after it is deleted.
            removed.client.stop_reporting();
            tokio::spawn(async move {
                // Deleted first, with the token still in memory for the
                // revoke: the keyring does its jobs in order, so a quick
                // sign-in again saves after this and is kept, however long
                // Slack takes to answer the revoke.
                if let Err(error) = credentials.delete_token(&team).await {
                    log::warn!("could not delete the token: {error}");
                }
                if revoke && let Err(error) = removed.client.act::<Value>("auth.revoke", &[]).await
                {
                    log::info!("auth.revoke: {error}");
                }
            });
        }
    }

    /// Starts signing in to Microsoft Teams with a device code (see
    /// [`super::super::teams::sign_in`]); the answer comes back through
    /// `Internal::TeamsSignedIn`.
    #[cfg(feature = "teams")]
    pub(super) fn start_teams_sign_in(&mut self, tenant: Option<String>, personal: bool) {
        let account = if personal {
            crate::teams::auth::Account::Personal
        } else {
            crate::teams::auth::Account::Work
        };
        let sink = self.sink.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = super::super::teams::sign_in(account, tenant, sink).await;
            let _ = internal.send(Internal::TeamsSignedIn(result));
        });
    }

    /// Without Teams in the build there is nothing to sign in to.
    #[cfg(not(feature = "teams"))]
    pub(super) fn start_teams_sign_in(&mut self, _tenant: Option<String>, _personal: bool) {
        self.sink
            .send(Event::SignIn(SignIn::Failed(Failure::Unsupported)));
    }

    /// Saves and starts a Teams workspace once its sign-in went through.
    #[cfg(feature = "teams")]
    pub(super) fn teams_signed_in(
        &mut self,
        result: Result<(Workspace, crate::teams::auth::TeamsCredentials), Failure>,
    ) {
        let (workspace, creds) = match result {
            Ok(signed_in) => signed_in,
            Err(error) => {
                log::warn!("Teams sign-in failed: {error:?}");
                self.sink.send(Event::SignIn(SignIn::Failed(error)));
                return;
            }
        };
        let credentials = self.credentials.clone();
        let team = workspace.team_id.clone();
        let saved = creds.clone();
        tokio::spawn(async move {
            if let Err(error) = credentials.save_teams_token(&team, &saved).await {
                log::warn!("could not store the Teams token: {error}");
            }
        });
        let name = workspace.name.clone();
        self.add_teams(workspace, creds);
        self.sink.send(Event::SignIn(SignIn::Done(name)));
    }
}

/// Opens the sign-in page; the waiting screen shows its address to open by
/// hand when no browser comes up.
fn open_browser(url: &str) {
    if let Err(error) = open::that_detached(url) {
        log::warn!("could not open the browser: {error}");
    }
}

/// Checks a pasted token and finds out whose it is.
async fn validate(http: &reqwest::Client, token: Token) -> Result<SignedIn, Failure> {
    if !token.access.starts_with("xox") {
        return Err(Failure::NotAToken);
    }
    if token.access.starts_with("xoxb-") {
        return Err(Failure::BotToken);
    }
    let client = Client::new(http.clone(), token.clone());
    let test: types::AuthTest = client
        .call("auth.test", &[])
        .await
        .map_err(|e| failure(&e))?;
    Ok(SignedIn {
        team_id: test.team_id,
        user_id: test.user_id,
        token,
        // Slack's answer said which scopes the token has.
        scopes: client.scopes(),
    })
}
