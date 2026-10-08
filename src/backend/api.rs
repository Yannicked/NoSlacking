//! What every Web API call shares: what its failures mean, calls whose
//! refusals can mean "already done", walking a listing page by page, and
//! reading a conversation's history.

use std::collections::HashSet;

use serde_json::Value;

use crate::failure::Failure;
use crate::model::Ts;
use crate::slack::session::Refusal;
use crate::slack::{Client, SlackError, types};

/// What an API failure means for the interface, which words it.
pub(super) fn failure(error: &SlackError) -> Failure {
    if error.is_auth() {
        return Failure::SignedOut;
    }
    match error {
        SlackError::Api(code) => match code.as_str() {
            "missing_scope" => Failure::MissingPermission,
            "channel_not_found" => Failure::ConversationGone,
            "not_in_channel" => Failure::NotInChannel,
            "is_archived" => Failure::Archived,
            "msg_too_long" => Failure::TooLong,
            "cant_update_message" | "edit_window_closed" => Failure::CantEdit,
            "cant_delete_message" => Failure::CantDelete,
            "cant_delete_file" => Failure::CantDeleteFile,
            // `emoji.add`, as the web client calls it.
            "error_name_taken" | "error_name_taken_i18n" => Failure::EmojiNameTaken,
            "error_bad_name_i18n" | "error_lower_case_names_only" | "error_missing_name" => {
                Failure::InvalidName
            }
            "error_too_big" | "resized_but_still_too_large" | "too_many_frames" => {
                Failure::EmojiTooBig
            }
            "error_bad_upload" | "error_bad_format" | "error_bad_wide" | "error_no_image"
            | "no_image_uploaded" => Failure::BadEmojiImage,
            // Slack's own refusal of the sign-in's kind: an OAuth sign-in
            // made without what the method needs (search, say). A call only
            // a browser session may make is refused before Slack is asked,
            // as `SlackError::NeedsSession`.
            "not_allowed_token_type" => Failure::MissingPermission,
            "no_permission" => Failure::Restricted,
            "invalid_code" | "code_already_used" => Failure::LinkExpired,
            "bad_redirect_uri" => Failure::BadRedirect,
            "invalid_client_id" | "bad_client_secret" => Failure::BadClient,
            // Creating, renaming and leaving channels.
            "name_taken" => Failure::NameTaken,
            "invalid_name" | "invalid_name_specials" | "invalid_name_punctuation" => {
                Failure::InvalidName
            }
            "invalid_name_maxlength" => Failure::NameTooLong,
            "cant_leave_general" => Failure::CantLeaveGeneral,
            "restricted_action" | "restricted_action_read_only_channel" => Failure::Restricted,
            "method_not_supported_for_channel_type" => Failure::WrongKind,
            other => Failure::Slack(other.to_owned()),
        },
        SlackError::RateLimited => Failure::RateLimited,
        SlackError::Http(status) => Failure::Http(*status),
        SlackError::Network(detail) => Failure::Network(detail.clone()),
        SlackError::Decode(detail) => Failure::Unexpected(detail.clone()),
        SlackError::NoUserToken => Failure::NoUserToken,
        SlackError::NeedsSession => Failure::NeedsSession,
        SlackError::Session(refusal) => match refusal {
            Refusal::NotACookie => Failure::NoSessionCookie,
            Refusal::NotSlack => Failure::NotSlackAddress,
            Refusal::NoToken => Failure::NoSessionToken,
            Refusal::CookieRefused => Failure::CookieRefused,
        },
    }
}

/// The outcome of a call made for its effect, where Slack refusing it
/// with one of `done` means it is already as asked (a reaction already
/// there, a file already gone), which is no failure.
pub(super) fn done_if<T>(result: Result<T, SlackError>, done: &[&str]) -> Result<(), SlackError> {
    match result {
        Err(error) if error.is_code(done) => Ok(()),
        other => other.map(|_| ()),
    }
}

/// A Web API call made for its effect: its method, its parameters, and
/// the error codes that mean it is already done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Call {
    pub method: &'static str,
    pub params: Vec<(&'static str, String)>,
    pub done: &'static [&'static str],
}

impl Call {
    /// A call that the `done` refusals count as done.
    pub fn new(
        method: &'static str,
        params: Vec<(&'static str, String)>,
        done: &'static [&'static str],
    ) -> Self {
        Self {
            method,
            params,
            done,
        }
    }

    /// Makes the call (see [`act_with_blocks`]); a `done` refusal is
    /// success.
    pub async fn run(self, client: &Client) -> Result<(), SlackError> {
        done_if(
            act_with_blocks::<Value>(client, self.method, &self.params).await,
            self.done,
        )
    }
}

/// Adds a message's text as Slack's own composer sends it: the mrkdwn
/// `text`, which notifications and older clients show, and the same
/// message as a `rich_text` block, which Slack draws. When no block can be
/// made (see [`crate::slack::rich_out`]), the text goes alone.
pub(super) fn with_text(params: &mut Vec<(&'static str, String)>, text: String) {
    let blocks = crate::slack::rich_out::blocks_param(&text);
    params.push(("text", text));
    if let Some(blocks) = blocks {
        params.push(("blocks", blocks));
    }
}

/// Slack's answers when it will not take a message's `blocks`.
const BLOCKS_REFUSED: [&str; 3] = [
    "invalid_blocks",
    "invalid_blocks_format",
    "msg_blocks_too_long",
];

/// Makes a call that may carry `blocks`; should Slack refuse them, the
/// same call goes again with the text alone, so a message is never lost
/// to its layout.
pub(super) async fn act_with_blocks<T: serde::de::DeserializeOwned>(
    client: &Client,
    method: &str,
    params: &[(&'static str, String)],
) -> Result<T, SlackError> {
    match client.act::<T>(method, params).await {
        Err(error)
            if error.is_code(&BLOCKS_REFUSED)
                && params.iter().any(|(name, _)| *name == "blocks") =>
        {
            log::warn!("Slack refused a message's blocks ({error}); sending its text alone");
            client.act(method, &without_blocks(params)).await
        }
        other => other,
    }
}

/// The same parameters without `blocks`.
fn without_blocks(params: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    params
        .iter()
        .filter(|(name, _)| *name != "blocks")
        .cloned()
        .collect()
}

/// A `conversations.history` request. Each reader asks a little
/// differently; this keeps the parameters in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryQuery {
    channel: String,
    limit: u32,
    /// Sends `include_all_metadata=false`, as the readers of whole pages
    /// do.
    no_metadata: bool,
    /// The newest message to read, itself included.
    latest: Option<Ts>,
    /// The message to read after, and what to tell Slack about including
    /// it, if anything.
    oldest: Option<(Ts, Option<bool>)>,
    cursor: Option<String>,
}

impl HistoryQuery {
    /// Up to `limit` of `channel`'s newest messages.
    pub fn new(channel: &str, limit: u32) -> Self {
        Self {
            channel: channel.to_owned(),
            limit,
            no_metadata: false,
            latest: None,
            oldest: None,
            cursor: None,
        }
    }

    /// Leaves out the messages' metadata.
    pub fn without_metadata(mut self) -> Self {
        self.no_metadata = true;
        self
    }

    /// Reads back from `ts`, which is included.
    pub fn up_to(mut self, ts: &Ts) -> Self {
        self.latest = Some(ts.clone());
        self
    }

    /// Reads on from `ts`. `inclusive` is sent when given; left out,
    /// Slack's default leaves `ts` itself out.
    pub fn after(mut self, ts: &Ts, inclusive: Option<bool>) -> Self {
        self.oldest = Some((ts.clone(), inclusive));
        self
    }

    /// Reads the page at `cursor`, if any.
    pub fn at(mut self, cursor: Option<String>) -> Self {
        self.cursor = cursor;
        self
    }

    /// The form parameters.
    pub fn params(&self) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("channel", self.channel.clone()),
            ("limit", self.limit.to_string()),
        ];
        if self.no_metadata {
            params.push(("include_all_metadata", "false".to_owned()));
        }
        if let Some(latest) = &self.latest {
            params.push(("latest", latest.0.clone()));
            params.push(("inclusive", "true".to_owned()));
        }
        if let Some((oldest, inclusive)) = &self.oldest {
            params.push(("oldest", oldest.0.clone()));
            if let Some(inclusive) = inclusive {
                params.push(("inclusive", inclusive.to_string()));
            }
        }
        with_cursor(params, self.cursor.clone())
    }

    /// Asks Slack for the page.
    pub async fn page<T: serde::de::DeserializeOwned>(
        &self,
        client: &Client,
    ) -> Result<T, SlackError> {
        client.call("conversations.history", &self.params()).await
    }
}

/// One page of your `users.conversations` of `kinds`, for [`paginate`].
pub(super) async fn my_conversations_page(
    client: &Client,
    kinds: &str,
    cursor: Option<String>,
) -> Result<(Vec<types::Channel>, Option<String>), SlackError> {
    let params = with_cursor(
        vec![
            ("types", kinds.to_owned()),
            ("exclude_archived", "true".to_owned()),
            ("limit", "200".to_owned()),
        ],
        cursor,
    );
    let page: types::ConversationsPage = client.call("users.conversations", &params).await?;
    let next = page.response_metadata.cursor();
    Ok((page.channels, next))
}

/// Walks a cursor-paged listing.
///
/// `page` fetches the page at a cursor (none for the first) and answers its
/// items and the next cursor; `each` takes every page's items as they come,
/// so a caller can show them early and keeps what arrived before a failure,
/// and answers whether to go on. The walk ends at an empty cursor, at a
/// cursor Slack already gave (some undocumented listings answer every page
/// with the same one), when `each` says stop, at the first error, or after
/// `max_pages`, which is logged: a listing cut short looks complete
/// otherwise.
pub(super) async fn paginate<T, Fut>(
    what: &str,
    max_pages: usize,
    page: impl FnMut(Option<String>) -> Fut,
    mut each: impl FnMut(Vec<T>) -> bool,
) -> Result<(), SlackError>
where
    Fut: std::future::Future<Output = Result<(Vec<T>, Option<String>), SlackError>>,
{
    walk(what, max_pages, page, |items, _| each(items)).await
}

/// [`paginate`], telling `each` whether its page is the last one the walk
/// reads (short of a failure), for a caller that says when a listing is
/// complete.
pub(super) async fn walk<T, Fut>(
    what: &str,
    max_pages: usize,
    mut page: impl FnMut(Option<String>) -> Fut,
    mut each: impl FnMut(Vec<T>, bool) -> bool,
) -> Result<(), SlackError>
where
    Fut: std::future::Future<Output = Result<(Vec<T>, Option<String>), SlackError>>,
{
    let mut cursor = None;
    let mut given = HashSet::new();
    for read in 1..=max_pages {
        let (items, next) = page(cursor.take()).await?;
        let next = match next.filter(|next| !next.is_empty()) {
            Some(next) if !given.insert(next.clone()) => {
                log::debug!("{what}: Slack repeated a cursor; that was the last page");
                None
            }
            next => next,
        };
        let capped = next.is_some() && read == max_pages;
        if !each(items, next.is_none() || capped) {
            return Ok(());
        }
        if capped {
            log::warn!("{what}: stopped after {max_pages} pages; the rest is left out");
        }
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
    Ok(())
}

/// `params`, plus the cursor when there is one.
pub(super) fn with_cursor(
    mut params: Vec<(&'static str, String)>,
    cursor: Option<String>,
) -> Vec<(&'static str, String)> {
    if let Some(cursor) = cursor {
        params.push(("cursor", cursor));
    }
    params
}

/// Whether a failed fetch may work later: an outage or a rate limit, not
/// Slack saying no (an unknown id stays unknown).
pub(super) fn worth_retrying(error: &SlackError) -> bool {
    !matches!(error, SlackError::Api(_) | SlackError::NeedsSession)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Page = std::future::Ready<Result<(Vec<u32>, Option<String>), SlackError>>;

    /// A pretend listing: page `n` holds `n`, and the cursor for page `n+1`
    /// is `"n+1"` until `last`.
    fn pages(
        last: u32,
        fail_at: Option<u32>,
        asked: &mut Vec<Option<String>>,
    ) -> impl FnMut(Option<String>) -> Page {
        move |cursor| {
            asked.push(cursor.clone());
            let n = cursor.map_or(0, |c| c.parse().unwrap_or(0));
            if fail_at == Some(n) {
                return std::future::ready(Err(SlackError::RateLimited));
            }
            let next = (n < last).then(|| (n + 1).to_string());
            std::future::ready(Ok((vec![n], next)))
        }
    }

    #[tokio::test]
    async fn paginate_stops_when_slack_repeats_itself() {
        // The same cursor every time: the second page is the last.
        let mut asked = 0;
        let walked = paginate(
            "t",
            10,
            |_| {
                asked += 1;
                std::future::ready(Ok((vec![1], Some("same".to_owned()))))
            },
            |_| true,
        )
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(asked, 2);
        // A caller that has seen it all before says stop, even though the
        // cursors keep changing.
        let asked = std::cell::Cell::new(0);
        let mut taken = 0;
        let walked = paginate(
            "t",
            10,
            |_| {
                asked.set(asked.get() + 1);
                std::future::ready(Ok((vec![0], Some(asked.get().to_string()))))
            },
            |_| {
                taken += 1;
                taken < 2
            },
        )
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(asked.get(), 2);
    }

    #[tokio::test]
    async fn walk_says_which_page_is_the_last() {
        let mut asked = Vec::new();
        let mut lasts = Vec::new();
        let walked = walk("t", 10, pages(2, None, &mut asked), |_, last| {
            lasts.push(last);
            true
        })
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(lasts, [false, false, true]);
        // Cut short by the cap, the last page read is the last.
        let mut asked = Vec::new();
        let mut lasts = Vec::new();
        let walked = walk("t", 2, pages(100, None, &mut asked), |_, last| {
            lasts.push(last);
            true
        })
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(lasts, [false, true]);
        // A repeated cursor ends it too.
        let mut lasts = Vec::new();
        let walked = walk(
            "t",
            10,
            |_| std::future::ready(Ok((vec![1], Some("same".to_owned())))),
            |_: Vec<u32>, last| {
                lasts.push(last);
                true
            },
        )
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(lasts, [false, true]);
    }

    #[test]
    fn a_refused_layout_leaves_the_text() {
        let params = vec![
            ("channel", "C1".to_owned()),
            ("text", "hi".to_owned()),
            ("blocks", "[]".to_owned()),
        ];
        assert_eq!(without_blocks(&params), params[..2]);
    }

    #[test]
    fn already_done_refusals_are_success() {
        let done = ["already_reacted", "no_reaction"];
        assert_eq!(done_if(Ok::<u8, SlackError>(1), &done), Ok(()));
        assert_eq!(
            done_if::<()>(Err(SlackError::Api("no_reaction".into())), &done),
            Ok(())
        );
        assert_eq!(
            done_if::<()>(Err(SlackError::Api("channel_not_found".into())), &done),
            Err(SlackError::Api("channel_not_found".into()))
        );
        // Only Slack's own codes count; a transport failure never does.
        assert_eq!(
            done_if::<()>(Err(SlackError::RateLimited), &done),
            Err(SlackError::RateLimited)
        );
        assert!(SlackError::Api("a".into()).is_code(&["b", "a"]));
        assert!(!SlackError::NeedsSession.is_code(&["not_allowed_token_type"]));
    }

    #[test]
    fn history_queries_ask_as_each_reader_does() {
        let ts = Ts::new("1.000100");
        let pairs = |query: HistoryQuery| query.params();
        assert_eq!(
            pairs(HistoryQuery::new("C1", 1)),
            [("channel", "C1".to_owned()), ("limit", "1".to_owned())]
        );
        assert_eq!(
            pairs(
                HistoryQuery::new("C1", 50)
                    .without_metadata()
                    .at(Some("next".into()))
            ),
            [
                ("channel", "C1".to_owned()),
                ("limit", "50".to_owned()),
                ("include_all_metadata", "false".to_owned()),
                ("cursor", "next".to_owned()),
            ]
        );
        assert_eq!(
            pairs(HistoryQuery::new("C1", 25).without_metadata().up_to(&ts)),
            [
                ("channel", "C1".to_owned()),
                ("limit", "25".to_owned()),
                ("include_all_metadata", "false".to_owned()),
                ("latest", "1.000100".to_owned()),
                ("inclusive", "true".to_owned()),
            ]
        );
        assert_eq!(
            pairs(
                HistoryQuery::new("C1", 25)
                    .without_metadata()
                    .after(&ts, Some(false))
            ),
            [
                ("channel", "C1".to_owned()),
                ("limit", "25".to_owned()),
                ("include_all_metadata", "false".to_owned()),
                ("oldest", "1.000100".to_owned()),
                ("inclusive", "false".to_owned()),
            ]
        );
        assert_eq!(
            pairs(HistoryQuery::new("C1", 50).after(&ts, None)),
            [
                ("channel", "C1".to_owned()),
                ("limit", "50".to_owned()),
                ("oldest", "1.000100".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn paginate_follows_cursors_to_the_end() {
        let mut asked = Vec::new();
        let mut seen = Vec::new();
        let walked = paginate("t", 10, pages(3, None, &mut asked), |p| {
            seen.extend(p);
            true
        })
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(seen, [0, 1, 2, 3]);
        assert_eq!(
            asked,
            [None, Some("1".into()), Some("2".into()), Some("3".into())]
        );
    }

    #[tokio::test]
    async fn paginate_stops_at_the_cap_and_at_errors() {
        let mut asked = Vec::new();
        let mut seen = Vec::new();
        let walked = paginate("t", 2, pages(100, None, &mut asked), |p| {
            seen.extend(p);
            true
        })
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(seen, [0, 1]);
        let mut asked = Vec::new();
        let mut seen = Vec::new();
        let walked = paginate("t", 10, pages(5, Some(2), &mut asked), |p| {
            seen.extend(p);
            true
        })
        .await;
        assert_eq!(walked, Err(SlackError::RateLimited));
        // What came before the failure was handed over.
        assert_eq!(seen, [0, 1]);
    }

    #[test]
    fn every_auth_code_means_signed_out() {
        for code in crate::slack::client::AUTH_ERRORS {
            assert_eq!(
                failure(&SlackError::Api((*code).to_owned())),
                Failure::SignedOut,
                "{code}"
            );
        }
    }

    #[test]
    fn codes_map_to_what_they_mean() {
        for (code, meant) in [
            ("missing_scope", Failure::MissingPermission),
            ("channel_not_found", Failure::ConversationGone),
            ("not_in_channel", Failure::NotInChannel),
            ("is_archived", Failure::Archived),
            ("msg_too_long", Failure::TooLong),
            ("edit_window_closed", Failure::CantEdit),
            ("cant_update_message", Failure::CantEdit),
            ("cant_delete_message", Failure::CantDelete),
            ("cant_delete_file", Failure::CantDeleteFile),
            ("error_name_taken", Failure::EmojiNameTaken),
            ("error_lower_case_names_only", Failure::InvalidName),
            ("resized_but_still_too_large", Failure::EmojiTooBig),
            ("error_bad_upload", Failure::BadEmojiImage),
            ("not_allowed_token_type", Failure::MissingPermission),
            ("code_already_used", Failure::LinkExpired),
            ("bad_redirect_uri", Failure::BadRedirect),
            ("bad_client_secret", Failure::BadClient),
            ("name_taken", Failure::NameTaken),
            ("invalid_name_specials", Failure::InvalidName),
            ("invalid_name_maxlength", Failure::NameTooLong),
            ("cant_leave_general", Failure::CantLeaveGeneral),
            ("restricted_action", Failure::Restricted),
            ("method_not_supported_for_channel_type", Failure::WrongKind),
            ("some_new_code", Failure::Slack("some_new_code".into())),
        ] {
            assert_eq!(failure(&SlackError::Api(code.into())), meant, "{code}");
        }
    }

    #[test]
    fn a_call_only_a_session_may_make_says_so() {
        assert_eq!(failure(&SlackError::NeedsSession), Failure::NeedsSession);
        assert!(!worth_retrying(&SlackError::NeedsSession));
    }

    #[test]
    fn transport_trouble_keeps_its_kind() {
        assert_eq!(failure(&SlackError::RateLimited), Failure::RateLimited);
        assert_eq!(failure(&SlackError::Http(502)), Failure::Http(502));
        assert_eq!(
            failure(&SlackError::Network("timed out".into())),
            Failure::Network("timed out".into())
        );
        assert_eq!(
            failure(&SlackError::Decode("eof".into())),
            Failure::Unexpected("eof".into())
        );
    }
}
