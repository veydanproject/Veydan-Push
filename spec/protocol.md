# VPush protocol, version 1

For the one who writes a client. This file describes what the server does
**today**; each stage adds its part here when it is built, not before.

All bodies are JSON. The types are in the crate `vpush-proto`, and examples,
word for word, in [golden/](golden/): a client's tests should read a copy of
that folder.

## Every answer

Header `X-Request-Id`: the id of the request in the server's log. Show it to
the user next to an error, so that the operator can find the request.

## Errors

Every answer that is not 2xx has this body:

```json
{ "error": { "code": "not_found", "message": "no such route" },
  "request_id": "5f3a9c1e" }
```

`code` is for the program, `message` is for a person and may change.

| Code | HTTP | Meaning |
|---|---|---|
| `bad_request` | 400 | The request cannot be read; `message` says what is wrong |
| `auth_missing` | 401 | No `Authorization: Nostr …` header |
| `auth_invalid` | 401 | The header is not a signed kind 27235 event, or it was signed for another method or another body |
| `auth_expired` | 401 | The event is more than 60 seconds from the server's clock. `error.server_time` carries that clock, unix seconds |
| `auth_replay` | 401 | This event was used already |
| `auth_url_mismatch` | 401 | The event was signed for another address |
| `not_found` | 404 | No such route, or no such device of this owner |
| `payload_too_large` | 413 | The body is above `limits.max_body_bytes` |
| `unknown_app` | 422 | The server does not serve this `app_id` |
| `provider_disabled` | 422 | The server serves the app, but not through this push service |
| `token_invalid` | 422 | The push service does not know the token, or it is of another project. Nothing was registered. Ask the push service for a new token and register again |
| `limit_devices`, `limit_relays`, `limit_groups` | 422 | More than the server allows; the numbers are in `/v1/info` |
| `limit_devices_total` | 422 | The server holds as many devices as it takes. A device that is registered renews as before; a new one may find room later |
| `rate_limited` | 429 | Too often; header `Retry-After` says after how many seconds to come back |
| `internal` | 500 | The server failed; the request may be repeated |

A request that runs over the server's time limit is answered with a bare 504,
without this body.

## Who is asking: NIP-98

Every request to `/v1/devices/…` carries

```
Authorization: Nostr <base64 of a signed event>
```

The event:

| Field | Value |
|---|---|
| `kind` | 27235 |
| `created_at` | now; the server accepts 60 seconds either way |
| `content` | empty |
| tag `u` | the full address of the request, exactly: `https://vpush.veydan.net/v1/devices/phone-0001` |
| tag `method` | `PUT`, `GET`, `DELETE`, `POST` |
| tag `payload` | sha256 of the body, hex. Required when there is a body |
| tag `nonce` | any random string, a new one for every request |

The key that signed the event is the owner of the device. There is no other
way to name an owner, so nobody can register, read or remove a device of
another: to everybody else the device does not exist (404).

Three things go wrong most often:

- **The address.** `u` is compared as text. Take the server's address without
  a trailing slash and add the path to it. A query is a part of the address.
- **The nonce.** An event is accepted once. Two requests of the same second,
  to the same address, with the same body are the same event without a
  nonce, and the second is refused with `auth_replay`.
- **The clock.** On `auth_expired`, take `server_time` from the answer, sign
  again with the clock corrected by the difference, and repeat once.

base64 is the standard alphabet, with padding.

## Before registering

```
GET /v1/info
```

No authorization.

```json
{ "version": "0.3.0",
  "apps": [ { "id": "net.veydan.mobile", "providers": ["fcm"] } ],
  "relays": { "policy": "allow_list",
              "allowed": ["wss://node-1.veydan.net", "wss://nos.lol"] },
  "limits": { "devices_per_pubkey": 10, "relays_per_device": 16,
              "groups_per_device": 500, "registration_days": 30,
              "test_per_hour": 3, "max_body_bytes": 65536 } }
```

With the policy `allow_list` the server watches only the relays it names.

## Registering a device

```
PUT /v1/devices/{device_id}
```

`device_id` is chosen by the client: 8 to 64 letters, digits, `_` or `-`. One
per identity on a device, kept for as long as the identity lives there.

The body is everything about the device, every time
([golden/device_put.json](golden/device_put.json)). What is not named is no
longer watched.

| Field | | Meaning |
|---|---|---|
| `app_id` | required | The Android package or the iOS bundle id |
| `channel` | required | `{ "provider": "fcm", "token": "…" }` |
| `app_version` | | For the operator's eyes |
| `prefs` | | `{ "dm": true, "groups": true }`; both true when absent |
| `author_key` | | 64 hex. With it the server does not push a user's own group message back to the user. See [the mark of the author](#vp-16-hex-the-mark-of-the-author) |
| `relays` | | `[{ "url": "wss://…", "dm": true, "groups": true }]`: where to watch, and for what |
| `groups` | | `[{ "id": "<64 hex>", "keys": ["<64 hex>", …] }]`: the user's groups, each with the push keys the device holds for it; see below |

The server learns nothing else about the user: not the language, not what
the user calls a group. What a push says is the device's to write.

### The groups and their push keys

A group is named by its id and by one or two **push keys**:

```
push_key = hex(HMAC-SHA256(key = the key of the group, msg = "veydan-push-group-v1"))
```

One, or two for seven days after the key of the group was changed: the
push key of the key of now, and of the one before it. After those days the
app registers anew with the key of now alone. The key of the group itself
is never sent here.

The server pushes an event of a group to a device only when the event
carries the mark of a push key that device registered (see
[the mark of the group](#gp-16-hex-the-mark-of-the-group)). A group
registered with a key that is not the group's is a group the device is
told nothing of.

| What is wrong | Answer |
|---|---|
| A group that is not `{ "id", "keys" }`: an id alone, as an app older than 0.3.0 sent it; no `keys` | 400 `bad_request` |
| An id or a key that is not 64 hex characters, lower case | 400 `bad_request` |
| No key, or more than two | 400 `bad_request` |
| The same id twice; the same key twice in one group | 400 `bad_request` |
| More groups than `groups_per_device` | 422 `limit_groups` |

Nothing of a registration that is refused is kept.

The answer ([golden/device_answer.json](golden/device_answer.json)):

```json
{ "device_id": "phone-0001", "expires_at": 1792592000,
  "relays": [
    { "url": "wss://node-1.veydan.net", "status": "pending" },
    { "url": "wss://relay.example.org", "status": "not_allowed",
      "detail": "this server does not watch this relay" } ] }
```

A relay the server refuses does not fail the registration: it is answered
about, and left out. Show the statuses to the user.

| Status | Meaning |
|---|---|
| `ok` | Connected and watching |
| `pending` | Accepted; not connected yet |
| `not_allowed` | The server does not watch this relay |
| `invalid` | Not an address of a relay: `detail` says why |
| `restricted` | The relay does not let the server read |
| `unreachable` | The relay cannot be reached |

`url` in the answer is the address as the server writes it: lower case, no
trailing slash. An address with a query is refused: a key of a relay must
never be sent here.

**When to register again:**

- the push service gave a new token;
- the relays, the groups or the preferences changed;
- the key of a group changed: the group has another push key now;
- seven days passed since the last time. A registration nobody renewed for
  `registration_days` is forgotten.

Registering the same token under another owner removes the device of the
former owner: one address at a push service is one device.

### The token is checked

Before a device is registered the server asks the push service whether the
token is one it can push to. Nothing is sent to the phone for that. A token
the push service does not know is refused with `token_invalid`, and nothing
is kept: not the device, not what it asked to be watched.

The question is asked for a new device, for a new token of a known one, and
for a device in the state `dead_token`. So a device that was marked dead
comes back by registering the token it has, when the push service vouches
for it. The registration that renews a token the push service has vouched
for asks nobody.

A push service that cannot be asked at the moment does not hold a
registration back: the device is registered, and the first push to it says
what the token is worth.

### How often

| What | How often |
|---|---|
| Registrations of one owner | 10 in a minute |
| New devices from one network address | 20 in an hour |

More than that is answered with `rate_limited` and `Retry-After`. A new
device is a `device_id` the owner has not registered, or another token for
one that is; renewing a device as it is does not count. The second limit is
there only when the server is told whom a request came from by the proxy in
front of it.

Register when there is a reason to (see above), not on a timer of seconds:
an app that registers on every start and every change stays far below ten
a minute.

## Reading and removing

```
GET    /v1/devices/{device_id}
DELETE /v1/devices/{device_id}
```

`GET` answers with what is kept: the state (`active`, or `dead_token` when
the push service said the token is gone), the relays with their statuses, the
ids of the groups, and what became of the last push (`last_outcome`,
`last_push_at`). The token is not given back, and neither are the push keys
of the groups: no answer of the server says one.

`DELETE` answers 204, also when there was nothing to remove. Remove the
device when the user turns pushes off, and before an identity is deleted:
afterwards there is no key to sign the request with.

## Does a push arrive

```
POST /v1/devices/{device_id}/test
```

Sends one push of the type `test`, which the device shows in its own words,
and answers with what the push service said:

```json
{ "outcome": "delivered", "trace": "5f3a9c1e" }
```

| Outcome | Meaning |
|---|---|
| `delivered` | The push service took it. If nothing shows up, the cause is on the device |
| `dead_token` | The push service says the token is gone, or is none it can push to. Ask it for a new one and register again |
| `rejected` | The push service refused; repeating will not help |
| `retry` | The push service is busy, cannot be reached, or did not answer in ten seconds |

`trace` is also in the push, so the two can be matched. No more than
`test_per_hour` of these an hour for a device. They are counted by the
token: the same token under another `device_id` is the same device.

## What arrives on the device

The same through every provider. Through FCM it arrives as the `data` of the
message, where every value is a string; the keys are the same.

```json
{ "v": 2, "type": "dm", "event": "{\"id\":\"…\",\"kind\":1059,…}",
  "trace": "5f3a9c1e" }
```

The push carries no text. The server does not know what the message says
or who wrote it, and it does not guess: the device opens the event, and
writes the notification in its own words and its own language. A `test`
push is shown the same way, with a line of the device's own.

| Key | Present | Meaning |
|---|---|---|
| `v` | always | Version of this format, `2`. On a higher one the app should ask for an update instead of guessing |
| `type` | always | `dm`, `group`, `sync`, `test`, `broadcast`, `manifest_update` |
| `event` | `dm`, `group`, when it fits | The event itself, as a JSON string, exactly as the relay gave it |
| `event_id` | `dm`, `group`, when `event` is absent | Id of the event the push is about |
| `relay` | with `event_id` | The relay the event was seen on: take the event there by its id |
| `group_id` | `group` | |
| `count` | when above one; always in `sync` | How many events this push stands for |
| `data` | service pushes | A JSON string; its meaning depends on `type` |
| `trace` | always | Id of this push in the server's log |

### The event, or where to find it

A `dm` or `group` push comes in one of two forms, never both:

- **with `event`**: the push is about one event, and the event itself is
  inside. The device opens it without a network. `event_id` and `relay` are
  not sent: they are in the event, and bytes are short.
- **with `event_id` and `relay`**: the device asks the relay for the event
  by its id. This is the form when the event does not fit, and always when
  `count` is present: a push for several events names the last of them,
  since the device cannot show them all from one.

An event fits when the string form of the whole push, every key and every
value added up, is at most 3900 bytes: FCM refuses a `data` map above 4096,
and the rest is room for what the transport adds. A direct message of a
few hundred characters fits; one with a long description of a file may
not. When FCM refuses a push with the event all the same, the server sends
it once more in the other form.

There is never a `notification` part in an FCM message. The app shows
notifications itself, from `data`; this is what keeps one message from
producing two notifications.

### By type

| `type` | Sent when | Carries | Shown |
|---|---|---|---|
| `dm` | A sealed direct message (kind 1059) for the owner appeared on a watched relay | `event`, or `event_id` and `relay`; `count` | yes |
| `group` | An event of a watched group (kind 9) appeared, with the mark of a push key the device registered for the group | the same, and `group_id` | yes |
| `sync` | More came for the device than it is pushed about one by one | `count`, and nothing of any one event | yes |
| `test` | The owner or the operator asked | nothing but `trace` | yes |

`broadcast` and `manifest_update` are not sent yet. `manifest_update` is
the silent one: the device handles it and shows nothing.

`count` is present when the push stands for more than one event: the device
was woken about this chat a moment ago, and what came since was counted.
For direct messages the count is of all senders together.

### One notification per chat

A push about a chat replaces the one before it. Tell chats apart the way the
server does: all direct messages are one chat, since the server cannot tell
senders apart, and every group is a chat of its own.

### Thirty pushes a minute, and one that counts the rest

A chat is pushed about once in a while (see `count` above), and a device
has many chats. So a device is also pushed to no more than 30 times in a
minute, whatever the pushes are about. What comes over that is not pushed
one by one. It is counted, and a minute after the first of it one push of
the type `sync` says how many there were:

```json
{ "v": 2, "type": "sync", "count": 10, "trace": "5f3a9c1e" }
```

`count` is the number of events the device was not pushed about since the
`sync` push before this one, or since the thirty ran out: every event that
was held back is in the count of exactly one `sync` push. There is no
`event`, no `event_id`, no `relay` and no `group_id`: the push is about
no one of them.

The device shows it, in its own words, as "10 new messages", and opens the
list of chats; what the messages are it takes from its relays, as it does
when it is opened. A `sync` push replaces the one before it, like a push
about a chat. The number is what came over the limit, not everything that
is unread.

The numbers are the server's (`pipeline.device_cap`,
`pipeline.device_window_secs`); 30 and a minute are what they are unless
its operator says otherwise. At most one `sync` push is sent in that time.

## What an app writes on its events for the push server

The server sees an event as a relay sees it: sealed. Three tags on the
**outside** of an event tell it what it cannot find out.

### `["silent", "1"]`

On a gift wrap (kind 1059) or a group event (kind 9): nobody is to be woken
for this. An app puts it on

- the copy of a message it keeps for the author's other devices;
- signals between apps: accepting and blocking, edits and deletions, keys;
- what serves a group and is not a message: operations, chains of keys.

Without the tag every one of these wakes somebody with "New message".

A relay learns from the tag that the event is not a message. It does not
learn which of the things above it is.

### `["vp", "<16 hex>"]`: the mark of the author

On every group event. A group event is signed by a key made for that one
event, so nobody outside the group can tell who wrote it, the push server
included: it would push a user's own message back to the user.

```
k    = HMAC-SHA256(key = "veydan-push-author-v1", msg = the author's secret key)
mark = hex(HMAC-SHA256(key = k, msg = the public key that signed the event))[0..16]
```

`k` is what a device gives the server as `author_key` when it registers.
For every device a group event would be pushed to, the server makes the
mark with the device's `k`; when it is the mark on the event, the device
belongs to the author, and is not told.

`k` cannot be turned back into the author's key, and signs nothing. To
whoever does not hold it, a mark is a random string; every group event has
one, and no two are alike. What the push server learns: which events of a
group its user wrote.

A value to check an implementation against, computed apart from the server
and the messenger (with openssl): the secret key `01` thirty-two times and
an event signed by the public key `02` thirty-two times give

```
k    = 48d060b886c7db36283c9e657795adb5f2dc5c7886eadec18610b91b11b9c8e8
mark = 20526ea0e3c8f745
```

### `["gp", "<16 hex>"]`: the mark of the group

On every group event. The id of a group is no secret: it is in the `h` tag
of every event of the group, in the open, and anybody can sign an event
that names it. Without a mark, a stranger who knows the id would wake every
member of the group, and have the push service carry their events to them.

```
push_key = HMAC-SHA256(key = the key of the group, msg = "veydan-push-group-v1")
mark     = hex(HMAC-SHA256(key = push_key, msg = the public key that signed the event))[0..16]
```

The mark is made as the mark of the author is, with the push key in the
place of `k`. `push_key` is what a device gives the server for the group
when it registers.

An event carries one `gp` tag. For seven days after the key of the group
was changed it carries two: the mark of the new key and the mark of the
one before, for the phones that slept through the change and have
registered the key before only.

What the server does with an event of a watched group:

- it reads the first four `gp` tags, and passes over what is not 16 hex
  characters, lower case;
- it makes the mark of every push key registered for the group, and pushes
  the event to the devices that registered a key whose mark is on the
  event. A device of the group without such a key is not told;
- an event with no mark, or with the mark of no key registered for the
  group, is pushed to nobody.

The mark of the author applies after that: of the devices the event would
be pushed to, the author's are left out. `silent` applies before: a silent
event is pushed to nobody, marked or not.

An event is held against 64 push keys of its group at most, those
registered by the most devices first. A group has one or two in use; the
rest are keys of devices that slept through several changes, and keys
somebody made up.

**What the server learns from a push key**: that an event came from
somebody who holds a key of the group that its user holds too. Nothing of
what the event says, and not who wrote it: every member makes the same
mark. A push key cannot be turned back into the key of the group, opens no
event and signs nothing. To whoever does not hold it a mark is a random
string, and no two events have the same one.

A value to check an implementation against, computed with openssl: the key
of the group `03` thirty-two times and an event signed by the public key
`02` thirty-two times give

```
push_key = c960ddf616adb9a57ac7d0eb04da5e25cf3d139c95d5828addb8f1f0ed50da28
mark     = 86a8b9acabf6941d
```

## What a push server cannot do

- **Relays that give a sealed message to its recipient only.** Some relays
  answer a request for the direct messages of a key with `auth-required`
  unless the one who asks is that key. A push server is not. Such a relay
  gets the status `restricted`, with what the relay said in `detail`; the
  messages that arrive there bring no push. `relay.damus.io` is one.
- **Who wrote a direct message.** So a contact the user muted or blocked
  wakes the phone like any other. It is the device that opens the event,
  sees who wrote it, and shows the message or keeps quiet.
- **Messages of the user's own key from another app**, which puts no
  `silent` on its copies.

### What it holds back, and cannot stop

The limits of the server make these cost something. They do not make them
impossible, and nothing the server could do would.

- **Anybody can make a phone be pushed to with a direct message.** Anybody
  may write to anybody: a sealed message to a user is an event any key can
  sign, and the server cannot tell a friend's from a stranger's. What holds
  it back: a device is woken about direct messages once in a while and the
  rest is counted (see `count`), it is pushed to no more than 30 times a
  minute whatever comes, and the phone stays silent for the second message
  of a stranger.
- **Somebody who left a group can mark its events until its key is
  changed, and for a while after.** The mark of the group is made from the
  key of the group, and whoever left knows that key until it is changed.
  For seven days after a change events carry the mark of the key before as
  well, for the phones that slept through the change. So a former member
  can still make marks the server takes during those days, and after them
  for the phones that have not registered since: the server knows those
  by the key before only.
- **Whether a token is a phone.** The push service says whether a token is
  one it can push to (`validate_only`), not whether anybody holds the
  phone: a token can be had by a script. The check makes a made-up
  registration cost something; it does not make one impossible. That there
  are not too many of them is what the limits on registering are for.
