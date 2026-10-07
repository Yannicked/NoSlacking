# Microsoft Teams support: research (2026-10-06)

Read-only research: no Microsoft API was called and nothing was signed in
to. Microsoft's docs were read from their source repository,
`microsoftgraph/microsoft-graph-docs-contrib` at
`4ad99fd37a9e2e8538275a0a9cdff7907052f3ec`; the links below go to the
published learn.microsoft.com pages built from it.

## Status and decision

**Not being built for now.** The two routes compare like this:

- **The official route (Microsoft Graph with an app registration)** works
  for loading chats and sending on demand. It has no live updates for a
  desktop client, channels always need an administrator, and it can never
  carry calls. For the way NoSlacking is meant to work it is too
  cumbersome (section 1).
- **The route working third-party clients take** reimplements the Teams
  web client's private protocol. It gets there by signing in *as
  Microsoft's own Teams application*, which is what lets it skip the
  consent and app-governance controls an organisation applies to
  third-party apps. That is a step beyond NoSlacking's Slack sign-in, which
  reuses the user's own session with Slack, and it was decided not to
  build it (section 2).

The analysis of how Teams would fit NoSlacking (section 4) holds for either
route and is kept for if this is picked up again.

---

## 1. The official route: Microsoft Graph

### 1.1 What Graph can do for Teams messaging (delegated, as the signed-in user)

| Need | Graph API | Delegated permission | Admin consent? |
|---|---|---|---|
| List my chats (1:1, group, meeting), newest first, with the last message and my read position (`viewpoint`) | `GET /me/chats?$expand=lastMessagePreview&$orderby=lastMessagePreview/createdDateTime desc` (`$top` at most 50) | Chat.ReadBasic / Chat.Read | No |
| Read chat history | `GET /chats/{id}/messages` | Chat.Read | No |
| Send in a chat | `POST /chats/{id}/messages` | ChatMessage.Send or Chat.ReadWrite | No |
| Edit or delete my chat message | `PATCH` / `POST .../softDelete` | Chat.ReadWrite | No |
| Reactions (any emoji since 2024) | `POST .../setReaction`, `.../unsetReaction` | ChatMessage.Send / Chat.ReadWrite (chats), ChannelMessage.Send (channels) | No |
| Mark a chat read | `POST /chats/{id}/markChatReadForUser` (the whole chat, not one message) | Chat.ReadWrite | No |
| Create a chat | `POST /chats` | Chat.Create | No |
| My teams and channels | `GET /me/joinedTeams`, `GET /teams/{id}/channels` | Team.ReadBasic.All, Channel.ReadBasic.All | No |
| **Read channel messages and replies** | `GET /teams/{t}/channels/{c}/messages`, `.../replies` | **ChannelMessage.Read.All** | **Yes** |
| Post to a channel or reply | `POST .../messages`, `.../replies` | ChannelMessage.Send | No |
| **Edit or delete my channel message** | `PATCH`, `softDelete` | **ChannelMessage.ReadWrite** | **Yes** |
| Presence | `GET /me/presence`, `POST /communications/getPresencesByUserId` | Presence.Read(.All) | No |
| People | `GET /users` | User.ReadBasic.All | No |
| Files in messages | SharePoint/OneDrive items; inline images through `hostedContents` | Files.ReadWrite(.All) / Sites.* | **Yes for Files.*.All / Sites.*.All since July–August 2025** (Microsoft-managed consent policy) |
| Adaptive cards | Arrive as `attachments` with `contentType: application/vnd.microsoft.card.adaptive`. Users can't press card buttons through Graph (that is the bot framework) | — | — |
| Typing, other people's read receipts | **No Graph API** | — | — |

Message bodies are HTML (`body.contentType = html` whenever there is a
mention). Mentions are `<at id="n">` tags plus a `mentions[]` array.
Replies exist only in channels ("posts with replies"); chats have
quote-replies (`replyWithQuote`) but no threads.

Sources:
[chatMessage](https://learn.microsoft.com/graph/api/resources/chatmessage),
[list chats](https://learn.microsoft.com/graph/api/chat-list),
[list chat messages](https://learn.microsoft.com/graph/api/chat-list-messages),
[list channel messages](https://learn.microsoft.com/graph/api/channel-list-messages),
[update chatMessage](https://learn.microsoft.com/graph/api/chatmessage-update),
[setReaction](https://learn.microsoft.com/graph/api/chatmessage-setreaction),
[softDelete](https://learn.microsoft.com/graph/api/chatmessage-softdelete),
[markChatReadForUser](https://learn.microsoft.com/graph/api/chat-markchatreadforuser),
[presence](https://learn.microsoft.com/graph/api/presence-get),
[permissions reference](https://learn.microsoft.com/graph/permissions-reference).

### 1.2 Live updates: the blocker

- **Change notifications need a public HTTPS endpoint.** "If your endpoint
  isn't publicly accessible, Microsoft Graph doesn't send notifications to
  your endpoint." The alternatives are Azure Event Hubs and Event Grid,
  Azure resources someone has to own and pay for.
  ([webhooks](https://learn.microsoft.com/graph/change-notifications-delivery-webhooks))
  Graph has **no websocket or long-poll delivery for Teams messages.**
- What a signed-in user can subscribe to: `/chats/{id}/messages`,
  `/users/{id}/chats/getAllMessages`, and channel messages (admin consent).
  Subscriptions over an hour need a `lifecycleNotificationUrl`;
  notifications that carry the message need an encryption certificate.
  ([Teams message notifications](https://learn.microsoft.com/graph/teams-changenotifications-chatmessage))
- To work, NoSlacking would need a **relay service**: an HTTPS endpoint
  receiving Graph's notifications and passing them to each desktop. That
  means the project running servers that see users' messages, against its
  "your data stays on your machine" stance.
- **No delta sync for signed-in users.** `chats/getAllMessages/delta` and
  `getAllMessages` are application-only ("Delegated: Not supported").
  ([delta](https://learn.microsoft.com/graph/api/chatmessage-delta),
  [getAllMessages](https://learn.microsoft.com/graph/api/chats-getallmessages))
- **Polling is not allowed.** The Teams API overview, "Polling
  requirements": *"If your app polls to see whether a resource has
  changed, you can only do that once per day … Apps that don't follow these
  polling requirements will be considered in violation of the Microsoft
  APIs Terms of Use."* Re-reading when the user acts is fine; a loop is
  not. ([Teams API overview](https://learn.microsoft.com/graph/api/resources/teams-api-overview#polling-requirements))
- **Metering is gone.** The metered Teams APIs ended on 25 August 2025
  ([licensing](https://learn.microsoft.com/graph/teams-licenses)), and the
  protected-API process only covered application permissions.
- Resource-specific consent lets a Teams app *installed in a team or chat*
  read its messages without tenant admin consent, but it needs a packaged
  Teams app (sideloading is often disabled), is mainly for applications,
  and still delivers through webhooks. Not a fit for a desktop client.

### 1.3 Signing in a native app

- A **public client** (no secret): auth code with PKCE and a loopback
  redirect (`http://localhost:<port>`, any port), the same shape as
  NoSlacking's own-Slack-app OAuth. Refresh tokens rotate; no MSAL crate is
  needed.
- **Device code sign-in is often blocked** by a Microsoft-managed
  Conditional Access policy rolled out from February 2025.
  ([block authentication flows](https://learn.microsoft.com/entra/identity/conditional-access/policy-block-authentication-flows),
  [managed policies](https://learn.microsoft.com/entra/identity/conditional-access/managed-policies))
- **An app registration is required.** Two options:
  1. **A NoSlacking multi-tenant registration.** Since November 2020 users
     generally can't consent to an *unverified* multi-tenant app asking for
     more than basic profile permissions; it is stepped up to admin
     consent. Becoming a verified publisher needs a Microsoft partner
     (MPN) ID, which needs a business.
     ([publisher verification](https://learn.microsoft.com/entra/identity-platform/publisher-verification-overview))
     Channels need admin consent regardless.
  2. **"Bring your own app registration"**, the Teams twin of "your own
     Slack app". Most organisations don't let users register apps, and
     channels still need an admin.
- In practice: chats only might work through user consent in permissive
  tenants; channels need IT; in locked-down tenants nothing works until IT
  approves the app.

### 1.4 Throttling

From [throttling limits](https://learn.microsoft.com/graph/throttling-limits#microsoft-teams-service-limits):
- Reading a chat or channel message: 20 rps per app per tenant, 200 rps per
  app across all tenants, **1 rps per app per tenant per chat or channel**.
- Posting: 20 rps (chat) or 50 rps (channel) per app per tenant, **1 rps
  per user per chat or channel**.
- Listing chats: **5 rps per user**.
- Other Teams GETs: 30 rps per app per tenant and **1500 rps across all
  tenants**, a ceiling every NoSlacking user would share with one shipped
  registration.

**Verdict:** fine for an on-demand client (open a chat, load it, send).
Unworkable as a live chat client without a webhook relay or breaking the
polling rule. Channels always need admin consent.

---

## 2. The route third-party clients take (not pursued)

Every working third-party Teams client reimplements the Teams web client's
private protocol: its chat service for conversations and messages, and its
own websocket for live events (the counterpart of Slack's RTM socket).

| Project | Language | Notes |
|---|---|---|
| [EionRobb/purple-teams](https://github.com/EionRobb/purple-teams) | C (libpurple) | Active since 2022. |
| [IanTerzo/Squads](https://github.com/IanTerzo/Squads) | Rust (iced, tokio, reqwest) | MPL-2.0. |
| [YourSandwich/mautrix-teams](https://github.com/YourSandwich/mautrix-teams) | Go | AGPL-3.0; also joins meetings with audio, video and screen share. |
| [IsmaelMartinez/teams-for-linux](https://github.com/IsmaelMartinez/teams-for-linux) | Electron | Wraps the web client; nothing to reuse, but shows demand. |

They sign in to Microsoft **as Microsoft's own Teams application**, using
its client identity, rather than as themselves. Because that application is
first-party and pre-approved in every tenant, no consent prompt or admin
approval comes up. That is also the problem: it gets a third-party program
past the app-consent and app-governance controls an organisation sets for
third-party software. NoSlacking's Slack sign-in doesn't do this; it reuses
the user's own browser session with Slack.

Practical risks named in the research: tenant administrators see sign-ins
as "Microsoft Teams" from an unusual client; Conditional Access (compliant
device, token protection, blocked flows) can stop it outright; Microsoft
could add client attestation; and it falls under the Microsoft APIs Terms
of Use's ban on circumventing technical limits.

**Decision (2026-10-07):** not built. The working assumption for any
future Teams support is that it goes through a route the user's
organisation can see and approve.

---

## 3. Calls and meetings

- **Official:** Graph calling bots with application-hosted media must be
  C#/.NET on Windows Server in Azure, acting as a bot rather than the user
  ([requirements](https://learn.microsoft.com/microsoftteams/platform/bots/calls-and-meetings/requirements-considerations-application-hosted-media-bots)).
  The Azure Communication Services Teams interop SDK targets web and mobile
  and needs ACS resources and tenant setup. Neither fits a desktop client.
- **Unofficially** it is possible (mautrix-teams does it), as a separate
  project of weeks to months, but that depends on the route in section 2.

---

## 4. Fit with NoSlacking

### 4.1 How Slack-shaped the code is

- **The layering is right.** `src/slack/` is the only part that knows
  Slack's wire format; `src/backend/` turns it into `model` types and
  `Event`s; views never see Slack JSON. A `src/teams/` beside it would
  follow the AGENTS.md shape.
- **The worker is Slack-only throughout.** Each workspace holds a
  `slack::Client`; sign-in state, the RTM and Socket Mode connections, and
  `poll.rs` are Slack's. About half of `backend::Command`'s ~110 variants
  are generic (history, threads, send, react, edit, delete, mark read);
  the rest are Slack-only (sign-in variants, sections, Block Kit, slash
  commands, scheduled messages, custom emoji, huddles, DND, user groups,
  bookmarks, Later).
- **The model leaks Slack in a few places, all fixable cheaply:**
  - `model::Ts` is a Slack timestamp. Teams message ids are millisecond
    epochs and map losslessly to `"secs.mmm000"`.
  - `Message.text` is Slack mrkdwn, but `Message::rich_text()` already
    prefers a pre-built `mrkdwn::Block` tree, so Teams HTML can be
    translated straight into that, as `slack/rich.rs` does for `rich_text`.
    Sending needs an HTML writer, the counterpart of `slack/rich_out.rs`.
  - `Workspace::can(Feature)` is already a per-sign-in capability gate, the
    right hook for Slack-only features.
  - Block Kit types could draw Teams adaptive cards read-only.
  - `settings::WorkspaceMeta` needs a `service: Slack | Teams` field
    (serde default Slack) so existing settings keep loading.

### 4.2 Concept map

| Teams | NoSlacking model | Fit |
|---|---|---|
| Tenant (organisation) | `Workspace` (tenant + user; one person can be a guest in several) | Good |
| Team | No equivalent; closest are sidebar sections | Each team as a sidebar section holding its channels |
| Channel | `Conversation` (channel / private) | Good |
| 1:1 / group / meeting chat | `Direct` / `Group` | Good |
| Channel post with replies | Thread parent and replies | Partial: every post is a thread root; the thread panel works but looks Slack-like |
| Chat reply | Quote | Chats have no threads; quote-replies only |
| Reactions | `Reaction` | Good; legacy names (`like`, `heart`, …) map to emoji |
| Mentions | `@user` in rich text | Good |
| Read marker | `last_read` | Good |
| Presence (Available, Busy, DND, Away, Offline, …) | Active/away plus status | Richer in Teams; needs a small presence enum |
| Files | `File` | Images map well; SharePoint files are links |
| Custom emoji, Block Kit, slash commands, huddles, scheduled send, Later, user groups, DND sync | — | Slack-only, behind `Workspace::can` |

### 4.3 Suggested architecture (either route)

1. **Make the worker serve more than one service first,** with no Teams
   code and no visible change: a `Service` on workspaces and settings, an
   enum `Backend::{Slack(..), Teams(..)}` in the worker routed by team
   (an enum rather than a trait object, so `match` finds every place),
   Slack's sockets and polling under the Slack arm, and Slack-only UI
   driven by `Workspace::can`. Estimated 2–3 weeks.
2. **`src/teams/`** beside `src/slack/`: auth, networking (sharing the
   proxy-aware client from `slack/net.rs` through a common module), the
   service clients, types, HTML in and out, read-only cards; offline
   fixtures so `cargo test` stays offline.
3. **`src/backend/teams_translate.rs`**: Teams types into `model`,
   including id ↔ `Ts`.

---

## 5. If this is picked up again

- **Through Graph with an app registration** the organisation approves:
  on-demand loading, sending, reactions and read marks work; live updates
  need either the organisation's own webhook relay or a manual refresh;
  channels need admin consent; calls are out of reach. Worth it only if an
  organisation wants NoSlacking for Teams and is willing to register and
  approve it.
- **Before any Teams code:** the multi-service worker refactor (4.3, step
  1) is useful on its own and lowers the risk for Slack users.

---

## 6. Since then (feat/teams, 2026-10-07)

The chat route was built after all, behind the `teams` cargo feature, by
signing in as Microsoft's Teams desktop client. Notes for the next steps.

### 6.1 Personal accounts (Teams free)

Feasible but unproven. What has to change:

- Sign in with the consumer client id `8ec6bc83-69c8-4392-8f08-b3c986009232`
  (tenant `consumers`), chosen *before* the device code: a first-party id
  is bound to its audience, so detecting a personal account afterwards
  (`tid` 9188040d-6c67-4c5b-b112-36a304b66dad) only works as a check.
  Refreshes must use the same client id and scope.
- The skype token comes from `https://teams.live.com/api/auth/v1.0/authz/consumer`.
  ost asks for `https://api.spaces.skype.com/.default`; purple-teams, which
  ships a personal build, asks for
  `service::api.fl.spaces.skype.com::MBI_SSL openid profile offline_access`
  and sends `X-MS-Client-Consumer-Type: teams4life`.
- Personal access tokens may be opaque rather than JWTs, so who you are
  should come from the skype token's `skypeid` claim; MRIs are
  `8:live:…`, not `8:orgid:…`.
- There are no teams or channels (no CSA); chats only. The chat-service
  host and Trouter registration for consumers are unknown.
- ost declares a personal configuration but never uses it.

**Confirmed with `--teams-probe` against a real account (2026-10-07):**
the consumer client id on tenant `consumers` with scope
`service::api.fl.spaces.skype.com::MBI_SSL openid profile offline_access`
signs in (the `.default` scope is AADSTS70011); the access token is
opaque (`EwA…`); the consumer `authz` answers with the skype token at
`skypeToken.skypetoken` (24 h) and a `regionGtms` whose `chatService` is
`https://msgapi.teams.live.com` and `middleTier`
`https://teams.live.com/api/mt`; the skype token is a JWT whose `skypeid`
is `live:.cid.…`; the chat list, messages and Trouter negotiation all
work with it. One-to-one chats are `19:uni01_…@thread.v2` here too.
This is what the "Personal account (Teams free)" sign-in now does.
Sending works (with a numeric `clientmessageid`). The personal middle
tier's `fetchShortProfile` refuses every token we hold (401 with the
access token, the skype token, both, and the consumer headers), and a
thread's members (`/v1/threads/{id}`) carry ids only, so names come from
messages' `imdisplayname`: chats without a topic are named after their
recent writers.

A recording of teams.live.com showed why: the web client signs in with its
own client id (`4b3e8f46-56d3-427f-b1e2-d239b2ea6bca`) and calls the
middle tier with a token for
`https://mtsvc.fl.teams.microsoft.com/teams.mt.readwrite` plus the skype
token. It names personal people with `fetchShortProfile` and the work
people in personal chats with `fetchFederated`, and yourself with
`/beta/users/me`. Its chat token comes from `api/auth/v2.0/authz/consumer`.
Whether the consumer device-code client may have the middle tier scope is
what `--teams-probe` now tests.

### 6.2 Audio and video calls

ost (MIT) implements 1:1 and channel calls. Its flow: an IC3 token
(`https://ic3.teams.office.com/.default`); Trouter registration of
`NextGenCalling` / `DesktopNgc_2.5:SkypeNgc`; an SDP offer POSTed to the
epconv service from `regionGtms`, with callbacks to Trouter URLs.

The media is the Skype/Lync dialect, not WebRTC: SDES-SRTP (no DTLS), one
ICE session per m-line, PCMU audio, `X-H264UC` video, and SDP compressed with
a dictionary taken from Microsoft's binaries. str0m does DTLS-SRTP only,
so the huddle stack's audio pipeline (capture, AEC, jitter buffer,
speaker), TURN client, H.264 decode and call UI carry over, but the
transport does not. Either write a small SDES/ICE/RTP stack (ost's is
about 3k lines) or prove that Teams accepts the browser dialect (DTLS,
BUNDLE, Opus) through str0m.

Rough plan: a spike calling the Echo bot (3–5 days) to pick the
transport; outgoing 1:1 audio (2 weeks); talking (1 week); incoming
calls via Trouter (1–1.5 weeks); meetings (1–2 weeks); video receive
(2–3 weeks); camera and screen share (4+ weeks). The risks are policy
(it rests entirely on the first-party client id), protocol churn
(scraped version strings and capability masks), and codecs (Microsoft's
servers prefer SILK and X-H264UC).

### 6.3 What other clients taught

- **ost:** its personal-account scope is refused. Asking a device code
  for `https://api.spaces.skype.com/.default` with the consumer client id
  gives AADSTS70011 (invalid scope); `service::api.fl.spaces.skype.com::MBI_SSL
  openid profile offline_access` is accepted (`--teams-probe`, 2026-10-07).
- **teams-for-linux:** runs the web app and reads its page, so it has
  almost no protocol knowledge. Its tested Graph results with the web
  app's token: `/me`, calendar, mail and `/me/people` work; presence,
  `/me/chats` and creating chats are 403. Graph's `getByIds` is 403 for
  our token too, which is why people are looked up through the middle
  tier's `fetchShortProfile`. Work one-to-one chats are either
  `19:…@unq.gbl.spaces` or `19:uni01_…@thread.v2`. They saw a chat
  service send answer 201 and never arrive, so check sends end to end.
