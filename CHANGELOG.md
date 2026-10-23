# Changelog

Versions of VPush are its own and do not follow the app. Tags are
`v<version>`.

## 0.5.1 — tokio 1.54.1, ctutils 0.4.5

- `tokio` 1.54.1 (RUSTSEC-2026-0386: the constructors of `AsyncFd` that took
  any raw descriptor violate I/O safety; neither VPush nor vguard builds an
  `AsyncFd`) and `ctutils` 0.4.5 (RUSTSEC-2026-0385: masks not made in
  constant time). Nothing else changed.

## 0.5.0 — the gateway: a push for a server of one's own

- The gateway (`spec/protocol.md`, "The gateway"): a push for a server of
  one's own. A push server of the network with `[gateway] accept = true`
  issues tickets for the tokens of its devices (`POST`, `GET`, `DELETE
  /v1/tickets`, signed as `/v1/devices`; 32 random bytes, `tickets.days`
  30, `tickets.per_device` 8, kept as their SHA-256 and first 8 hex,
  gone with their device and with its dead token, the ones that ran out
  swept once an hour) and wakes them for a private push server
  (`POST /v1/gateway/wake`, no signature: `404 ticket_unknown`, `413`,
  `429` after 30 a minute per ticket, then one attempt at FCM with the
  `data` type `sealed` — `blob`, `sender`, `trace` — answered `202`, `404
  ticket_unknown` for a dead token, `503 provider_unavailable` or `502
  provider_rejected`). Its log of a wake: the trace, the ticket's first 8
  hex, the size, the outcome. `/v1/info` says `tickets`; `vpush ctl stats`
  counts them.
- The wake counted by the device as well as by the ticket: 30 a minute
  and 300 an hour per device, summed over all its tickets, so eight
  tickets in hostile hands wake a phone no more often than one; urgent
  wakes (a call) in a budget of their own, 20 a minute and 120 an hour per
  device, so a flood of ordinary wakes never silences a call; and by the
  address, 600 requests to wake a minute (behind `trusted_proxy`), before a
  random ticket costs a hash and a query. All `429 rate_limited`.
- `Wake.urgent` (optional, `false` when absent; `golden/wake_urgent.json`):
  the gateway sends at high priority only an urgent wake, and a private
  push server asks for one only for the push of a call (`call` inside);
  every other wake goes at normal priority.
- What the gateway learns, said as it is: the token, the identity that
  took the ticket, the address and seal key of the private server that
  wakes it, the times, the size and whether a wake is urgent; not the
  sender, the chat, the kind or the content (blind tickets, which would
  hide which identity uses which private server, are not built).
- A private push server (`[gateway] url`, `key_file` made at the first
  start) registers devices with the channel `gateway` (ticket and push
  key), seals every push — the `data` map FCM would have carried, NIP-44
  v2 from its key to the push key, base64url — and hands it to the
  gateway; a push over 3600 sealed goes by `event_id` and `relay`, a lost
  ticket is a dead token, `429` and `503` are tried again, the ticket is
  checked at registration with a sealed `test` push marked `check`
  (`"1"`), which the device shows nothing for (`Payload.check`). No key of FCM is
  needed: an app is named without one. `/v1/info` says `"providers":
  ["gateway"]` and `gateway: { url, sender }`.
- Migrations 006 (the table of devices made again, for the provider
  `gateway`; a release before this one runs on it) and 007 (tickets).
- `vpush-proto`: `Channel::Gateway`, `PushType::Sealed` with `blob` and
  `sender`, the bodies of the tickets and the wake, `Info.gateway` and
  `Info.tickets`, the codes `no_device`, `ticket_unknown`,
  `provider_unavailable`, `provider_rejected`; examples of all in
  `golden/`. `spec/operations.md`, "The gateway".

## 0.4.0 — the invitation to a call

- The release carries VGuard: `vguard-linux-x86_64` and
  `vguard-linux-aarch64` beside the binaries of `vpush`, in the same
  `SHA256SUMS`, each signed by the release key as the component `vguard`
  (`vguard-linux-<arch>.sig`). The kit of Veydan-Server takes it from
  here, and puts up no relay without it.
- VGuard (`crates/vguard`, the binary `vguard`), the read policy of a
  relay of the network: a WebSocket proxy between nginx and strfry that
  passes every frame on, but answers itself a REQ, COUNT or NEG-OPEN for
  gift wraps of a key the connection did not sign in with (NIP-42), for
  events of groups without `#h`, or with no filter worth the name. The
  other relays and VPush come with the key of the peers and read
  everything. `spec/vguard.md`. `scripts/build-release.sh` builds it
  beside `vpush` (`dist/vguard`), and `make build-deploy` puts it into
  the kit of a server.
- VGuard before its first install on the relays, for the apps already
  released: an AUTH may be 3 days from the clock of the relay
  (`auth_skew_secs`, was 10 minutes), so a phone with a wrong clock still
  signs in and gets its messages; a challenge signs in once, so the same
  AUTH sent again on the connection is refused. A question for gift wraps
  sent after the challenge but before the AUTH (the app's history, as the
  connection opens) waits for the AUTH up to `auth_wait_ms` (5 s) and is
  answered after it, instead of `auth-required`, which sent the app to a
  plain REQ capped at strfry's newest 500 events; what the client sends
  meanwhile is held behind it in order (32 frames or 1 MiB at most), the
  AUTH alone goes ahead. `spec/vguard.md`, "Signing in".
- Denied keys (Veydan Cloud, the owner's ban of a key on every server of
  the network): `denied-keys.txt`, one key a line, laid by the agent of
  vcloud and read again when it changes (once a minute at most). VGuard
  (`denied_keys_file`) refuses the AUTH of such a key (`restricted: key
  denied`) and every EVENT it signed (`blocked: key denied`), answers a
  question for its events alone as empty and drops its events on their
  way from strfry; VPush (`denied_keys_file`, by default beside the
  database) refuses its requests with `403 key_denied`. No file, nobody
  denied: a server without one works as before. `spec/vguard.md`,
  `spec/protocol.md`, `spec/operations.md`.
- Closed groups (Veydan Cloud, the owner's closing of a group on the
  relays of the network after a report on it): `denied-groups.txt`, one
  id of a group a line, beside `denied-keys.txt` and read again the same
  way. VGuard (`denied_groups_file`) answers a question for the events of
  closed groups alone as empty, takes the closed ids out of a question
  that names open groups too, refuses every EVENT with an `h` of a closed
  group (`blocked: group denied`) and drops such events on their way from
  strfry; a sync of closed groups alone is refused. No file, no group
  closed. `spec/vguard.md`, "Denied groups".
- `spec/protocol.md` says what a relay learns from VPush: the list of
  keys and groups it watches there.
- Anonymous requests can no longer fill the memory of used signatures and
  lock everybody out: a signed event is remembered only once the request
  reached a device of its signer (or a valid registration), and every
  request to a device is limited to 120 a minute from one address (a /64 of
  IPv6), when `server.trusted_proxy` is on. `spec/protocol.md`,
  `spec/operations.md`.
- Parallel registrations cannot pass `devices_per_pubkey` and
  `devices_total`: the transaction takes the write lock first (`BEGIN
  IMMEDIATE`), and a limit is said as a limit, not as `internal`.
- `devices-forget` makes the watcher read the plan at once, so the relays
  stop asking for the forgotten owner without waiting for the minute.
- `vpush ctl devices-forget <owner>` (the admin request `devices_forget`)
  forgets every device of one owner at once, with their relays and groups:
  the step `push.forget` of the deletion of an account of Veydan Cloud,
  which the agent of vcloud runs. The answer says how many went.
- A gift wrap with `["call", "1"]` on its outside (the invitation to a
  call of the messenger) is pushed at once, past the throttle of the chat
  and the cap of the device, with a `ttl` of a minute and the collapse key
  `call`. The push is otherwise a `dm` push with `call` set to `"1"`; the
  phone opens it and finds the call. `spec/protocol.md`, "What an app
  writes on its events".
- A device is rung for no more than `[pipeline] call_cap` calls within
  `call_window_secs` (6 in 60 by default): the tag is anyone's to put on
  a wrap. What comes over that is dropped.
- `["call", "0"]`, the end of a ringing (the call was taken on another
  device, declined, or given up), is pushed the same way: at once, for a
  minute, and the phone opens it and stops ringing. It has a collapse key
  of its own, `call_end`, so that the end of one call never takes the
  place of the waiting ring of another. The ends are on a budget of their
  own, with the cap of the rings. `spec/protocol.md`, `["call", "0"]`.

## 0.3.1 — the source is public

The server is the same as in 0.3.0; this release opens its source.

- `THIRD-PARTY-LICENSES.md`: every crate of `Cargo.lock` with its licence,
  written by `make third-party`.
- `spec/operations.md`: the setup of one particular server gave way to the
  general case it was an example of, "nginx in docker".
- `make build-deploy` is told where the server kit is:
  `DEPLOY_BIN=<folder>`.
- The plans the server was built by stay with the project; the protocol and
  the operations guide are here.

## 0.3.0 — the mark of the group

A group event is pushed only when a holder of the key of the group marked
it. The id of a group is no secret: it is in the `h` tag of every event of
the group, in the open, and anybody can sign an event that names it. Such
an event used to wake every member, and FCM carried it to them.

**The body of a registration changed.** An app older than this release
names a group by its id alone and is answered `bad_request`: it registers
nothing until it is updated. The server and the app are released together.

- Registration: `groups` is a list of `{ "id", "keys" }`. `keys` are the
  push keys of the group: `HMAC-SHA256(key = the key of the group, msg =
  "veydan-push-group-v1")`, 64 hex characters, of the key of now, and of
  the one before when the key was changed. The server is never given the
  key of the group, and a push key opens nothing.
- An id or a key that is not 64 hex characters in lower case, no key or
  more than two, a group named twice, one key twice in a group:
  `bad_request`. A group named twice used to be taken once.
  `limits.groups_per_device` counts groups as before.
- On a group event, `["gp", mark]`: the first 16 hex characters of
  `HMAC-SHA256(key = the push key, msg = the public key that signed the
  event)`, made the way the mark of the author (`vp`) is. One tag, or two
  for seven days after the key of the group changed: the mark of the new
  key, and of the one before.
- A group event is pushed to the devices that registered a push key whose
  mark is on it, and to no other device of the group. One without a mark,
  or with the mark of no key registered for the group, is pushed to
  nobody: noted, `Unmarked` in the log at `debug`, and counted in
  `group_events_unmarked` under `since_start` of `vpush ctl stats`.
- `silent` is looked at before the mark, and `vp`, the throttle and the 30
  pushes a minute after it, all as they were.
- The first four `gp` tags of an event are read, and what is not 16 hex
  characters in lower case is passed over. An event is held against 64
  push keys of its group at most, those registered by the most devices
  first: keys somebody made up for a group do not crowd out the key of its
  members. When a group has more than 64 the log says so at `warn`.
- What a forged event costs: a note that it was seen when it carries no
  mark, and one reading of the keys of the group more when it carries one.
  The keys are read from the database for every such event; they are not
  kept in memory, where a key registered a moment ago would be missing.
- Migration 005 adds `device_group_keys`. The groups registered before it
  have no keys, and an event of such a group is for nobody (`Nobody`):
  **after the migration a device gets no group pushes until its app
  registers anew**. Direct messages are pushed to it as before.
- A push key is said back nowhere: not in the answer to `GET`, which lists
  the ids of the groups as before, and not in the log. `vpush ctl devices`
  shows how many keys a device registered for a group: `(1 key)`, `(no
  key)`. When a body cannot be read, what the answer quotes of it has
  anything long enough to be a key replaced by its mark.
- `spec/golden/device_put.json` is the new shape. The copy in the app must
  follow.
- `spec/protocol.md`: the push keys, the mark, what the server learns from
  a push key, and what its limits hold back and cannot stop.

Going back: 0.2.2 runs on a database 0.3.0 has been at, and pushes group
events to every device of a group again. It does not read the registration
of the app that 0.3.0 was released with.

## 0.2.2 — not to be worn down

What anybody may ask of the server costs it little now, and what a relay
may send at it loses nothing. An app that talks to 0.2.1 talks to this
release; it meets two new words of refusal, `token_invalid` and
`limit_devices_total`, and one more kind of push, `sync`.

- A token is asked about before its device is registered. FCM is sent the
  request a push is sent with, marked `validate_only`: it checks the token
  and sends nothing. A token it does not know is refused with
  `token_invalid` (422), and nothing is kept. A made-up token used to be
  registered, watched for and pushed to in vain.
- The question is asked for a new token and for a device that is
  `dead_token`. A device that was marked dead comes back with the token it
  has when FCM vouches for it; it used to stay dead until the phone got a
  new one. A renewal of a token FCM has vouched for asks nobody.
- When FCM cannot say (busy, out of reach, five seconds without an answer)
  the device is registered as before and noted as not vouched for; the
  first push decides. Migration 004 adds `devices.token_checked_at`, and
  `vpush ctl devices` shows it.
- Registrations of one owner: 10 a minute. New devices from one address: 20
  an hour. Past either, `rate_limited` with `Retry-After`. The address is
  the one the reverse proxy names in `X-Real-IP`, and is read only with
  `[server] trusted_proxy = true`; without the key the limit by address is
  off, and the start of the server says so. `X-Forwarded-For` is never
  read. An IPv6 address is counted by its first 64 bits.
- `[limits] devices_total` (100 000): a registration that would be one
  more device is refused with `limit_devices_total` (422). Those who are
  registered renew as before.
- What is watched on a relay changes no more often than once in 15 seconds.
  The first registration after a quiet time is asked for at once, and what
  follows it waits and is asked for together. Every change used to be
  asked for within a second, and every asking makes the relay send two
  days of direct messages again.
- A device is pushed to no more than 30 times a minute, whatever the pushes
  are about (`pipeline.device_cap`, `pipeline.device_window_secs`). What
  comes over that is counted, and one push of the type `sync` says how
  many there were: `count`, and no event, no id and no group. It is shown
  ("10 new messages"); `sync` is no longer one of the silent types. Every
  event that was held back is in the count of one `sync` push, and there
  is one such push a minute at most.
- What a relay sends is put into a queue of 10 000 and dealt with by a task
  of its own. It used to be dealt with where it was received, one event and
  several trips to the database at a time, while nostr-sdk's channel of
  2048 places filled behind it and dropped the oldest without a word.
- When the queue is full the line of the relay starts over: a new
  connection, asked again as after any time away. What was in the queue is
  dealt with first; what was put into it is remembered and not taken a
  second time, so a line that starts over has room for what it lost. The
  log says so at `warn`. No more often than once in 15 seconds, after the
  first time.
- nostr-sdk does not say when its channel drops something: its stream of
  notifications skips over the loss. So an event is let into nostr-sdk
  only while the channel has room for it, and the relay is held back for
  the moment it takes the line to make room. Nothing is dropped there now.
- `vpush ctl stats` has `since_start`: registrations refused by each limit
  (`refused_rate_owner`, `refused_rate_ip`, `refused_devices_total`),
  tokens refused (`tokens_invalid`), `sync_pushes`, and
  `lines_started_over`.

To do after the deploy: `trusted_proxy = true` in the config of a server
behind nginx (the example nginx config sets the header).

## 0.2.1 — what the audit found

Nothing changes between the server and its clients: an app that talks to
0.2.0 talks to this release as it is.

- What the push service says of a push is noted for the token the push went
  to. A push on its way to a former token, answered `dead_token` after the
  phone had registered a new one, marked the new token dead.
- FCM's `INVALID_ARGUMENT` for a push without the event, or for both forms
  of a push, is about the token: `dead_token`, and so is a token of another
  project (`SENDER_ID_MISMATCH`). Such a device was pushed to in vain for
  thirty days. A push that is only too big is sent once more without the
  event, as before, and no token is blamed for it.
- `vpush ctl log set … --for` with a time no clock counts to took the
  server down. A change of levels lasts a day at most, and an hour when
  `--for` is not given: there is none without an end any more.
- Config: `pipeline.throttle_secs = 0` is refused; it turned the throttle
  off. The key of a relay (`api_key_file`) is 16 or more letters, digits,
  `.`, `_`, `~` or `-`: it is put into an address as it is, and taken out
  of the log as it is.
- A relay whose address nostr-sdk does not take was `pending` for ever. It
  is `unreachable` now, the log says why, and it is tried again every time
  the plan is read.
- NIP-98: a used event is remembered for 122 seconds, not 120. One dated a
  minute ahead could be used a second time in its last second. No more than
  100 000 are remembered; past that a request is answered `rate_limited`.
- Test pushes are counted by the token: the same phone registered under a
  new `device_id` got `test_per_hour` more. A test push is one attempt of
  ten seconds at most, and a push service that is busy or silent is
  answered about as `retry`. The request used to end in a bare 504, and
  `vpush ctl test-push` in "did not answer in time".
- A key or a group that is no longer watched on a relay loses the stock
  taken there. When it came back, what the relay had taken for it in the
  meantime was pushed as new. The table `baselines` no longer only grows.
- The ids of subscriptions carry a number that changes every time they are
  made anew (`dm-0-g17`). The end of stored events of a request that had
  been replaced was taken for the end of the new one, and what the new one
  then brought was pushed as new.
- An event dated more than 15 minutes ahead, or more than three days back,
  is noted and not pushed (`Misdated`): it would be pushed again every time
  the server forgets it.
- A relay that had been failing for long lost the requests made while it
  was away until the hourly renewal; they are made again the moment it is
  back on the line.
- The deploy builds before it reads the way to the server, so no build
  script or macro of a dependency runs with the server's password around.
  `deploy.env` is kept readable by its owner alone, and is ignored by git
  in both places it may lie.
- The server's key is recorded once (`make trust`) and every command talks
  only to a server that shows it. A server seen for the first time used to
  be trusted and handed the password.
- The database is saved before a switch by the database itself
  (`vpush backup`, `VACUUM INTO`): what was still in the write-ahead log is
  in the copy. A copy that cannot be made stops the deploy.
- `make rollback` holds the release it goes back to to the same test as a
  new one (up, and staying up), and returns to where it was when it fails.
- The unit gives the server no capabilities, only its own directories, the
  network, and the system calls of an ordinary service. Run
  `make bootstrap` once to install it.

## 0.2.0 — the phone reads the message itself

The server writes no texts any more. A push carries the event, and the
device opens it and writes the notification in its own words: who wrote,
what they said, in the user's language. Payload version 2.

- A `dm` or `group` push carries the event itself in `event` when the whole
  push weighs 3900 bytes or less, and `event_id` with `relay` when it does
  not: one form or the other, never both. A push that stands for several
  events (`count`) names the last of them, since one event cannot show
  five. `title`, `body` and `group_name` are gone; a `test` push says only
  what it is.
- When FCM refuses a push with the event as `INVALID_ARGUMENT`, it is sent
  once more without it.
- Registration: `groups` is a list of ids, and `locale` is no more. The
  server does not learn what the user calls a group, nor what language the
  user reads. `texts.rs` and `locales/` are deleted, with `vpush ctl
  test-push --title/--body/--silent`.
- Migration 003 drops the columns `devices.locale` and `device_groups.name`.
  It takes away, which the rule of 001 forbids, and may because no release
  before this one was in production: a 0.1 release cannot open a database
  0.2.0 has been at.
- `spec/golden/`: `device_put.json`, `push_dm.json` and `push_group.json`
  are the new shapes. The copy in the app must follow.

## 0.1.1 — found on the phone

- A test push waits ten minutes for the phone, not one. A phone whose
  connection to the push service went stale learns it only at its next
  heartbeat, and a test push was dropped in the meantime without a word.
- Signals are listened for before `ready` is logged. A SIGHUP right after
  it met the default action and ended the process silently.
- A second server on the same admin socket stops before it opens the
  database of the running one.
- The tests start their local relays on ports the system hands out; the
  relay's own random pick collided with the servers of other tests.

## 0.1.0 — from an event on a relay to a push (phone push plan, stage B2)

The server does what it is for: a message for a registered user appears on
a relay, and the user's phone is told.

- A relay of the allow-list is connected to when somebody registered for
  it, by a task of its own; a relay nobody needs any more is let go. The
  connection comes back by itself, for as long as it takes.
- What is watched is read from the registrations, whenever they change and
  once a minute. A device that leaves takes nothing from a device that stays.
- Direct messages are asked for two days back, because their senders date
  them so; group events from when the relay was last on the line.
- What was on a relay before the watch of a key or a group began is taken
  stock of, not pushed. What came while the server was away is pushed once
  when it is back.
- An event is dealt with once, however many relays bring it and however
  often they bring it again.
- `silent` on an event: no push. `vp` on a group event: no push to the
  devices of its author. Both are described in `spec/protocol.md`.
- A device is woken about one chat once in `pipeline.throttle_secs` (20).
  The first event is pushed at once; the rest are counted and told in one
  push when the time is up.
- Texts in the language of the device; a group by the name the device gave it.
- Every event has a `trace`, in every log line about it and in its pushes.
- Statuses of relays for clients are real now: `ok`, `pending`,
  `restricted` (with what the relay said), `unreachable`.
- `vpush ctl relays`.
- The keys of gated relays are taken out of everything that is logged, by
  this server or by a library: nostr-sdk logs the address it connects to,
  and the key is a part of that address.

Found on the live server, and fixed:

- The first TLS connection to a relay took the process down: no crypto was
  chosen for rustls. The tests talked to relays without TLS. There is a test
  with TLS now, and it fails without the fix.
- The deploy took a release for alive when it answered once. A release that
  fell a moment after the start passed, and systemd went on starting it.
  Alive now means up for eight seconds without a restart.
- A release that kept falling left systemd unwilling to start the next one,
  the one with the fix. The deploy clears that before it starts a release.
- A release older than the database could not open it: migrations it did
  not know were an error. They are not any more, which is what going back a
  release needs. Holds from this release on: 0.0.3 cannot open a database
  0.1.0 has been at.

Known: `relay.damus.io` answers `auth-required` to a request for the direct
messages of somebody else, so it is `restricted`. Relays that give a sealed
message to its recipient only cannot be watched by a push server.

## 0.0.3 — registration of devices (phone push plan, stage B1)

Devices can register, and ask for a test push. The server still watches no
relay: a registered device gets no pushes about messages yet.

- `PUT`, `GET`, `DELETE /v1/devices/{device_id}` and
  `POST /v1/devices/{device_id}/test`; `GET /v1/info`. See `spec/protocol.md`.
- Every request is signed (NIP-98). The key that signed it owns the device;
  to anybody else the device does not exist. The address is compared in
  full, an event opens one request, and the clock may differ by a minute;
  `auth_expired` carries the server's clock.
- The database: one SQLite file (WAL), behind the trait `Store`. Config
  `[store] path`. Migrations only add, so a rollback finds a database it can
  read.
- Which relays are watched: `[relays] policy = "allow_list"` and
  `[[relays.allow]]`. A relay that is not on the list is answered about by
  name and left out; the rest of the registration stands.
- Limits: devices of an owner, relays and groups of a device, test pushes an
  hour. `[limits]`.
- Texts of pushes by the language of the device: `locales/ru.toml`,
  `locales/en.toml`. A name of a group is kept per device.
- A token registered under a new owner removes the device of the former one.
  A token the push service gave up on marks the device `dead_token`; a new
  token clears the mark.
- `vpush ctl devices <npub or hex>`, `vpush ctl stats`.
- Registrations nobody renewed for `registration_days` are forgotten.
- `spec/golden/`: examples of the protocol, read by the server's tests and
  meant to be copied by clients.
- `cargo run -p vpush-server --example probe`: a client to ask a running
  server by hand.

Found while testing: two requests of one second are one event unless the
client adds a `nonce` tag. It is in the protocol now.

## 0.0.2 — FCM delivery (stage 1)

Still nothing is pushed by itself: there are no relays and no registrations
yet. What exists is the way out, and a command to try it.

- Delivery through FCM (HTTP v1) with the service account file of the
  Firebase project. One access token serves all pushes and is replaced
  before it runs out, or at once when FCM refuses it.
- Pushes are data-only, always. There is no `notification` part, so Android
  never shows a second notification next to the app's own.
- What FCM answers is turned into one of four outcomes: `delivered`,
  `dead_token`, `rejected`, `retry`. A token is dead only when FCM says
  `UNREGISTERED`. `INVALID_ARGUMENT`, a token of another project and a bare
  404 (a wrong project id) leave the device registered.
- Further attempts: up to three, the wait doubles, `Retry-After` is obeyed.
  A service that asks for more than 30 seconds is not waited for.
- `vpush ctl test-push --app <id> --token <token>`: one push to one device,
  with every attempt and what the service said. `--silent` for a push
  without text.
- Config: `[apps.<id>.fcm]`. The service account file is read and checked by
  `check-config` and at start. `[apps.<id>.apns]` is read and checked too;
  delivery to iOS is not built yet.
- Tokens, access tokens and keys never reach the log; a device token is
  written as `#1a2b3c4d`. A test reads the log at trace level to make sure.
- The push payload (`vpush-proto`): `spec/protocol.md`.

## 0.0.1 — skeleton (stage 0)

Nothing is pushed yet. This stage is the ground the rest stands on.

- Configuration: one TOML file. An unknown key or a wrong type is an error.
  Every problem in the file is reported at once. `vpush check-config`.
- Logging: a level per module, from the config. Changed on the running server
  with `vpush ctl log set relay=debug --for 15m`; the change goes away by
  itself when the time is up. `systemctl reload vpush` re-reads the log
  section of the config. Text or JSON output.
- `GET /healthz`. Every answer carries `X-Request-Id`; every request is one
  line in the log, the health check only at debug level.
- Admin socket (unix socket, owner and group only) and `vpush ctl`:
  `health`, `log show`, `log set`, `log reset`.
- Stops cleanly on SIGINT and SIGTERM. A second server on the same socket
  refuses to start.
- Release build: one static binary (musl), stamped with version, commit and
  build time.
- Deploy: `make deploy` with a rollback of its own when the new release does
  not answer; `make rollback`, `make status`, `make logs`, `make bootstrap`.
- Tests: unit, the real binary end to end, the deploy scripts against a
  directory (`make test-deploy`). CI job `vpush`.

## Next

1. ~~FCM delivery and `ctl test-push`~~ done in 0.0.2.
2. ~~Storage and the registration API (NIP-98)~~ done in 0.0.3.
3. ~~Relay connections~~ done in 0.1.0.
4. ~~The pipeline from event to push~~ done in **0.1.0**.
5. ~~The phone reads the message itself; no texts on the server~~ done in **0.2.0**.
6. Reliability: reconnect gaps, retries.
7. Admin commands by direct message, silent service pushes, broadcasts — **0.3.0**.
8. Relays of other operators: address checks, probe, limits — **0.4.0**.
9. UnifiedPush — **0.5.0**.
10. Diagnostics: traces per user, relay or device.
11. Hardening and load — **0.6.0**.

APNs (iOS) has its place in the interfaces from stage 1 and is built later.
