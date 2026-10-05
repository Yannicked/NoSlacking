//! What every Web API call shares: plain words for its failures, and
//! walking a listing page by page.

use std::collections::HashSet;

use crate::slack::SlackError;

/// A user-facing description of an API failure.
pub(super) fn describe(error: &SlackError) -> String {
    if error.is_auth() {
        return "the sign-in is no longer valid; sign in again".into();
    }
    match error {
        SlackError::Api(code) => match code.as_str() {
            "missing_scope" => {
                "the Slack app lacks a permission; reinstall it from the manifest".into()
            }
            "channel_not_found" => "the conversation no longer exists".into(),
            "not_in_channel" => "you are not in that channel".into(),
            "is_archived" => "the channel is archived".into(),
            "msg_too_long" => "the message is too long".into(),
            "cant_update_message" | "edit_window_closed" => {
                "that message can no longer be edited".into()
            }
            "cant_delete_message" => "you cannot delete that message".into(),
            "invalid_code" | "code_already_used" => "the sign-in link expired; try again".into(),
            "bad_redirect_uri" => {
                "the redirect URL does not match the Slack app; check its OAuth settings".into()
            }
            "invalid_client_id" | "bad_client_secret" => "the client ID or secret is wrong".into(),
            other => other.replace('_', " "),
        },
        other => other.to_string(),
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
    fn failures_read_as_plain_sentences() {
        for code in crate::slack::client::AUTH_ERRORS {
            assert_eq!(
                describe(&SlackError::Api((*code).to_owned())),
                "the sign-in is no longer valid; sign in again",
                "{code}"
            );
        }
        assert_eq!(
            describe(&SlackError::Api("channel_not_found".into())),
            "the conversation no longer exists"
        );
        assert_eq!(
            describe(&SlackError::Api("some_new_code".into())),
            "some new code"
        );
        assert_eq!(describe(&SlackError::Http(502)), "HTTP 502");
    }
}
