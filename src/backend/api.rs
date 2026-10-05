//! What every Web API call shares: what its failures mean, and
//! walking a listing page by page.

use std::collections::HashSet;

use crate::failure::Failure;
use crate::slack::SlackError;
use crate::slack::session::Refusal;

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
        SlackError::Session(refusal) => match refusal {
            Refusal::NotACookie => Failure::NotACookie,
            Refusal::NoToken => Failure::NoSessionToken,
            Refusal::CookieRefused => Failure::CookieRefused,
        },
    }
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
    mut page: impl FnMut(Option<String>) -> Fut,
    mut each: impl FnMut(Vec<T>) -> bool,
) -> Result<(), SlackError>
where
    Fut: std::future::Future<Output = Result<(Vec<T>, Option<String>), SlackError>>,
{
    let mut cursor = None;
    let mut given = HashSet::new();
    for _ in 0..max_pages {
        let (items, next) = page(cursor.take()).await?;
        if !each(items) {
            return Ok(());
        }
        match next.filter(|next| !next.is_empty()) {
            Some(next) if !given.insert(next.clone()) => {
                log::debug!("{what}: Slack repeated a cursor; that was the last page");
                return Ok(());
            }
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
    log::warn!("{what}: stopped after {max_pages} pages; the rest is left out");
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
    !matches!(error, SlackError::Api(_))
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
