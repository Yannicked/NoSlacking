//! Work teams and their channels, read through CSA: the teams you are
//! in, and a channel's posts with their replies.

use super::{TeamsClient, refused};
use crate::failure::Failure;
use crate::teams::auth::{Account, RESOURCE_CSA};
use crate::teams::types::{Message, Team, TeamsResponse};

/// The teams-and-channels list of the chat service aggregator (CSA).
const TEAMS_URL: &str = "https://teams.microsoft.com/api/csa/api/v1/teams/users/me?isPrefetch=false&enableMembershipSummary=true";

/// Where the CSA's channel posts are read, as the work web client reads a
/// channel (recorded): posts only, each with its replies.
const CSA_URL: &str = "https://teams.microsoft.com/api/csa/api/v1";

/// A channel's posts, as the CSA answers (HTTP 207).
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PostsPage {
    posts: Vec<CsaPost>,
}

/// One channel post: its message and its replies so far.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct CsaPost {
    message: Option<Message>,
    replies: Option<CsaReplies>,
}

/// A post's replies, the newest few, and how many there are.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct CsaReplies {
    messages: Vec<Message>,
    #[serde(rename = "totalCount")]
    total_count: u32,
}

/// What the CSA answers for a post's replies.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RepliesPage {
    #[serde(alias = "replies")]
    messages: Vec<Message>,
}

/// The posts of a CSA answer, each with its replies.
fn posts_of(page: PostsPage) -> Vec<Post> {
    page.posts
        .into_iter()
        .filter_map(|post| {
            let message = post.message?;
            let (replies, reply_count) = post
                .replies
                .map(|r| (r.messages, r.total_count))
                .unwrap_or_default();
            Some(Post {
                message,
                replies,
                reply_count,
            })
        })
        .collect()
}

/// A channel post with the replies it came with and how many it has.
#[derive(Clone, Debug, Default)]
pub struct Post {
    pub message: Message,
    pub replies: Vec<Message>,
    pub reply_count: u32,
}

impl TeamsClient {
    /// Fetches joined teams and channels from the chat service aggregator,
    /// which wants a bearer token of its own audience.
    /// The newest posts of the channel `channel`, with their replies, as
    /// the work web client reads a channel (CSA `containers/{id}/posts`,
    /// system lines left out). Needs the channel's team, which
    /// [`Self::get_teams`] learnt.
    pub async fn get_channel_posts(
        &self,
        channel: &str,
        limit: usize,
    ) -> Result<Vec<Post>, Failure> {
        let team = self
            .team_of_channel(channel)
            .ok_or_else(|| Failure::Unexpected("the channel's team is not known".into()))?;
        let encode = |id: &str| crate::percent::encode_strict(id).to_string();
        let url = format!(
            "{CSA_URL}/containers/{}/posts?modality=post&pageSize={limit}&teamId={}&filterSystemMessage=true",
            encode(channel),
            encode(&team)
        );
        let page: PostsPage = self.csa_get(&url, "read a channel").await?;
        Ok(posts_of(page))
    }

    /// The replies to the post `post` of the channel `channel`, from the
    /// CSA (`teams/{team}/channels/{channel}/posts/{post}/replies`, the web
    /// client's address for them).
    pub async fn get_post_replies(
        &self,
        channel: &str,
        post: &str,
        limit: usize,
    ) -> Result<Vec<Message>, Failure> {
        let team = self
            .team_of_channel(channel)
            .ok_or_else(|| Failure::Unexpected("the channel's team is not known".into()))?;
        let encode = |id: &str| crate::percent::encode_strict(id).to_string();
        let url = format!(
            "{CSA_URL}/teams/{}/channels/{}/posts/{post}/replies?pageSize={limit}",
            encode(&team),
            encode(channel)
        );
        let page: RepliesPage = self.csa_get(&url, "read a post's replies").await?;
        Ok(page.messages)
    }

    /// A GET from the CSA, with the sign-in the teams list takes.
    async fn csa_get<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        what: &str,
    ) -> Result<T, Failure> {
        let skype = self.credentials().skype_token;
        let resp = self
            .bearer(RESOURCE_CSA, |http, token| {
                let request = http
                    .get(url)
                    .bearer_auth(token)
                    .header("x-ms-client-version", "1416/1.0.0.2024050301");
                match &skype {
                    Some(skype) => request.header("X-Skypetoken", skype),
                    None => request,
                }
            })
            .await?;
        if !resp.status().is_success() {
            return Err(refused(resp, what).await);
        }
        resp.json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))
    }

    /// The team the channel `channel` is in, as the teams list said.
    pub fn team_of_channel(&self, channel: &str) -> Option<String> {
        self.channel_teams.read().ok()?.get(channel).cloned()
    }

    pub async fn get_teams(&self) -> Result<Vec<Team>, Failure> {
        // Teams free has chats only: no teams, and no list to ask.
        if self.credentials().account == Account::Personal {
            return Ok(Vec::new());
        }
        let skype = self.credentials().skype_token;
        let resp = self
            .bearer(RESOURCE_CSA, |http, token| {
                let request = http
                    .get(TEAMS_URL)
                    .bearer_auth(token)
                    .header("x-ms-client-version", "1416/1.0.0.2024050301");
                match &skype {
                    Some(skype) => request.header("X-Skypetoken", skype),
                    None => request,
                }
            })
            .await?;
        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }
        let data: TeamsResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        if let Ok(mut known) = self.channel_teams.write() {
            for team in &data.teams {
                for channel in &team.channels {
                    known.insert(channel.id.clone(), team.id.clone());
                }
            }
        }
        Ok(data.teams)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teams::auth::TeamsCredentials;

    #[tokio::test]
    async fn a_personal_account_has_no_teams_to_list() {
        let client = TeamsClient::new(TeamsCredentials {
            account: Account::Personal,
            ..TeamsCredentials::default()
        });
        assert_eq!(client.get_teams().await, Ok(Vec::new()));
    }

    #[test]
    fn channel_posts_read_with_their_replies() {
        // As the work web client's CSA answer has them (recorded shape):
        // camel case, a deleted post with no content, replies embedded.
        let page: PostsPage = serde_json::from_value(serde_json::json!({
            "posts": [{
                "containerId": "19:c@thread.tacv2",
                "id": "1700000000000",
                "message": {
                    "messageType": "RichText/Html", "content": null, "id": "1700000000000",
                    "imDisplayName": "Ann", "from": "8:orgid:ann", "parentMessageId": "1700000000000",
                    "composeTime": "2026-10-08T08:00:00Z", "properties": {"mentions": []}
                },
                "replies": {"totalCount": 4, "messages": [{
                    "messageType": "RichText/Html", "content": "<p>yes</p>", "id": "1700000000500",
                    "imDisplayName": "Bob", "from": "8:orgid:bob", "parentMessageId": "1700000000000"
                }]}
            }],
            "hasMoreBackward": true
        }))
        .expect("a page reads");
        let posts = posts_of(page);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].reply_count, 4);
        assert_eq!(posts[0].message.post_id(), None, "a post names itself");
        assert_eq!(
            posts[0].replies[0].post_id().as_deref(),
            Some("1700000000000")
        );
        assert_eq!(posts[0].replies[0].im_display_name.as_deref(), Some("Bob"));
    }
}
