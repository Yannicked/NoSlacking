//! Asks Slack's search for a page of results.

use super::api::failure;
use super::{Event, Sink};
use crate::search::{PAGE_SIZE, Query, Scope, Sort};
use crate::slack::Client;
use crate::slack::search::{FilesAnswer, MessagesAnswer};

/// The parameters of page `page` of `query`.
pub fn params(query: &Query, page: u32) -> Vec<(&'static str, String)> {
    vec![
        ("query", query.text.clone()),
        ("count", PAGE_SIZE.to_string()),
        ("page", page.max(1).to_string()),
        (
            "sort",
            match query.sort {
                Sort::Relevant => "score",
                Sort::Newest => "timestamp",
            }
            .to_owned(),
        ),
        ("sort_dir", "desc".to_owned()),
        ("highlight", "true".to_owned()),
    ]
}

/// Reads page `page` of `query` and answers request `request`.
pub async fn search(client: Client, query: Query, page: u32, request: u64, sink: Sink) {
    let params = params(&query, page);
    let result = match query.scope {
        Scope::Messages => client
            .call::<MessagesAnswer>("search.messages", &params)
            .await
            .map(MessagesAnswer::into_page),
        Scope::Files => client
            .call::<FilesAnswer>("search.files", &params)
            .await
            .map(FilesAnswer::into_page),
    };
    sink.send(Event::Search {
        team: query.team,
        request,
        result: result.map_err(|error| failure(&error)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::Failure;
    use crate::slack::SlackError;

    #[test]
    fn queries_go_to_slack_as_typed() {
        let query = Query {
            team: "T1".into(),
            text: "from:@ana in:#general plan".into(),
            scope: Scope::Messages,
            sort: Sort::Newest,
        };
        let params = params(&query, 3);
        assert!(params.contains(&("query", "from:@ana in:#general plan".into())));
        assert!(params.contains(&("page", "3".into())));
        assert!(params.contains(&("sort", "timestamp".into())));
        assert!(params.contains(&("highlight", "true".into())));
    }

    /// An OAuth sign-in made before NoSlacking asked for search:read is
    /// refused either way.
    #[test]
    fn a_missing_permission_is_told_apart() {
        for code in ["missing_scope", "not_allowed_token_type"] {
            assert_eq!(
                failure(&SlackError::Api(code.into())),
                Failure::MissingPermission
            );
        }
        assert_eq!(failure(&SlackError::RateLimited), Failure::RateLimited);
    }
}
