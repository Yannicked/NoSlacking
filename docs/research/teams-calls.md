# Teams voice and video calls: research (2026-10-07)

How the Teams web client (teams.live.com, personal account) places and
answers one-to-one calls, read from a capture of its signalling, and how
NoSlacking could do the same on top of the huddle media stack.

The capture is 50 decoded messages from the user's own web session, one
JSON file per message, in time order: the HTTP requests the web app made
(`method`, `url`, `status`, `sent`, `got`) and the notifications the
server pushed over its Trouter socket (`url` = the callback path, `body`).
It holds three calls with the same person, who used a Teams desktop or
mobile client (it names itself `CallSignalingAgent (1416/…)`):

| Files | Call | Ended by |
|---|---|---|
| 000–019 | outgoing, audio and video offered, callee took audio only | the callee |
| 020–040 | incoming, audio and video offered | the caller |
| 041–049 | outgoing, audio only | the callee |

File numbers below ("file 007") point into that folder so each claim can
be checked. The capture itself holds live identifiers and must not be
committed.

Notation. `8:live:<me>` is our MRI, `8:live:<other>` the other person's.
`{surl}` is our Trouter base URL (`https://{trouter host}:3443/v4/f/{trouterId}/`),
`{fp}` the flight proxy `https://api.flightproxy.skype.com/api/v2`. ICE
passwords are `<24 chars>`, fingerprints `<32 hex pairs>`, our public and
LAN addresses `<public-ip>` and `<lan-ip>`. Microsoft's relay addresses are
shown by their prefix (`52.114.x.x`, `20.202.x.x`).

---

## Summary

- **The web client talks the browser dialect, and so does the callee for
  it.** Our side offers DTLS-SRTP (`a=setup:actpass`, SHA-256
  fingerprint), BUNDLE, `rtcp-mux`, Opus, H.264 and AV1 (file 000). The
  native callee answers in kind: DTLS (`a=setup:passive`), one ICE
  session for the bundle, Opus 111, H.264 (file 001). An incoming call
  from the native client offers the old Skype dialect (SILK, SATIN,
  X-H264UC, SDES `a=crypto`) as a superset *with* a DTLS fingerprint, and
  the web app answers it with plain WebRTC (DTLS active, Opus, H.264),
  which the caller accepts (files 020, 024). So §6.2's fear (SDES only, a
  separate ICE session per m-line, compressed SDP) does not hold for the
  web dialect, and **str0m's transport fits**. The SDP blob is plain text
  even under `contentType: "application/sdp-ngc-1.0"`.
- **Media is peer to peer.** In a 1:1 call there is no media server: the
  far end is a full ICE agent (no `a=ice-lite`) offering host,
  server-reflexive and relay candidates. In the incoming call the two
  ends settled on a host-to-host pair on the LAN (file 029,
  `a=remote-candidates`). Both test clients were on the same LAN, so
  traversal across NATs is not yet shown.
- **str0m cannot read Microsoft's SDP as it is.** Its parser accepts only
  `UDP/TLS/RTP/SAVPF` (and SCTP) as a protocol, and every Teams m-line says
  `RTP/SAVP`, so the whole SDP fails to parse. More mismatches follow
  (§D.5): implicit directions, rejected m-lines without `a=mid`, `m=x-data`,
  candidates with an `MTURNID` suffix or `TCP-ACT` transport, per-m-line
  ICE credentials, no `a=setup` in an incoming offer. The proposal is to
  own the SDP ourselves and drive str0m through its direct API (§F).
- **Signalling is all JSON over HTTPS, with callbacks to our Trouter
  URLs**: one `POST cpconv` starts a call (conversation and invitation in
  one request); the answer, acceptance, renegotiations, roster, and the
  end come back as pushes to URLs we made up. Every request carries just
  `X-Skypetoken`; no IC3 or other bearer token was seen.
- **Gaps in the capture**: our own hang-up, declining an incoming call,
  cancelling while it rings, the Trouter registration that brings
  incoming calls, the TURN server addresses, and any acknowledgement of
  `mediaAnswer` or `callAcceptance` were not recorded (§G).

---

## 1. Common ground

### 1.1 Hosts

Everything the web app sends goes through the consumer flight proxy,
which forwards to an internal host named in the path:

| Service | URL shape |
|---|---|
| Start a conversation and call | `POST {fp}/cpconv` |
| Conversation controller | `{fp}/cp/conv-{region}-prod-aks.conv.skype.com/conv/{convId}[/{action}]?i={node}&e={epoch}` |
| Call controller, outgoing call leg | `{fp}/cp/cc-{region}-prod-aks.cc.skype.com/cc/v1/{active\|mediaAnswers\|callAcceptance\|negotiations}/{callId}/…?i={node}[&e={epoch}]` |
| Call controller, incoming call leg | `{fp}/cp/cc-{region}-prod-aks.cc.skype.com/cc/v1/{forked\|incoming}/{callId}/…?i={node}` |
| Broker (event long-poll, not used in this capture) | `{fp}/cp/broker-{region}-prod-aks.broker.skype.com/api/v1/{subscribe\|publish}/{id}…` |

None of these is built by hand except `cpconv`: every other URL arrives in
an earlier answer or push, query string included, and is used verbatim.
`{region}` changed between calls (`euwe`, `frce`), so nothing may be
cached across calls.

### 1.2 Request headers

Every request in the capture carries the same set (files 000–044):

| Header | Value |
|---|---|
| `x-skypetoken` | the skype token (the only credential) |
| `content-type` | `application/json` |
| `x-microsoft-skype-chain-id` | a UUID per call: our call id for an outgoing call, the notification's `debugContent.callId` for an incoming one |
| `x-microsoft-skype-message-id` | a fresh UUID per request |
| `x-microsoft-skype-client` | `SkypeSpaces/1415/{build}/os=linux; …; browser=chrome; …/TsCallingVersion={ver}/Ovb={hash}` |
| `ms-teams-ring` | `general` |

The chain id comes back as `debugContent.callId` in our renegotiation
answers (file 007) and in the pushes' `X-Microsoft-Skype-Chain-ID`.

### 1.3 Ids we make up

| Id | Shape | Lifetime | Seen as |
|---|---|---|---|
| call id (chain id) | UUID | one call | header above; `debugContent.callId` (file 007) |
| endpoint id | UUID | the app session (same in all three calls) | `participants.from.endpointId`; the roster's endpoint key (file 004) |
| our participant id | UUID | one call | `participants.from.participantId` |
| the callee's participant id | UUID | one call | `participants.to[].participantId`; the callee's roster entry then carries it (file 004) |
| media leg id | 32 upper-case hex | one call | `mediaContent.mediaLegId`, echoed in every SDP push |
| call agent id | UUID | one call | the path segment after `callAgent/` in every callback URL |
| callback tag | 8 lower-case hex | one link | the segment after the call agent id |

The endpoint id is presumably the one the web app registered on Trouter
with (our `epid`); the capture does not show the registration.

### 1.4 Callback URLs

Every link we hand the server is

```
{surl}callAgent/{callAgentId}/{tag}/{scope}/{event}/
```

with `{scope}` `conversation` or `call`, and `{event}` the name of the
link (`rosterUpdate`, `conversationEnd`, `mediaAnswer`, `acceptance`,
`end`, `mediaAcknowledgement`, …). Two exceptions: `updateMediaDescriptions`
has no trailing slash (file 024). The server POSTs to it through Trouter,
so the frame we receive has `url` set to the path alone:
`/v4/f/{trouterId}/callAgent/{callAgentId}/{tag}/{scope}/{event}/`.

So a call session does not need to remember the tags: the call agent id
picks the session, the last two segments say what arrived. Links are
host `…-f.trouter.teams.microsoft.com:3443`; the pushes arrive with a
`Host` of `…-t.trouter…` and that is irrelevant to us.

The incoming-call notification is the one push that comes to the bare
`{surl}` path (`/v4/f/{trouterId}/`, file 020).

### 1.5 Pushes over Trouter

Each push is a Trouter request frame (`3:::{"id":…,"method":"POST","url":…,"headers":…,"body":…}`).
Facts from the headers:

- Callback pushes carry `X-Microsoft-Skype-Content-Encoding: gzip`
  (files 001–019): the body arrives compressed. The capture shows it
  decoded; the exact wire form (probably base64 of gzip inside the JSON
  string) must be checked on a raw frame. `src/teams/socket.rs`
  `parse_frame` reads the body as JSON directly, so it would see nothing.
- `Trouter-Timeout` is about 12 s for callbacks and 19 s for the call
  notification: our `3:::{"id":N,"status":200}` reply must go within it.
  `handle_frame_control` already sends it at once for every frame.
- `User-Agent` tells the sender: `CallController/…` for call-scope
  pushes, `ConversationService/…` for conversation-scope ones,
  `Skype-NotificationHub/…` for the incoming-call notification.

### 1.6 Participant object

Every request names us as

```json
{"id": "8:live:<me>", "displayName": "<my name>",
 "endpointId": "{endpoint id}", "participantId": "{our participant id}",
 "languageId": "en-us"}
```

(`from` in `participants`, `sender` in renegotiation answers, `acceptedBy`
in an acceptance). Pushes name the far end the same way, plus
`hidden: false` and `propertyBag`.

---

## A. Outgoing call

Timeline of file 000–019 (seconds from the first request):

| t | File | Direction | What |
|---|---|---|---|
| 0.0 | 000 | → | `POST cpconv` with conversation, invitation and SDP offer |
| 3.3 | 001 | ← | `call/mediaAnswer`: the callee's SDP answer, while it still rings |
| 5.3 | 002 | ← | `call/acceptance`: picked up, audio only, links for the live call leg |
| 5.5 | 003 | → | `PUT updateEndpointMetadata` |
| 5.5, 5.7 | 004, 005 | ← | `conversation/rosterUpdate` ×2 |
| 6.5 | 006 → 007 → 009 | ← → ← | renegotiation offer, our answer, acknowledgement |
| 6.5 | 008 | ← | `rosterUpdate` |
| 8.8 | 010 → 011 → 012 | | second renegotiation |
| 11.3 | 013 → 014 → 015 | | third renegotiation |
| 14.6 | 016 | → | `POST updateEndpointState` (muted) |
| 14.7 | 017 | ← | `rosterUpdate` (our mute) |
| 16.8 | 018 | ← | `call/end` (callee hung up) |
| 17.1 | 019 | ← | `conversation/conversationEnd` |

The second outgoing call (041–049) is the same up to acceptance, then had
no renegotiation at all: media ran on the acceptance's SDP until the
callee hung up at 8 s.

### A.1 `POST {fp}/cpconv` (files 000, 041)

Answered `201 Created`. One request creates the conversation and rings
the callee. Body, with the fields that matter in bold:

```text
conversationRequest:
  conversationType: null, subject: null, suppressDialout: false
  applicationType: "TFL"
  roster: {type: "Delta", rosterUpdate: {callback}/conversation/rosterUpdate/}
  properties: {allowConversationWithoutHost: true,
               enableGroupCallEventMessages: true,
               enableGroupCallUpgradeMessage: false,
               enableGroupCallMeetupGeneration: false}
  links: conversationEnd, conversationUpdate, localParticipantUpdate,
         addParticipantSuccess, addParticipantFailure, addModalitySuccess,
         addModalityFailure, confirmUnmute, receiveMessage   (all callbacks,
         scope "conversation")
contentSharing: null
participants:
  from: {id: 8:live:<me>, displayName, endpointId, participantId, languageId}
  to:   [{id: 8:live:<other>, participantId: {new UUID}}]
capabilities: null
endpointCapabilities: 73463
clientEndpointCapabilities: 42876960
endpointMetadata: {holographicCapabilities: 3}
groupContext / groupChat / meetingInfo / meetingData: null
endpointState: {endpointStateSequenceNumber: 1,
                endpointProperties: {additionalEndpointProperties:
                                     {infoShownInReportMode: "FullInformation"}}}
callInvitation:
  callModalities: ["Audio", "Video"]       (file 041: ["Audio"])
  replaces / transferor / clientTransferContext / customContext: null
  links: progress, mediaAnswer, acceptance, redirection, end
                                           (callbacks, scope "call")
  clientContentForMediaController: controlVideoStreaming, csrcInfo
                                           (callbacks, scope "call")
  pstnContent: {emergencyCallCountry: "", platformName: <x-microsoft-skype-client>,
                publicApiCall: false}
  emergencyContent: null
  mediaContent:
    blob: <SDP offer, see D.1>
    contentType: "application/sdp-ngc-1.0"
    requiredFeatures: "nonByPass"
    clientLocation: "NL"
    applyChannelParameters: {multiChannelParameter: {mids: ["*"],
        mediaParameter: "{\"sendSideBWSeed\":{\"seedValueBitsPerSec\":600000}}"}}
    mediaLegId: {32 hex}
  voicemailSettings: {}
  locationContent / networkContent / areaContent: null
debugContent: {ecsEtag: "<etag of the client config>"}
participantPropertyBag: {aiVoiceConsent: {value: {aiVoiceConsentValue: "0"},
                                          sequenceNumber: 0}}
```

What matters for us: `participants` (both MRIs and the ids of §1.3),
`callModalities`, the callback links (all must be reachable through our
Trouter session: the server will push to each of `progress`, `mediaAnswer`,
`acceptance`, `end`, `rosterUpdate`, `conversationEnd` at least),
`mediaContent.blob`, `contentType` and `mediaLegId`. The capability
numbers and the `ecsEtag` are opaque; copy the capability numbers, and
try leaving `debugContent` out.

The answer:

```text
conversationController: {fp}/cp/conv-…/conv/{convId}?i=…&e=…
sequenceNumber: 1, subject: ""
activeModalities: {realTimeActivityFeed: {links: {}}}
state: {isMultiParty: false, groupCallInitiator: null, isBroadcast: false,
        isMeetingActivated: false}
links: leave, addParticipant, removeParticipant, addModality,
       addParticipantAndModality (…/add), removeModality, mute, unmute,
       notificationLinks, merge, updateEndpointMetadata, updateEndpointState,
       admit, subscribe (broker), brokerPublish, brokerHttpTransport,
       conversationHttpTransport, setMeetingLayout, updateParticipantProperties,
       reportSyntheticMedia, publishState, removeState, updateMeetingSettings,
       searchParticipants, getAllParticipants, admitAll, updateMeetingGroups,
       updateParticipantMapping, joinMeetingGroup, leaveMeetingGroup,
       sendMessage, updateMeetingStates
callLimits: {remainingDurationInMinutes: 60, maxAllowedParticipants: 100,
             sponsor: "", enforcePaywallLimits: false}
conversationStartTime: "<ISO time>"
```

Keep `links.leave`, `links.updateEndpointState` and
`links.updateEndpointMetadata`; the rest are for group calls and
meetings. `brokerHttpTransport` and `conversationHttpTransport`
(`http://{ip}/enc`) look like an encrypted fast path and are not needed.

### A.2 `call/mediaAnswer` push (files 001, 042)

The callee's device answers the offer before the person picks up, so ICE
and DTLS can start during ringing ("early media"):

```text
mediaAnswer:
  sender: {id: 8:live:<other>, displayName, endpointId: {their endpoint},
           languageId, participantId: {the id we gave them}, hidden: false}
  mediaContent: {contentType: "application/sdp", blob: <SDP answer, D.2>,
                 mediaLegId: {ours}, escalationOccurring: false,
                 newOffer: false, clientLocation: "NL"}
  callModalities: ["audio", "video"]
  links: {mediaAcknowledgement: {fp}/cp/cc-…/cc/v1/mediaAnswers/{callId}/…/acknowledge?i=…}
debugContent: {ProcessingCallControllerInstance: "https://cc-…/"}
```

What to do: apply the answer to the peer (start ICE checks, then DTLS as
client; we offered `actpass`, they answered `passive`). The web app made
no HTTP request to `links.mediaAcknowledgement` (no such request between
files 001 and 003); either the Trouter delivery reply is enough or the
capture missed it (§G).

### A.3 `call/acceptance` push (files 002, 043)

The person picked up:

```text
callAcceptance:
  acceptedBy: {the callee, as in A.2, with languageId}
  acceptedCallModalities: ["audio"]
  links:
    acknowledgement:          {fp}/cp/cc-…/cc/v1/callAcceptance/{callId}/…/acknowledge?i=…
    callLeg:                  {fp}/cp/cc-…/cc/v1/active/{callId}/{n}/a{n}/{n}?i=…&e=…
    mediaRenegotiation:       {callLeg path}/renegotiate?…
    transfer, retargetCompletion, startOutgoingNegotiation, hold, monitor,
    updateCallState:          {callLeg path}/{name}?…
    replacement:              https://cc-…/cc/v1/callParticipant/…/replacement?rt=…&rc=…
    callControllerHttpTransport: http://{ip}/enc
  mediaContent: {contentType: "application/sdp", blob: <the answer again,
                 origin version +1>, mediaLegId, escalationOccurring: false,
                 newOffer: false, clientLocation}
  callKeepAliveInterval: 2700
  clientEndpointCapabilities: 799843, applicationType: "TFL",
  endpointCapabilities: 226559
```

What to do: the call is live; show it as connected; keep `links.callLeg`
(the hang-up and keep-alive target, §C.5) and
`links.mediaRenegotiation` (only needed if we start a renegotiation
ourselves, e.g. to add video). Again no acknowledgement request was made.

The SDP here is the mediaAnswer's with its origin version one higher and
directions adjusted to what was accepted: in file 002 `main-video` became
`recvonly` (the callee took audio only, so they send no video but still
receive ours); in file 043 `main-video` is rejected outright
(`m=video 0 RTP/SAVP 102`, dropped from BUNDLE). str0m takes one answer
per offer, so with the SDP API the acceptance cannot be applied as a
second answer; with the direct API (§F) we read the new directions and
change the media in place. Stage 1 can also ignore the mediaAnswer's SDP
and start the peer on the acceptance, at the cost of a slower start.

`callKeepAliveInterval: 2700` is in seconds (45 minutes). The web
client's code (`scheduleKeepAlives`) POSTs `{"callParticipantUpdate":{}}`
to `callAcceptance.links.callLeg` every nine tenths of it.

The caller must also acknowledge the pickup: the web client POSTs
`{"callAcceptanceAcknowledgement":{"links":{…}}}` to
`callAcceptance.links.acknowledgement` (`_sendCallAcceptanceAcknowledgement`),
the links being fresh callbacks for `mediaRenegotiation`, `transfer`,
`replacement`, `balanceUpdate`, `retargetCompletion`,
`controlVideoStreaming` and `updateMediaDescriptions`. Without it the
audio flows, but the far end gives up on the call a while in ("couldn't
complete call" on the phone).

### A.4 `PUT {conversationController}/updateEndpointMetadata` (files 003, 044)

Sent right after acceptance:

```json
{"participants": {"from": {…us…}}, "endpointMetadata": {"holographicCapabilities": 3}}
```

Answered 200 with the conversation (as in A.1) now including
`activeModalities.call.links.participants` (a call-hub URL). Probably
optional; send it for parity.

### A.5 Then

Roster updates (§C.2), renegotiations (§C.1), mute (§C.3) and the end
(§C.4) follow. Nothing else is required to keep a short call up.

---

## B. Incoming call

Timeline of files 020–040:

| t | File | Direction | What |
|---|---|---|---|
| 22.0 | 020 | ← | call notification at `{surl}` (evt 107) |
| 22.0 | 021 | → | `POST attach` on the forked leg, with a `join` of the conversation |
| 22.2 | 022 | → | `POST progress` "ringing" |
| 27.3 | 023 | → | `POST updateEndpointState` (not muted) |
| 27.7 | 024 | → | `POST accept` with our SDP answer |
| 28.0 | 025 | → | `PUT updateEndpointMetadata` |
| 28.1 | 026–028 | ← | `rosterUpdate` ×3 |
| 28.5 | 029 → 030 → 031 | | renegotiation (adds the data m-line) |
| 28.9 | 032 → 033 → 034 | | renegotiation |
| 29.2 | 035 → 036 → 037 | | renegotiation |
| 29.4 | 038 | ← | `rosterUpdate` |
| 31.7 | 039 | ← | `call/end` (the caller hung up) |
| 31.9 | 040 | ← | `conversation/conversationEnd` |

Where the notification goes: the personal web client keeps a second
Trouter connection (`go.trouter.teams.microsoft.com`) registered at
`https://teams.microsoft.com/registrar/prod/v2/registrations` as
`appId: SkypeSpacesWeb`, `templateKey: TFLSkypeSpacesWeb_2.0`, transport
`context: "TFL"`, `ttl: 3600`. Both recorded notifications and every call
callback arrived on that connection; the chat connection
(`TeamsCDLWebWorker` at `edge.skype.com`) got none. We register our one
connection both ways and renew both before the hour is up.

### B.1 The notification (file 020)

A Trouter POST to the bare `{surl}` path. Headers: `User-Agent:
Skype-NotificationHub/…`, `Content-Type: text/xml` (although the body is
JSON), the chain id = the call id. Body:

```text
evt: 107
gp:                                 (decoded by the capture tool; check the
                                     raw encoding on a live frame)
  callNotification:
    from: {id: 8:live:<other>, displayName, displayNameSource, endpointId,
           languageId, participantId, hidden, propertyBag}
    to:   {id: 8:live:<me>, displayName: null, endpointId: 0000…0000,
           participantId: {assigned to us}, …}
    links:
      attach:    {fp}/cp/cc-…/cc/v1/forked/{callId}/{n}/i1/{n}/attach?i=…
      progress:  {…forked path…}/progress?i=…
      reject:    {…forked path…}/reject?i=…
      mediaAnswer: "cc://ma"        (a placeholder: the answer goes with the acceptance)
      udpTransport: "udp://{ip}:3478/"
    mediaContent: {contentType: "application/sdp", blob: <SDP offer, D.3>,
                   mediaLegId: {theirs}, escalationOccurring: false,
                   newOffer: false, clientLocation}
    udpKey: {sessionKey: <base64>, ticket: <base64>}   (secrets; never log)
    fromApplicationType: "TFL"
    clientEndpointCapabilities: 799810
  conversationInvitation:
    conversationController: {fp}/cp/conv-…/conv/{convId}?i=…&e=…
    isMultiParty: false, isBroadcast: false
  debugContent: {callId, causeId, participantId, ProcessingCallControllerInstance,
                 potentialCallNotificationSent: false, ecsEtag, clientDebugContent}
  groupContext: null
```

Keep: `from` (who calls; the MRI names the chat to open), `to.participantId`
(our participant id for the rest of this call), the three links,
`mediaContent` (the offer and its `mediaLegId`),
`conversationInvitation.conversationController`, and `debugContent.callId`
(the chain id of every request we make for this call). `udpTransport` and
`udpKey` belong to the encrypted fast path; ignore them and keep them
out of logs.

### B.2 `POST {attach}` (file 021)

Sent at once, before the person decides: it ties our endpoint to the
forked leg and joins the conversation in the same request.

```text
attach:
  requireMediaContent: false
  links: {end: {callback}/call/end/}
  locationContent / networkContent / areaContent: null
  applicationType: "TFL"
capabilities: null
endpointCapabilities: 73463
additionalActions: [{
  name: "join",
  url: {conversationController from the notification},
  waitForResponse: true,
  input:
    capabilities: null, endpointCapabilities: 73463
    conversationRequest:
      applicationType: "TFL"
      roster: {type: "Delta", rosterUpdate: {callback}/conversation/rosterUpdate/}
      links: conversationEnd, conversationUpdate, localParticipantUpdate,
             addParticipantSuccess, addParticipantFailure, receiveMessage
    endpointMetadata: {}
    participants: {from: {us, participantId = to.participantId from B.1}}
}]
debugContent: {ecsEtag}
```

Answered 200:

```text
participants: {from: caller, to: us}
callInvitation:
  callModalities: ["audio", "video"]
  links:
    progress:    {fp}/cp/cc-…/cc/v1/incoming/{callId}/{n}/t/{n}/progress?i=…
    newOffer:    {…incoming path…}/mediaOfferRequest?i=…
    mediaAnswer: {…incoming path…}/mediaAnswer?i=…
    acceptance:  {…incoming path…}/accept?i=…
    redirection: {…incoming path…}/redirect?i=…
    callLeg:     {…incoming path…}/reject?i=…
    callController: "http://callcontroller.invalid"
    subscribe, brokerHttpTransport: (broker)
  mediaContent: null
additionalActionResponses: [{name: "join", url, output: {
  roster: {participants: {8:live:<other>: …}, type: "Delta", sequenceNumber: 0, …},
  conversationController, sequenceNumber: 1, activeModalities: {call: {links:
  {participants}}}, state, links: {leave, updateEndpointState,
  updateEndpointMetadata, … as in A.1}, subscriptionDetails: {selfParticipant},
  conversationStartTime }}]
```

Keep `callInvitation.links.progress`, `.acceptance` and `.callLeg` (note it
ends in `/reject`: probably the decline target for this leg), and the
join output's `links.leave`, `links.updateEndpointState` and
`links.updateEndpointMetadata`.

From here the caller's own `callAgent` callbacks are ours: the
`call/end` given in `attach.links.end` is where a caller's hang-up or a
pickup elsewhere will arrive.

### B.3 `POST {callInvitation.links.progress}` (file 022)

```json
{"callProgress": {"sender": {…us…}, "status": "ringing", "phrase": "ringing"}}
```

Answered 202, no body. This is what makes the caller hear ringing; send it
as soon as attach answered, then ring locally.

### B.4 `POST {updateEndpointState}` (file 023)

Before accepting, the web app says it is not muted:

```json
{"from": {…us…},
 "endpointState": {"state": {"isMuted": false},
                   "endpointProperties": {"additionalEndpointProperties":
                                          {"infoShownInReportMode": "FullInformation"}},
                   "endpointStateSequenceNumber": 1}}
```

Answered 200, no body. See §C.3.

### B.5 `POST {callInvitation.links.acceptance}` (file 024)

The person picked up:

```text
callAcceptance:
  acceptedBy: {…us…}
  acceptedCallModalities: ["Audio", "Video"]
  capabilities: null, endpointCapabilities: 73463,
  clientEndpointCapabilities: 42876960
  links: mediaRenegotiation, transfer, replacement, balanceUpdate,
         retargetCompletion, controlVideoStreaming,
         updateMediaDescriptions (no trailing slash)      (callbacks, scope "call")
  clientContentForMediaController: controlVideoStreaming, csrcInfo   (callbacks)
  mediaContent:
    blob: <our SDP answer, D.4>
    contentType: "application/sdp-ngc-1.0"
    clientLocation: "NL"
    applyChannelParameters: {as in A.1}
    mediaLegId: {the caller's, from B.1}
  pstnContent: {as in A.1}
  callKeepAliveInterval: null
  applicationType: "TFL"
```

Answered 200:

```text
callAcceptanceAcknowledgement:
  links: callLeg ({fp}/cp/cc-…/cc/v1/active/{callId}/{n}/a{n}/{n}?i=…&e=…),
         mediaRenegotiation, transfer, replacement, retargetCompletion,
         startOutgoingNegotiation, hold, monitor, updateCallState
  callKeepAliveInterval: 2700
```

Unlike the outgoing case, the acknowledgement is the HTTP answer itself.
Keep `links.callLeg` for hang-up. The `mediaRenegotiation` link we gave
is where the caller's renegotiation offers will arrive (file 029).

What to do on our side: build the answer before sending this, start ICE
as the controlled agent and DTLS as the client (§D.3), and start the
microphone (or not, if the user picked up muted).

### B.6 Then

`PUT updateEndpointMetadata` (file 025, as A.4), roster updates,
renegotiations, end, conversation end. In this call the caller
renegotiated three times within 1.5 s of acceptance.

### B.7 Declining and missed calls

Not recorded. Candidates, in order of likelihood: `POST {callNotification.links.reject}`
(before or instead of attach) and `POST {callInvitation.links.callLeg}`
(which ends in `/reject`, after attach). When the caller gives up or
another of our devices answers, a `call/end` should arrive at
`attach.links.end` (its `callControllerTransactionEnd.acceptedElsewhereBy`
field, file 040, suggests how "answered elsewhere" is said).

---

## C. During the call

### C.1 Renegotiation and acknowledgement

The far end may send a new offer at any time; in the capture it did so
2–3 times right after the call started, as its media stack settled.

**Push `call/mediaRenegotiation`** (files 006, 010, 013, 029, 032, 035), to
the URL we gave as `mediaRenegotiation` (in an acceptance for incoming
calls; for outgoing calls the server used a `call/mediaRenegotiation`
callback that appears in no request of the capture, under the same call
agent id: either the server derives it or the web app sent it in a
request the capture lacks. Route by call agent id and event name only):

```text
mediaNegotiation:
  sender: null
  mediaContent: {contentType: "application/sdp", blob: <new offer, D.3>,
                 mediaLegId, escalationOccurring: false, newOffer: false,
                 clientLocation}
  callModalities: ["audio"]           (file 006; whatever is active)
  links:
    mediaAnswer: {fp}/cp/cc-…/cc/v1/negotiations/{callId}/{n}/t/{n}/answer?i=…
    rejection:   {fp}/cp/cc-…/cc/v1/negotiations/{callId}/{n}/t/{n}/reject?i=…
debugContent: null
```

**Our `POST {links.mediaAnswer}`** (files 007, 011, 014, 030, 033, 036),
within about 0.2 s in the capture:

```text
mediaAnswer:
  callModalities: ["Audio", "Video"]
  sender: {…us…}
  links: {mediaAcknowledgement: {callback}/call/mediaAcknowledgement/}  (a new tag each time)
  clientContentForMediaController: controlVideoStreaming, csrcInfo     (new callbacks)
  mediaContent: {blob: <our answer>, contentType: "application/sdp-ngc-1.0",
                 clientLocation, applyChannelParameters, mediaLegId}
debugContent: {callId, endpointId}
```

Answered `202 Accepted`, no body.

**Push `call/mediaAcknowledgement`** (files 009, 012, 015, 031, 034, 037),
to the link we just gave:

```json
{"mediaAcknowledgement": {"reason": "noError", "sender": null, "code": 0,
  "subCode": 0, "phrase": "Success", "resultCategories": ["Success"]},
 "links": {"callLeg": null}, "debugContent": {}}
```

A non-zero `code` would mean our answer was refused; treat it as a
failure of that renegotiation (and log the phrase). If we cannot answer
at all, `links.rejection` is the polite way out (its body is unknown).

What the renegotiations changed (from diffs of the SDPs):

| File | Change |
|---|---|
| 006 | bandwidth figures of `main-video` raised; the data m-line gains `fast_bandwidth_feedback` and `x-bwealgorithm` |
| 010 | `applicationsharing-video` removed (`m=video 0 RTP/SAVP 34`, out of BUNDLE) because our answer 007 had rejected it; `main-video` raised again |
| 013 | `main-video` loses `recvonly`, i.e. becomes `sendrecv` |
| 029 | origin address now the far end's host address; only the nominated host candidate and `a=remote-candidates` (ICE has concluded); a **new** `m=x-data` with its own ICE credentials, candidates and `setup:actpass` |
| 032 | the data m-line moves into the bundle (same port, credentials and `setup:passive` as the rest) |
| 035 | small bandwidth and extension changes |

Our answers changed only their origin version, `main-video`'s direction
(`sendonly` while they were `recvonly`, then `sendrecv`), the data
m-line's random `x-ssrc-range`, and (030) gained the data m-line.

### C.2 Roster updates

**Push `conversation/rosterUpdate`** (files 004, 005, 008, 017, 026–028,
038, 045–047):

```text
participants:
  "{mri}":
    version: {n}            (per participant, rises with each change)
    state: "active"
    details: {id, displayName, displayNameSource, propertyBag, resourceId,
              participantType: "inTenant", endpointId: 0000…, participantId: null,
              languageId: null, hidden: false}
    endpoints:
      "{endpointId}":
        call: {serverMuteVersion: 0}
        endpointCapabilities, clientEndpointCapabilities, participantId,
        clientVersion, languageId, endpointJoinTime, modalityJoined: "Call",
        endpointMeetingRoles: ["none"]
        endpointMetadata: {…}   (ours: holographicCapabilities; theirs:
                                 isMicrophoneOn, isSpeakerOn, isVideoOn,
                                 isCallMediaCaptured, …, all false in file 008)
        endpointState: {endpointStateSequenceNumber, state: {isMuted}}
    role: "admin", meetingRoles: [], enforceConsentToJoin: false
type: "Delta"
sequenceNumber: {n}         (per conversation, rises by one per push)
participantCounts: {totalParticipants: 2, …}
```

Each push is a delta: it carries only the participants that changed
(both in the first, one in each later one). Apply by MRI and keep the
highest `version`. For our UI: who is in the call (both entries
`active` once the callee joined), and whether the other person is muted
(`endpointState.state.isMuted`; the native callee never set it in the
capture, and its `endpointMetadata.isMicrophoneOn` stayed false while it
was clearly talking, so do not show "muted" from `isMicrophoneOn`).
Names come from `details.displayName`. A participant leaving was not
captured (the call ended first); expect `state` other than `active` or
the entry with no endpoints.

### C.3 Mute

`POST {conversation links.updateEndpointState}` (files 016, 023):

```json
{"from": {…us…},
 "endpointState": {"endpointStateSequenceNumber": 3,
                   "endpointProperties": {"additionalEndpointProperties":
                                          {"infoShownInReportMode": "FullInformation"}},
                   "state": {"isMuted": true}}}
```

Answered 200, no body; a roster update with our new state follows (file
017). The sequence number must rise with each update; `cpconv` used 1,
the mute in file 016 used 3 (a 2 was not captured, maybe a request the
capture filtered). Muting is otherwise local: keep sending Opus silence
or nothing, as the huddle stack does.

### C.4 The end

**Push `call/end`** to the `end` callback (files 018, 039, 048):

```text
callEnd:
  reason: "noError"
  sender: {who ended it}
  code: 0, subCode: 0, phrase: "LocalUserInitiated", resultCategories: []
debugContent: null
```

**Push `conversation/conversationEnd`** (files 019, 040, 049), a few
hundred ms later:

```text
code: 0, subCode: 5002,
phrase: "This conversation has ended as all participants left the audio-video modality."
sender: null, resultCategories: ["Success"]
callControllerTransactionEnd: {sender, reason, code, subCode, phrase,
                               resultCategories, acceptedElsewhereBy: null}
conversationType: "default"
```

What to do: on `call/end`, stop the media and the microphone and report
the call over; on `conversationEnd`, drop the conversation state. A
declined or unanswered outgoing call probably also ends with `call/end`,
with a non-zero code (not captured); map `code`/`subCode` to a
`Failure` in one place, as `backend/api.rs` does for Slack, with
`(0, _)` as a normal end.

### C.5 Hanging up ourselves, and declining

Recorded since (a second Chromium capture, three calls):

**Hanging up**, ringing or connected, is `POST {conversation links.leave}`
(the `leave` link of the `cpconv` answer), answered `204`. No request
goes to the call leg.

While it still rings:

```json
{"participants": {"from": {"id": "8:live:<me>", "displayName": "<name>",
                           "endpointId": "{endpoint id}", "languageId": "en-us"}},
 "conversationTransactionEnd": {"reason": "noError", "code": 0,
                                "phrase": "ConversationEndNoModalityConnected"},
 "callTransactionEnd": {"code": 487, "subCode": 0,
                        "phrase": "CallEndReasonLocalUserInitiated",
                        "resultCategories": ["Success"],
                        "callQualityDiagnosticsInformation": {"cancelationDuration": 4}}}
```

`cancelationDuration` is whole seconds since the call started. Once
connected, `from` also carries our `participantId`, and
`callTransactionEnd` is `{code: 0, subCode: 0, phrase:
"CallEndReasonLocalUserInitiated", resultCategories: ["Success"]}`.
The server then pushes `call/end` (ringing: `reason: "clientError"`,
`code: 487`; connected: `reason: "noError"`, `code: 0`) and
`conversation/conversationEnd` (`code: 0, subCode: 5002`). Our own 487 is
a normal end.

**Declining** an incoming call is `DELETE {callInvitation.links.reject}`
(the attach answer's `…/cc/v1/incoming/{id}/…/reject?…`), answered `202`:

```json
{"callEnd": {"code": 603, "subCode": 0, "phrase": "CallEndReasonLocalUserInitiated",
             "resultCategories": ["Success"], "applicationType": "TFL"}}
```

followed by a `conversation/conversationEnd` push. Either way, close the
media locally at once: a hang-up must never wait on the network.

### C.6 Screen sharing (recorded: `teams.live.com4.har`, the web client sharing twice, then the phone)

Recorded, and differing from what the code suggested: the offer's
`mediaContent` also carries `requiredFeatures: "nonByPass"` and
`negotiationTag: "{our participant id};ss_{n}"` (the same `n` for a
share's start and stop, rising by two per share); the acknowledgement of
the answer is an empty body; a stop drops the share line with port 0
(`m=video 0 RTP/SAVP 34` and its label), never inactive; sharing again
brings the line back at a new mid (one past the highest); about a second
after answering a start Microsoft offers again, raising the share's
limits, which the sharer answers `sendonly` with `ScreenSharer`; a line
only received on names no SSRC of ours; and the phone's share arrives at a
new mid with ICE of its own (answered bundled), at the camera's payload
type (told apart by `x-ssrc-range`), and its stop is never signalled.

What the code says, all of which held:

A share in a 1:1 call is a renegotiation by the sharer, nothing else: no
content-sharing session, no conversation modality, no data-channel
message. The sharer offers the `applicationsharing-video` line
`sendonly` (H.264 at the camera's number, `max-fs=8160;max-mbps=135000;
max-fps=1500`, 15 fps), with `callModalities` gaining `ScreenSharer`
(the values are `Audio`, `Video`, `ScreenSharer`, `ScreenViewer`, from the
directions of the offerer's own lines). The offer goes to the far end's
`mediaRenegotiation` link (from its acceptance, or our acceptance's
acknowledgement), as

```text
{"mediaNegotiation": {"callModalities": [...], "sender": {us},
  "links": {"mediaAnswer": {callback}/call/mediaAnswer/,
            "rejection": {callback}/call/rejection/},
  "mediaContent": {blob, contentType, mediaLegId, …}}}
```

and its answer comes as a `call/mediaAnswer` push (the HTTP answer says
nothing), acknowledged at its `mediaAcknowledgement` link; a refusal comes
to `call/rejection` (`491`/`3118` when two renegotiations cross, after
which the web client answers the other and sends its own again).
Stopping is the same with the line inactive or dropped and no
`ScreenSharer`. The receiver answers the share line `recvonly` with
`ScreenViewer`. We offer in the shape of the far end's latest
description (its mids and payload types), keep the DTLS role, and drop
the line again to stop, as the far end's own renegotiations do.

---

## D. The SDP

### D.1 Our offer (files 000, 041)

Session part:

```text
v=0
o=- {random 64-bit} 2 IN IP4 127.0.0.1
s=-
b=CT:4000
t=0 0
a=extmap-allow-mixed
a=msid-semantic: WMS *
a=group:BUNDLE 0 1 2 3
```

Four m-lines, all on one transport (one `ice-ufrag` of 4 chars, one
`ice-pwd` of `<24 chars>`, one `fingerprint:sha-256 <32 hex pairs>`,
`setup:actpass`, `ice-options:trickle`, `rtcp-mux`), every one with
protocol `RTP/SAVP`, the port and `c=` of our TURN relay candidate, and
`a=rtcp:{same port}`:

| mid | m-line | `a=label` | direction | Notes |
|---|---|---|---|---|
| 0 | `audio` | `main-audio` | `sendrecv` | the candidates are listed here only |
| 1 | `video` | `main-video` | `sendrecv` (041: `recvonly`) | `ssrc-group:FID` with rtx when sending |
| 2 | `video` | `applicationsharing-video` | `inactive` | screen share, unused |
| 3 | `x-data` | `data` | `sendrecv` | `a=x-data-protocol:sctp`, `a=sctp-port:5000`, `a=max-message-size:262144` |

Audio, in preference order: 105 `CN/48000`, 111 `opus/48000/2`
(`minptime=10;useinbandfec=1`, `rtcp-fb:111 transport-cc`), 63
`red/48000/2` (`111/111`), 9 `G722/8000`, 0 PCMU, 8 PCMA, 13 `CN/8000`, 110
`telephone-event/48000`, 126 `telephone-event/8000`. Extensions: 1
`ssrc-audio-level`, 2 abs-send-time, 3 transport-wide-cc, 4 `sdes:mid`.

Video (both video m-lines): H.264 at 102/104 (`42001f`, packetization 1
and 0), 108/114 (`42e01f`), 116/39 (`4d001f`), 118 (`64001f`); AV1 at 45
(`profile=0`); one rtx per codec. File 041 adds `f4001f` (41/43), a
`64001f` packetization-0 (120) and AV1 profile 1 (47). No VP8 or VP9.
Feedback: `goog-remb`, `transport-cc`, `ccm fir`, `nack`, `nack pli`;
`rtcp-rsize`. Extensions: toffset, abs-send-time, video-orientation,
transport-wide-cc, video-content-type, video-timing, color-space, mid,
rtp-stream-id, repaired-rtp-stream-id.

Microsoft's additions in our offer:

- `a=x-ssrc-range:{ssrc}-{ssrc}` on each sending m-line: our one SSRC
  (audio, sending video, data).
- `a=x-signaling-fb:* x-message app recv:dsh` (audio) and
  `… send:src recv:src,vc` (video): what in-band control messages we take.
- `a=label:…` on every m-line, naming its role. Microsoft's side keys its
  streams by label, so it must be kept.
- `b=CT:4000` (kbit/s).
- Two extension URIs are written with backslashes:
  `http:\\www.webrtc.org\experiments\rtp-hdrext\abs-send-time` and
  `http:\\www.ietf.org\id\draft-holmer-rmcat-transport-wide-cc-extensions-01`,
  while the others keep forward slashes. The browser writes them with
  slashes, so the web app's calling library rewrites them; we must do the
  same (both ways).
- `m=x-data … RTP/SAVP 127 126` with `rtpmap:127 x-data/90000` and
  `rtpmap:126 rtx/90000`: the browser's data channel
  (`m=application … UDP/DTLS/SCTP webrtc-datachannel`) dressed as an RTP
  m-line.
- `RTP/SAVP` instead of the browser's `UDP/TLS/RTP/SAVPF` throughout.

Candidates: all gathered before sending, although `trickle` is declared;
no trickle messages exist in the capture. File 000 has two `host`, two
`srflx` and one `relay` UDP candidate (the relay a `52.114.x.x` Microsoft
relay); file 041 one fewer srflx. No TCP candidates in the offer (the
renegotiation answers add the browser's `tcp-act … 9 typ host`).

The offer goes in `mediaContent.contentType: "application/sdp-ngc-1.0"`;
the blob is the plain SDP text with CRLF line ends, not compressed.

### D.2 The callee's answer (files 001, 002, 042, 043)

From the native client, `contentType: "application/sdp"`:

```text
v=0
o=- 0 {0, then +1 per version} IN IP4 {its relay address, 20.202.x.x}
s=-
c=IN IP4 {same}
b=CT:4000
t=0 0
a=x-mediabw:main-video send=12000;recv=12000
a=x-mediabw:applicationsharing-video send=12000;recv=12000
a=group:BUNDLE 0 1 2 3
```

Per m-line, same mids and order as our offer, protocol `RTP/SAVP`, port
= its relay port:

- **No direction attribute on active m-lines** (audio, and main video in
  the mediaAnswer): `sendrecv` is implied. `recvonly`/`sendonly`/`inactive`
  appear only when they apply.
- One `ice-ufrag` (4 chars) and `ice-pwd` for all m-lines; candidates on
  the audio m-line only.
- `a=setup:passive`, one `fingerprint:sha-256`. **No `a=ice-lite`**: a
  full ICE agent.
- Candidates (file 001), each followed by
  `a=x-candidate-info:{foundation} network-type=wlan`:
  - `UDP … typ relay raddr <public-ip> rport … MTURNID {decimal}` (its TURN relay, `20.202.x.x`)
  - `UDP … typ host` (`<lan-ip>`)
  - `TCP-ACT … typ host` (port 1024)
  - `UDP … typ srflx raddr <lan-ip> …`
  - `TCP-ACT … typ srflx …`
  - `TCP-PASS … typ relay …` and `TCP-ACT … typ relay …`
  The transport is upper case and the TCP forms are Microsoft's
  (`TCP-ACT`/`TCP-PASS` in the transport field, not RFC 6544's
  `tcp … tcptype active`).
- `a=rtcp-fb:* x-message app send:dsh,x-gain recv:dsh,x-gain` (audio),
  `… send:src,x-pli recv:src,x-pli` (video); `a=rtcp-fb:* nack`,
  `transport-cc`, `goog-remb`, `nack pli`; `rtcp-rsize`; `rtcp-mux`.
- `a=x-ssrc-range:` 1 SSRC for audio, 100 for each video m-line and the
  data m-line, with `ssrc-group:FID {a} {a+50}`, no `a=ssrc` lines. The
  SSRCs are small numbers (3079, 3080–3179, …).
- `a=label`, and `a=x-source:main-audio` / `main-video`;
  `a=x-mediasettings:applicationsharing-video=required` on the share line.
- Audio: 111 `opus/48000/2` with
  `maxplaybackrate=16000; sprop-maxcapturerate=16000; useinbandfec=1; usedtx=1; minptime=10`
  (wideband Opus, with DTX and FEC), 9 G722, 0 PCMU, 8 PCMA, 13 `CN/8000`,
  120 `CN/48000`, 126 `telephone-event/8000` `0-16`; `a=ptime:20`,
  `a=maxptime:200`; one extension, abs-send-time (backslash form, id 2).
- Video: 102 `H264/90000`
  `profile-level-id=42C02A;packetization-mode=1;max-mbps=…;max-fs=…;max-br=…;max-fps=…`
  (constrained baseline, level 4.2, but `max-fs=240` = 320×192 at first,
  raised by renegotiation to 8160) and 103 rtx.
- Data: `m=x-data … RTP/SAVP 127 126`, `a=x-data-protocol: sctp` (with a
  space), no `sctp-port`.

The share line (mid 2) stays `inactive`; in the acceptance of file 043
main video is rejected as the bare line `m=video 0 RTP/SAVP 102`: no
`a=mid`, no other attribute, and left out of BUNDLE (`0 2 3`).

### D.3 The caller's offer for an incoming call, and renegotiation offers

**Incoming offer (file 020)** from the native client, the Skype dialect
with a DTLS fingerprint added:

- `s=session`; `a=group:BUNDLE audio_0 video_1` (two m-lines only, mids
  are names).
- **A separate ICE session per m-line**: audio and video have different
  `ice-ufrag`/`ice-pwd`, their own candidates, and **component 2 (RTCP)
  candidates** on port+1 besides component 1, with `a=rtcp:{port+1}`,
  yet also `a=rtcp-mux`.
- Five `a=crypto` lines (SDES keys: `AES_CM_128_HMAC_SHA1_32`,
  `AES_CM_128_HMAC_SHA1_80` twice, `AEAD_AES_256_GCM` twice) **and** a
  `fingerprint:sha-256`, but **no `a=setup`**.
- Audio: 109 `SATINFB/48000/2`, 108 `SATIN/48000`, 104 `SILK/16000`, 102
  `opus/48000/2` (the same fmtp as D.2), 9 G722, 111 `SIREN/16000`, 18
  G729, 0 PCMU, 8 PCMA, 103 `SILK/8000`, 97 `RED/8000`, 13/118/117/120 CN
  at 8, 16, 24 and 48 kHz, 101 `telephone-event/8000`.
- Video (its own `c=` relay address): 119 `X-MSAV1`, 122 `X-H264UC`
  (`mst-mode=NI-TC`), 107 `H264` (`42C02A`, `max-fs=240`), 114 `H265v2`,
  123 `x-ulpfecuc`, and rtx for each.
- Extensions: abs-send-time, `http:\\skype.com\experiments\rtp-hdrext\frame-counters`,
  `http:\\skype.com\experiments\rtp-hdrext\fast_bandwidth_feedback#version_3`;
  `a=x-bwealgorithm:rmestimator bwc packetpair webrtc`.

The web app's answer (D.4) ignored all the Microsoft codecs, the
`a=crypto` keys and the per-m-line ICE, and the caller took it. That is
the main finding of this capture.

**Renegotiation offers** (files 006, 010, 013 for our call; 029, 032, 035
for theirs) are D.2's or D.3's shape, `contentType: "application/sdp"`,
with the origin version rising, `a=setup:passive` (the DTLS roles stay as
first settled), and, once ICE is done (file 029), only the nominated
local candidate and `a=remote-candidates:1 {our ip} {port} 2 {our ip} {port}`.
File 029 also adds an `m=x-data` (mid `data_2`) with **its own ICE
credentials and a full candidate set and `setup:actpass`**; file 032
folds it into the bundle.

### D.4 Our answers (files 007, 011, 014, 024, 030, 033, 036)

The browser's answer, rewritten like the offer (`RTP/SAVP`, labels,
`x-ssrc-range`, backslash URIs), `contentType: "application/sdp-ngc-1.0"`:

- Same session part as D.1 with our own origin id; BUNDLE over the
  m-lines we keep; one ICE session; `setup:active` (we are the DTLS
  client in every case: the far end said `passive` in its answer, or
  nothing at all in its offer).
- **Every candidate we have, in every answer**: host, srflx, several
  relay (`52.114.x.x`, different relays) and the browser's
  `tcp-act … 9 typ host`. Lower case `udp`/`tcp-act` here.
- Audio keeps only the codecs both sides have, at the offerer's numbers
  (file 024: Opus at 102, CN 48000 at 96 then 120).
- Video: one H.264 (`42e01f`, packetization 1, at the offerer's payload
  number) and its rtx; `sendonly`/`sendrecv` as the offer allows.
- The share m-line we reject as `m=video 0 RTP/SAVP 34` followed only by
  `a=label:applicationsharing-video`: payload type 34 (H.263) as a
  placeholder, no `a=mid`.
- The data m-line, when offered: `x-data` with `a=x-data-protocol:sctp`,
  `a=sctp-port:5000`, a new random `x-ssrc-range` each time.

### D.5 What str0m 0.24.1 does with this

Read in `str0m-0.24.1/src/sdp/parser.rs`, `data.rs` and `change/sdp.rs`:

| Microsoft SDP | str0m | Consequence |
|---|---|---|
| `RTP/SAVP` on every m-line | `media_line` accepts only `UDP/TLS/RTP/SAVPF`, `DTLS/SCTP`, `UDP/DTLS/SCTP` | **parse error for the whole SDP**; blocking unless rewritten |
| no direction attribute (D.2) | `check_consistent` wants exactly one direction per SRTP m-line | "inconsistent"; add `a=sendrecv` |
| rejected `m=video 0 RTP/SAVP 102` with nothing else (file 043) | every m-line needs one `a=mid` and an `rtpmap` for non-static payload types | "inconsistent"; fill in mid (from our offer at that position), `inactive` and an rtpmap |
| `m=x-data … RTP/SAVP` | an unknown media type; with the protocol rewritten it would pass as an RTP m-line with an `x-data` codec | must be translated to `m=application … UDP/DTLS/SCTP` or removed (keeping m-line positions) |
| candidates with `… MTURNID {n}` at the end | the candidate parser takes only `generation`, `network-id`, `ufrag`, `network-cost` after `raddr`/`tcptype`, then needs the line end | the line silently becomes "unused": **the far end's UDP relay candidate is lost** unless the suffix is cut |
| `TCP-ACT` / `TCP-PASS` transports | `Protocol::try_from` knows `udp`, `tcp`, `ssltcp`, `tls` | the line silently becomes "unused"; fine (we would drop them anyway) |
| per-m-line ICE credentials (020, 029) | one set of remote credentials; a change looks like an ICE restart | keep only the bundle's first m-line's credentials and candidates; never feed a second set |
| component-2 candidates (020) | we use `rtcp-mux` | drop them |
| no `a=setup` in an offer (020) | warns and takes the **passive** role | the web app took **active**; write `a=setup:passive` into what str0m sees (str0m then goes active) or set the role directly |
| `a=setup:passive` in renegotiation *offers* | inverted to active, consistent with the first handshake | fine |
| `a=crypto` | ignored as an unknown attribute | fine; never answer with `a=crypto` |
| backslash extension URIs | unknown extensions, so abs-send-time and transport-cc are not negotiated | rewrite to the slash form if we want them; Microsoft's own extensions (`frame-counters`, `fast_bandwidth_feedback`) stay unknown |
| `a=rtcp-fb:* …` | `*` is not a payload type; the line becomes "unused" | fine; str0m sets its own feedback |
| `a=x-…`, `a=label`, `b=CT`, `a=x-mediabw` | unknown, ignored | fine on input; we must *write* `label` and `x-ssrc-range` ourselves |
| Opus `maxplaybackrate=16000` etc. | Opus is matched by name and clock | fine |
| H.264 `profile-level-id=42C02A` (constrained baseline with both constraint flags) | str0m matches H.264 by profile; check whether `42C0` matches our `42e0` | only matters for video; the browser answered `42e01f` and the far end took it |

And on the way out, str0m writes `UDP/TLS/RTP/SAVPF`, port 9 and
`c=IN IP4 0.0.0.0`, its own random mids, `a=ssrc` lines but no
`x-ssrc-range` or `label`, and slash URIs; each must be turned into D.1's
form. The huddle stack already rewrites str0m's mids for Chime
(`src/huddle_audio/sdp.rs`), so the idea is not new here, but the list
above is long enough that str0m's SDP API is the wrong tool (§F.2).

---

## E. What the huddle stack gives us

| Piece | Where | For Teams |
|---|---|---|
| Microphone, opened only while unmuted | `huddle_audio/microphone.rs` | as is |
| AEC3, noise suppression, AGC, resampling to 48 kHz mono | `huddle_audio/processing.rs` | as is |
| Opus encoder (20 ms, 48 kHz, FEC off) | `huddle_audio/encoder.rs` | as is; Teams offers `useinbandfec=1`, ours stays off (a receiver may still ask, it does not require it) |
| Framer, DTX, RTP time, audio level | `huddle_audio/uplink.rs` | as is |
| Jitter buffer | `huddle_audio/jitter.rs` | as is |
| Opus decoder and speaker, with the render tap for AEC | `huddle_audio/speaker.rs` | as is (the far end sends 16 kHz-band Opus at 48 kHz clock) |
| DTLS through OpenSSL | `huddle_audio/dtls.rs` | as is; whether Microsoft's stack takes str0m's dimpl is untested, OpenSSL is the safe start |
| TURN client (UDP, TCP, TLS; Allocate, CreatePermission, Send/Data) | `huddle_audio/turn.rs` | reusable; needs Microsoft's TURN servers and the `trap/tokens` credentials (§6.5 of microsoft-teams.md) and permissions for *every* remote candidate address, not one media server |
| H.264 decode to egui pixels | `huddle_audio/decode.rs`, `screen.rs`, `gallery.rs` | for video (stage 4) |
| Camera and H.264 encoder (`42e01f`) | `huddle_audio/camera.rs`, `camera_send.rs`, `video_encoder.rs` | for video (stage 5); Teams answers `42e01f` |
| The session loop (sockets, TURN, str0m, audio, video, counts) | `huddle_audio/media.rs` | the shape, not the code: it is bound to Chime's signalling and to a relay-only peer |
| Chime signalling, join, region, roster, video INDEX, simulcast choice | `chime.rs`, `signaling.rs`, `join.rs`, `region.rs`, `roster.rs`, `video.rs`, `cameras.rs`, `watch.rs` | not reusable |
| Probe CLI pattern (`--huddle-probe … --send-tone`) | `huddle_audio/probe.rs` | the pattern for a `--teams-call-probe` |
| Worker side: one session, mic open/close, speaker watchdog | `backend/listen.rs` | the pattern; parts may be shared |
| Call bar, mute button, roster faces | `ui/call_bar.rs`, `huddle_mic.rs` | reusable if `huddles::Listen` and the call bar stop assuming a Slack channel and a huddle |
| Call window (shares, tiles), its viewport | `ui/call_window.rs`, `app/call.rs` | for video |
| Incoming ring prompt | `people::Event::HuddleInvite` and its UI | the model for an incoming Teams call (answer and decline instead of join) |

What must be added:

1. **Trouter for calls.** Register for calling (the web app's app id and
   template for calls were not captured; ost registers `NextGenCalling`
   with `DesktopNgc_2.5:SkypeNgc`); decode gzip bodies; route
   `callAgent/{id}/…` pushes to the call session and the bare-`{surl}`
   `evt: 107` notification to the call starter; hand the session the
   current `surl` and `epid`. Today `epid` is new on every reconnect and
   the read loop in `backend/teams.rs` owns the socket, so a reconnect in
   the middle of a call loses its callbacks. Keep the epid stable across
   reconnects, and do not drop the socket while a call is up.
2. **A calling HTTP client**: `cpconv`, `attach`, `progress`, `accept`,
   renegotiation `answer`, `updateEndpointState`, `updateEndpointMetadata`,
   hang-up, reject, with the headers of §1.2 and the ids of §1.3; and
   `GET https://edge.skype.com/trap/tokens` for TURN credentials (secrets:
   redact in `Debug`, never log).
3. **The SDP dialect**: a reader that pulls out what we need from
   Microsoft's SDP (credentials, filtered candidates, fingerprint, setup,
   per m-line mid, label, port, direction, payload types) and a writer that
   makes D.1/D.4 from our state. Tested on the captured SDPs with
   identifiers and secrets replaced.
4. **A call session**: a pure state machine for the signalling (like
   `signaling.rs`'s `Handshake`), and a driver for sockets, TURN and str0m
   (like `media.rs`), with host candidates as well as the relay, since the
   far end is a peer and may be on the same network.
5. **Model, events and UI**: a Teams call is a conversation (the 1:1
   chat) rather than a Slack channel; states Calling (ringing out),
   Ringing (in), Connecting, Live, Ended(reason); a Call button in a
   Teams 1:1 conversation; the ring prompt; `Failure` variants for
   declined, busy, no answer, unreachable, and a refused SDP.

---

## F. Proposed implementation

### F.1 Module layout

```
src/teams/calling/              (feature "teams")
  mod.rs        what a call is; ids (§1.3)
  types.rs      serde types: cpconv request and answer, callNotification,
                attach, callProgress, callAcceptance (both ways),
                mediaAnswer, mediaNegotiation, mediaAcknowledgement,
                callEnd, conversationEnd, rosterUpdate, endpointState
  links.rs      callback URLs: building {surl}callAgent/{agent}/{tag}/…,
                and reading a push's path back into (agent, scope, event)
  api.rs        the HTTP calls of §A–C (on TeamsClient's reqwest client and
                token), plus trap/tokens
  sdp.rs        the Microsoft dialect: read (§D.2, D.3) and write (§D.1, D.4)
  handshake.rs  the signalling state machine: events in, steps out
                (send this request, apply this SDP, ring, report live, end)
  codes.rs      callEnd / conversationEnd / acknowledgement codes to Failure

src/teams/socket.rs             gzip bodies; call frames out as their own
                                TrouterEvent variant
src/backend/teams.rs            route call pushes; keep epid and the socket
                                stable during a call
src/backend/teams_call.rs       the worker side: one call at a time, like
                                backend/listen.rs (mic, speaker watchdog,
                                events to the interface)
src/huddle_audio/peer.rs (new)  or a small shared module: UDP socket + TURN +
                                str0m loop + audio pipeline, taken out of
                                media.rs so Chime and Teams both drive it
src/ui/                         call bar and ring prompt made call-generic;
                                a Call button for Teams 1:1 chats
```

Naming (`huddle_audio` for something Teams also uses) can wait until the
second user exists; a move is cheap once the shape is known.

### F.2 Driving str0m: the direct API, our own SDP

str0m's `DirectApi` (`rtc.direct_api()`) has what a Teams call needs
without its SDP parser: `set_ice_controlling`, `local_ice_credentials`,
`set_remote_ice_credentials`, `add_remote_candidate` (on `Rtc`),
`local_dtls_fingerprint`, `set_remote_fingerprint`, `start_dtls(active)`,
`declare_media(mid, kind)`, `declare_stream_tx`, `expect_stream_rx`,
`start_sctp` (later, for the data m-line if needed). The huddle stack
already uses the direct API in `watch.rs`.

So the session:

1. builds an `Rtc` with Opus (and later H.264) at the payload numbers the
   SDP uses: ours for an outgoing call (111, as the web app), the
   offerer's for an incoming one (102 in file 020), set in the codec
   config before the `Rtc` is made;
2. writes its offer or answer from a template of D.1/D.4, filling in
   mids `0…3` (or the offerer's names), labels, `x-ssrc-range` from the
   SSRCs it declared, credentials, fingerprint, candidates, and the relay
   candidate's address as the m-lines' port and `c=`;
3. reads each remote SDP with `teams::calling::sdp`, applying only what
   changed: credentials and candidates from the bundle's first m-line
   (MTURNID cut, TCP and component 2 dropped), the fingerprint, the
   directions (to start or stop sending), the payload types;
4. on a renegotiation, adds any new candidates and answers from the same
   template, without touching str0m's transport (no ICE restart unless
   the bundle's credentials really change).

The alternative, rewriting Microsoft's SDP into something str0m's SDP API
accepts and back (§D.5's table), is possible but has to stay in step with
str0m's mid and m-line bookkeeping through every renegotiation, including
an m-line that arrives with its own credentials (file 029). The direct
API keeps all of that in our hands, and the SDP code becomes plain
string work that is tested against the capture.

### F.3 Stage 1: an outgoing audio-only 1:1 call

Goal: from a Teams 1:1 chat, ring the other person, talk both ways, mute,
hang up, and see their hang-up. First as a probe, then in the app.

1. **Fixtures.** Copy the needed SDPs and JSON bodies from the capture
   into `src/teams/calling/fixtures/` with every identifier, address,
   credential and fingerprint replaced (§ notation). Parse tests for each
   type; SDP reader tests for D.2, D.3 and the renegotiation offers;
   writer tests that our offer has the shape of D.1.
2. **Trouter.** Gzip bodies; a `TrouterEvent::Call { path, body }`; stable
   epid; hand `surl` to the call. A test that a captured push path routes
   to (agent, scope, event).
3. **TURN.** `trap/tokens` for credentials; find the relay server
   addresses (§G) and allocate with `turn.rs` (UDP first, TCP/TLS as the
   huddle does when UDP is blocked).
4. **Offer.** An `Rtc` with Opus only, controlling, a host candidate from
   our UDP socket, the srflx address TURN's Allocate answer gives us, and
   the relay. The offer mirrors file 041 (the audio-only call), in this
   order of attempts, logging the server's answer to each:
   a. the four m-lines of file 041 with `main-video` and the share line
      `inactive`, `x-data` offered but not used;
   b. if (a) is refused for the data line, the same without `x-data`;
   c. audio alone, to learn whether the extra lines are required at all.
5. **Signalling.** `cpconv` (A.1); on `mediaAnswer` apply it (start ICE
   and DTLS); on `acceptance` mark live and keep `callLeg`; on
   `mediaRenegotiation` answer (C.1) within the time the web app takes;
   on `rosterUpdate` keep names and mute state; on `end` and
   `conversationEnd` stop.
6. **Media.** Reuse microphone, processing, encoder, uplink, jitter and
   speaker exactly as `listen.rs` wires them; Opus silence while muted.
7. **Mute and hang-up.** `updateEndpointState` with a rising sequence
   number; hang-up as C.5 (record it from the web app first).
8. **Probe.** `noslacking --teams-call-probe {MRI} [--seconds N] [--send-tone]`,
   in the shape of `--huddle-probe`: signs in from the keyring, calls,
   logs every step at info level (never tokens, ICE passwords, TURN
   credentials or callback URLs), sends a tone, hangs up, ends with a
   summary and the step that failed. Try it against the user's own second
   account, on the same LAN and then from another network.
9. **App.** A Call button on Teams 1:1 conversations; the call bar shows
   the Teams call (Calling…, Live, mute, Hang up); failures through `t`.

### F.4 Later stages

- **Stage 2, incoming calls.** Calling registration on Trouter; the
  `evt: 107` notification (decode `gp`); attach, progress, the ring prompt
  (answer, decline) with a sound; accept with an answer built from file
  020's offer (Opus at the offerer's number, `setup:active`, BUNDLE with
  one transport, every other codec dropped); decline (record it first);
  `call/end` while ringing (caller gave up, answered elsewhere).
- **Stage 3, robustness.** Trouter reconnects during a call; the 45-minute
  keep-alive; codes to failures; a peer behind a different NAT (relay
  paths, TURN over TCP/TLS); our network changing; the 60-minute
  `callLimits` of the free plan (shown as the call ending, not an error).
- **Stage 4, receiving video.** Accept video in `callModalities`;
  `main-video` `recvonly`/`sendrecv`; H.264 decode into the call window;
  keyframe requests (PLI/FIR, as `watch.rs` does); following the far
  end's bandwidth renegotiations (`max-fs` starts at 240 macroblocks).
- **Stage 5, sending the camera.** The huddle camera pipeline at the
  answered profile and `max-fs`/`max-mbps`; `controlVideoStreaming` pushes
  (not seen yet) probably ask for a resolution.
- **Stages 4 and 5 as built** (`src/teams/calling/video.rs`). A 1:1
  call's video is the SDP alone: the source requests and data-channel
  messages of the web client's calling bundle (`sr`,
  `controlVideoStreaming`, the `main-channel` handshake) belong to its
  server-mixed path and never appear in a 1:1 capture. So the camera line
  is kept `sendrecv` from the first SDP of the call to the last, and our
  camera sends only while it is on: turning it on needs no
  renegotiation of ours, which was never captured (`POST
  {mediaRenegotiation}` with a `mediaNegotiation` body, glare handling).
  The far end renegotiates when its own camera starts (`recvonly`
  dropped, `callModalities` gaining `video`), which the call answers. Its
  video, like its audio, names no SSRC we can use and is learnt from the
  first packet at the line's H.264 payload type; a camera quiet for 3 s
  counts as off. The picture is decoded by the video helper into the
  huddle's `Gallery`; ours comes from the huddle's camera and encoder.
  H.264 is registered with str0m at the call's one number (ours, 108, in
  an offer; the offerer's in an answer), which the native client keeps
  through its renegotiations. Lost packets are asked for again (`nack`,
  which str0m sends for every incoming stream anyway) and come on the
  `rtx` payload type (FID, the far end's SSRC plus 50), paired with its
  video once both have arrived; without that pairing every loss cost a
  keyframe, and each keyframe a new decoder in the helper, which made the
  far end's camera stutter on a lossy path. Open: the native client's first offer limits what we send to
  `max-fs=240` (about 320×192) and raises it later; our camera sends
  640×480 regardless.
- **Stage 6, screen share and group calls.** `applicationsharing-video`
  and the conversation-level links (`addParticipant`, the broker) are out
  of scope until 1:1 works.

---

## G. Not in the capture

Each of these should be recorded from the web app (Chrome, with
WebSocket frames) before the code that depends on it is written:

1. ~~Our own hang-up~~ and ~~declining~~: recorded, see C.5. Letting an
   incoming call ring out is still unrecorded (B.7).
2. (see 1)
3. **Calls that fail**: declined, busy, unanswered, offline; the codes in
   `call/end` and how the progress callback reports ringing to us.
4. **The Trouter registration** the web app makes for calls (app id,
   template key, path), and the raw form of a gzip body and of `gp`.
5. **Acknowledgements**: whether the web app POSTs
   `mediaAnswer.links.mediaAcknowledgement` and
   `callAcceptance.links.acknowledgement`, or the Trouter delivery reply is
   the acknowledgement. Nothing in the 50 files does either.
6. **TURN servers**: the hosts and ports of Microsoft's relays (the
   `trap/tokens` answer holds credentials and a realm; where the server
   list comes from, perhaps the client configuration behind `ecsEtag`, is
   unknown).
7. **The keep-alive** after `callKeepAliveInterval` (2700 s).
8. **`updateEndpointState` sequence 2**, missing between files 000 and 016.
9. **A call between networks**, to see which candidate pair wins when the
   two ends are not on one LAN, and whether relay-to-relay works with a
   standard TURN client.
10. **What `progress` carries** for an outgoing call (the callback was
    given in file 000 but nothing arrived on it during ringing).

**Found since:** the TURN servers are in the Skype client configuration,
`GET https://config.teams.microsoft.com/config/v1/Skype/1415_1.0.0.0`
(personal web client, Europe): `Turn = {"addresses":
["gateway-eu.az.relay.teams.cloud.microsoft"], "realm": "rtcmedia",
"udpPort": 3478, "tcpPort": 443, "tlsPort": 443}`, with the credentials
from `trap/tokens`. The same file names a `DedicatedRelay`
(`dr-eu.skype-cr.akadns.net`, ports 50000–50007), not needed to start.

## H. Meetings (recorded: `teams.live.com5.har` to `teams.live.com8.har`)

Personal (Teams free) meetings, from the web client. Four sessions: Meet
now as organizer, alone (5) and admitting someone from the lobby (6, 8);
joining someone else's meeting by ID and passcode (5) and by link (7),
waiting in the lobby, being let in, leaving. 7 and 8 have the Trouter
traffic. Every request carries `x-skypetoken` and the `x-microsoft-skype-*`
headers of §1.2, as a 1:1 call's do; the callback links are made the same
way (§1.4), under the call's agent id.

### H.1 Meet now

`POST https://teams.live.com/api/mt/beta/me/calendarEvents/privateMeeting/schedulingService/create`
with `x-skypetoken`, `x-ms-client-type: web`, the web client's origin and
referer, and (left out of the recording, which was exported without
`Authorization` headers, but refused with 401 without it) the middle
tier's bearer token (`https://mtsvc.fl.teams.microsoft.com`), and
`{"meetingType":"MeetNow","isStreamEnabled":false,"subject":"Meeting with {name}","unhideChatThread":true}`.
Answered 201: `value.links.join` is the meeting link,
`https://teams.live.com/meet/{13 digits}?p={token}`, and
`value.groupContext.threadId` its chat (`19:meeting_…@thread.v2`). The
link is then joined as anyone's (H.2), and is what others are invited
with.

### H.2 Finding the meeting: the "preheat"

`POST {fp}/cpconv` without `callInvitation`:

- `meetingData`: `{meetingCode, passcode, meetingUrl}`. From a link, the
  code is its path's digits and the passcode its `p=` token. By ID, the
  code is the ID typed (spaces dropped) and the passcode the one typed,
  and `meetingUrl` is the link that would have had it:
  `https://teams.live.com/meet/{id}?p={passcode}`.
- `meetingPreferences: {shouldResurrect: "resurrect"}`, `groupContext`,
  `groupChat` and `meetingInfo` null.
- `conversationRequest` with the joining callbacks (`conversationEnd`,
  `conversationUpdate`, `localParticipantUpdate`,
  `addParticipantSuccess/Failure`, `receiveMessage`) and the roster
  callback.
- `endpointState`: sequence 0, `additionalEndpointProperties:
  {infoShownInReportMode: "FullInformation"}`.
- `participants`: `from` only.

The answer is the conversation: `conversationController`, `links`
(`leave`, `updateEndpointState`, `updateEndpointMetadata`, `subscribe`,
…), `state.conversationType: "scheduledMeeting"`, `meetingDetails`, and
`meetingData` with the meeting's own passcode (6 characters) in place of
a link's token.

### H.3 Joining

`POST {conversationController}` with the preheat's body plus:

- `meetingData` exactly as the preheat answered it.
- `conversationRequest.suppressDialout: true`, and the in-call callbacks
  (adding `addModalitySuccess/Failure`, `confirmUnmute`).
- `participants.to: []`.
- `endpointState.endpointProperties.preheatProperties: 1`.
- `callInvitation`: as a 1:1 call's (A.1), `callModalities` `["Audio",
  "ScreenViewer"]`, 13 m-lines in the web client's offer, plus
  `mediaDescriptions` (the receive-only video lines) and
  `applyChannelParameters` (a bandwidth seed). We send our own offer
  without those two; whether the meeting wants them is not known yet.

Then `POST {updateEndpointState}` with
`{from, endpointState: {endpointStateSequenceNumber: 1, endpointProperties: {preheatProperties: 0}}}`.

### H.4 The lobby, and being let in

- The meeting answers with a `call/acceptance` push, as a callee would.
  - For the organizer it comes from the call.
  - For anyone it keeps waiting, it says `controllerName: "lobby"` and
    `mediaContent.callLabel: "lobby"`. It carries an answer from a lobby
    media server, which mutes you.
- The web client acknowledges the acceptance in its Trouter reply's body
  (`callAcceptanceAcknowledgement` with its links). We post the same to
  the `acknowledgement` link, as for a 1:1 call.
- While you wait, the roster lists you with a `lobby` endpoint (H.5). The
  counts it gives are all zero, and you do not see the organizer.
- **Being let in** comes in four steps:
  1. A `conversation/conversationUpdate`: `activeModalities.call` set,
     `lobby` null, the meeting's chat in `activeModalities.groupChat`, and
     the links of one in the call, `admit` among them.
  2. A `call/mediaRenegotiation` (`originator: "mcGvc"`, `newOffer: true`)
     from the meeting's own media server. It has **a new DTLS certificate
     and new ICE credentials**, and is written in the older dialect
     (`application/sdp-ngc-0.5`):
     - every line `RTP/AVP` with `a=crypto` offers and a fingerprint, and
       no `a=setup`;
     - an ICE session per kind of line (audio, video, share, data);
     - Opus at 102 among 17 audio codecs;
     - the mids renumbered (`BUNDLE 1 2 5 … 13 3 4`).
  3. The web client answers on a **new transport**: new ufrag, certificate
     and port, DTLS `active`, `RTP/SAVP`, line for line.
  4. A `call/mediaAcknowledgement` (sub-code 10109, "Participant retarget
     was successful") carrying a whole new call leg's `links` (`callLeg`,
     `mediaRenegotiation`, …), which are used from then on.
- About six seconds later a second renegotiation (`newOffer: false`,
  `ngc-1.0`) offers the same server again in BUNDLE, on the audio line's
  credentials. It is answered as usual.

We do the same: an offer whose certificate differs from the far end's
last one moves the call. The media session hands its speaker, microphone
and video over to a new session, which answers the offer. The
acknowledgement's links then replace the old leg's.

### H.5 Who is waiting, and admitting

- In `rosterUpdate`, a participant waiting in the lobby has an endpoint
  with `lobby: {mediaStreams}` and no `call`, `modalityJoined: "Lobby"`,
  `role: "guest"`, and `participantCounts.lobbyParticipants` counts them.
- Once in, the endpoint has `call: {mediaStreams, serverMuteVersion}`,
  `modalityJoined: "Lobby,Call"`, and `role: "admin"` (in a personal
  meeting everyone admitted is one).
- A participant who left is `state: "inactive"` with no endpoints.
- To admit someone, `POST {links.admit}` (from the conversation update)
  with:

  ```json
  {"participants": {"from": me, "to": [{"id": mri}]},
   "links": {"admitFailure": cb, "admitSuccess": cb},
   "debugContent": {"causeId": uuid}}
  ```

  It is answered 202, then a `conversation/admitParticipantSuccess` push
  (`participants`, `participantInfos`) and the roster with them in the
  call.

### H.6 Leaving

`POST {leave}` with the 1:1 call's body (C.5, connected), answered 204,
then a `call/end` push (`CallEndReasonLocalUserInitiated`). No
`conversationEnd` came.

### H.7 Not known yet

- Whether the meeting takes our 4-line offer, or needs the web client's
  13 lines and `mediaDescriptions` before it sends video.
- How a wrong link or passcode is refused (we take a 4xx to the preheat
  as "no such meeting").
- Joining a work account's meeting: its links in the old form
  (`/l/meetup-join/…`) carry the thread and organizer instead, and were
  not recorded.
- Whether the second renegotiation always comes, and whether the new
  leg's keep-alive matters within a meeting's length.

### H.8 Video in a meeting (recorded: `teams.live.com5.har`, camera on)

A meeting's media server does not go by the SDP's directions for video.
It goes by `mediaDescriptions`, which say what each video line is used
for:

- **On joining** they travel in the `callInvitation`'s `mediaContent`,
  as `{descriptions: [{mid, direction: "recvonly"}, …], requestId: 1}`.
  They list every camera line and the share line, as receiving.
- **In every answer** they come again, `requestId` rising.
- **When the camera goes on**, two requests follow:
  1. `POST {applyChannelParameters}` (a call leg link) with
     `{applyChannelParameters: {multiChannelParameter: {mids: [camera mid],
     mediaParameter: "{\"maxVideoSendCapabilities\":{\"caps\":{…}}}"}}}`,
     answered 202.
  2. `POST {updateMediaDescriptions}` with
     `{UpdateMediaDescriptions: {mediaDescriptions: {descriptions: [{mid,
     direction: "sendrecv", label: "main-video"}, …], negotiationTag:
     "{participantId};v_{n}", requestId}}}`, answered 200.

Without them our camera was sent, but no one saw it, and no one's camera
reached us. We now send them as the web client does: receive-only on
joining, and the camera's line sending and receiving while it is on. How
the server says whose camera comes on a line (`csrcInfo`) has not been
recorded yet.

A guest who is not signed in has a `8:teamsvisitor:…` MRI. Admitting
someone sends the MRI the roster listed them by.

The meeting's media server answers the camera line with H.264 at payload
type 107 (resends 99) whatever was offered: we offered 108 and it
answered 107, so its video came at a number our line did not know, and
ours went at one it did not take. We now offer 107 and 99 when joining a
meeting; 1:1 calls keep 108.

**Receiving someone's camera (`teams.live.com9.har`).** The web client
joined; another web client joined, was let in, and turned its camera on
a few seconds later; the web client then showed it. Between the camera
going on (the roster's `main-video` stream turning `sendrecv`) and its
picture there was **no request and no push about video at all**: only
roster updates. So the web client asks for someone's video in-band. Its
web clients' roster endpoints list a `data` stream (`sendrecv`) and an
`applicationsharing` one; ours lists neither. The data line is an SCTP
data channel in the bundle (`m=x-data … RTP/SAVP 127 126`,
`a=x-data-protocol:sctp`, `a=sctp-port:5000`,
`a=max-message-size:262144`, label `data`), offered `actpass` and
answered `active`. What it carries is not in a HAR; the next step is a
log of its messages.

### H.9 The meeting's data channel (recorded: a log of its messages)

The web client creates one data channel, `main-channel` (ordered,
reliable, negotiated in-band), on the data line. Each message is a
16-byte header and a JSON array:

```
10 0f 92 00 | seq (u16, little-endian, each way from 0) |
from (i32, big-endian) 01 | to (i32, big-endian) 01 | [ {...} ]
```

The server is -4. The client is -2 until the server's `ack` arrives; the
`ack` is addressed to the id the client then sends from (415 in the log).

| who | message |
|---|---|
| client | `{"type":"syn","client_capabilities":["dsh","bwe","sr","ssbwe"]}` |
| server | `{"type":"ack","receive_capabilities":["sr","ssbwe","leave","heartbeat"],"send_capabilities":["bwe","dsh","sr_res","heartbeat","vdclc"]}` |
| server, every second | `{"type":"bwe","bw":613464,"video_bw":493317}` |
| server | `{"type":"dsh","history":[403]}`: who spoke, by audio source id |
| client | `{"type":"sr","controlVideoStreaming":{"sequenceNumber":1,"controlInfo":{"sourceId":202,"streamMsid":404,"fmtParams":[{"max-fs":8160,"max-mbps":244800,"max-fps":3000,"profile-level-id":"64001f"}]}}}` |
| server | `{"type":"sr_res","result":"ok","sequenceNumber":1}` |

- An `sr` (source request) asks for one participant's video.
  - `sourceId` is the roster `sourceId` of their `main-video` stream.
    That stream turns `sendrecv` when their camera goes on.
  - `streamMsid` is the `x-source-streamid` the meeting's SDP gives the
    receiving video line.
  - `sourceId: -1` asks for none.
- Each `sr` has an odd `sequenceNumber`, two above the last; the client
  sent 1, 3, 5 and 7 as the other camera went on, off and on again.
- Nothing about video goes over HTTP.

We do the same:
- open the channel on the data line in meetings, and send `syn`;
- once the `ack` is in, ask for the first participant whose camera is
  on, on our camera line's stream;
- ask again whenever the roster changes who that is, and `-1` when no
  one's is on.
