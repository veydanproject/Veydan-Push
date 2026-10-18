# Running VPush

For the person who runs the server. Covers what exists as of 0.1.0.

## What is on the server

```
/opt/vpush/
  releases/<version>-<commit>/vpush    every release that was deployed (last 5)
  current  -> releases/...             the one that runs
  previous -> releases/...             the one that ran before it
  etc/vpush.toml                       configuration (root:vpush, 0640)
  etc/secrets/                         keys (vpush only, 0700)
  data/vpush.db                        database
  data/backups/                        a copy per deploy (last 10)
  log/                                 unused unless file logging is set up
/run/vpush/admin.sock                  socket of `vpush ctl`
/etc/systemd/system/vpush.service
```

The server runs as the user `vpush`, which can write only to `data/` and
`log/`.

## First time

On your machine, in the `VPush` folder:

```
cp deploy/deploy.example deploy.env
```

Put the server's address and login in `deploy.env` (here or in `deploy/`).
The file is never committed, and the scripts keep it readable by you alone.
The way in is `VPUSH_PASSWORD` or `VPUSH_SSH_KEY`; with neither, ssh asks
for the password once per command. A key is the better of the two: nothing
secret lies in a file.

```
make trust
```

This records the server's key next to `deploy.env` (`known_hosts`), once per
machine you deploy from. It is taken from your own `~/.ssh/known_hosts` when
you have been to the server before; otherwise it is asked of the server and
shown, and you compare its fingerprint with the server's own word
(`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on its console) before
saying yes. Every other command talks only to a server that shows this key:
whoever answered in its place would be handed the password.

```
make bootstrap
```

This creates the user, the directories and the systemd unit, and writes the
example config. It can be run again: what exists is kept, and the unit is
replaced by the one of this release (it takes effect at the next start).

Then, on the server, set `public_url` in `/opt/vpush/etc/vpush.toml` to the
address clients will use.

VPush listens on `127.0.0.1:8090`. TLS belongs to the reverse proxy in front
(nginx, caddy), which forwards to that address.

## Domain and TLS

VPush speaks plain HTTP and leaves TLS to the reverse proxy. The proxy must
pass the path and the query on unchanged: registration checks a signature
over the full URL, which has to match `public_url`.

`deploy/nginx.example.conf` is the nginx part, with the certbot command in
its header.

The proxy also says whom a request came from, in the header `X-Real-IP`
(the example sets it). VPush reads the header only when told that a proxy
is there:

```toml
[server]
trusted_proxy = true
```

Set it when every request reaches VPush through the proxy, as it does
behind nginx. With it, new devices are counted by the address they are
registered from (see "Limits"). Without it that limit is off, and the log
says so at the start: VPush sees only the proxy's address, and a header is
whatever a client wrote. `X-Forwarded-For` is never read. A request with no
`X-Real-IP` while `trusted_proxy` is on came past the proxy; all such are
counted as one client.

### nginx in docker

When nginx already runs in a docker container on the host and holds ports 80
and 443 (a relay's stack, say), VPush is added to it instead of getting a
proxy of its own.

| Piece | Setting |
|---|---|
| DNS | the name of VPush, an A record to the host; behind a CDN only if it passes requests through unchanged |
| VPush | `listen = "172.17.0.1:8090"`, the docker bridge; the host loopback is not reachable from a container. `trusted_proxy = true`: nginx sets `X-Real-IP` |
| ufw | `172.16.0.0/12 → 172.17.0.1:8090` allowed; the port is closed from outside |
| Certificate | its own, for the name of VPush (certbot standalone stops nginx for a few seconds on renewal) |
| nginx | the two blocks of `deploy/nginx.example.conf` in the nginx config of the container |

When the config of nginx is a template rendered at the start of the
container, a change is applied by restarting the container, not by a
reload. Before that, the new config can be tried in a throwaway container on
the same network with `nginx -t`.

VPush starts after docker (`After=docker.service`): the bridge address exists
only once docker is up.

## Every release

```
make deploy
```

| Step | What happens | If it fails |
|---|---|---|
| 1 | Static binary is built, checksum taken | Nothing was sent |
| 2 | Binary is sent, checksum compared on the server | Nothing was installed |
| 3 | The new binary reads the live config | Nothing was changed; fix the config |
| 4 | Server stops; the new binary saves the database to `data/backups/` (`vpush backup`) | The old release is started again; nothing was switched |
| 5 | `current` points at the new release, server starts | |
| 6 | The release has to stay up for 8 seconds without a restart; waited for up to 30 | Previous release is put back and started; the command fails |
| 7 | `previous` points at the old release; releases beyond the last 5 are removed | |

The server is down between steps 4 and 6, a few seconds.

`make rollback` swaps `current` and `previous`. Running it twice returns to
where you were. The release it goes back to has to pass the test of step 6;
when it does not, the one that was running is put back.

The build (step 1) happens before `deploy.env` is read: what is built and
fetched there never sees the server's password.

## Looking at it

```
make status                          release, systemd state, health
make logs                            follow the log
make logs ARGS="-n 500 --since -1h"  any journalctl arguments
```

## Logging

Levels: `off`, `error`, `warn`, `info`, `debug`, `trace`.

What each level is for:

| Level | What is written |
|---|---|
| `error` | Something is broken and needs a person |
| `warn` | Something is wrong but the server copes |
| `info` | Start, stop, state changes, one line per request |
| `debug` | One line per step of the work |
| `trace` | Everything, including what comes over the wire |

A level is set for everything, and separately for a module. Modules:
`admin`, `api`, `config`, `delivery`, `logging`, `serve` today; `relay`, `pipeline`,
`store`, `auth`, `netguard`, `adminbot`, `broadcast` as they
arrive. Other crates are named as they are: `nostr_sdk`, `hyper`.

### In the config

```toml
[log]
format = "text"      # or "json"
level = "info"

[log.targets]
relay = "debug"
nostr_sdk = "warn"
```

After editing: `systemctl reload vpush`. The log section is applied without a
restart. Everything else in the config needs `systemctl restart vpush`. A
config with an error changes nothing, and the error is written to the log.

### On the running server

```
make server-ctl ARGS="log show"
make server-ctl ARGS="log set relay=debug --for 15m"
make server-ctl ARGS="log set debug,nostr_sdk=trace --for 5m"
make server-ctl ARGS="log reset"
```

On the server itself the same is `vpush ctl log ...`, run as root or as a
member of the `vpush` group.

- `--for` takes `90s`, `15m`, `2h`, `1d`. When the time is up the level goes
  back by itself, and a line in the log says so.
- Without `--for` the change lasts an hour. A day (`1d`) is the longest a
  change can last, and a longer time is refused: a server forgotten at
  `trace` fills the disk. What is to stay goes into the config.
- Changes made this way are laid over the config. `log show` prints both the
  config levels (`base`) and what is in force (`effective`).
- A restart forgets them.
- Crates that log through `log` and not through `tracing` (`reqwest`,
  `tungstenite`) are named and changed the same way.

### One request through the log

Every HTTP answer carries the header `X-Request-Id`. The same id is in the
log line of that request, as `request_id=...`.

## Configuration

`vpush check-config --config <file>` reads a config and reports every problem
in it at once, without starting anything. The deploy does this with the new
binary before it switches.

| Key | Default | Meaning |
|---|---|---|
| `public_url` | required | The address clients use, as they see it. `https`; `http` for localhost only. No trailing slash |
| `server.listen` | `127.0.0.1:8090` | Where VPush listens |
| `server.request_timeout_secs` | `15` | A request that takes longer is answered with 504 |
| `server.max_body_bytes` | `65536` | A larger request body is refused |
| `server.trusted_proxy` | `false` | A reverse proxy in front names the client in `X-Real-IP`. See "Domain and TLS" |
| `admin.socket` | `/run/vpush/admin.sock` | Socket of `vpush ctl` |
| `log.format` | `text` | `text` or `json` |
| `log.level` | `info` | Level of everything not named in `log.targets` |
| `log.targets` | empty | Level per module or crate |
| `denied_keys_file` | `denied-keys.txt` beside `store.path` | The keys denied on every server of the network, one 64-hex key a line, `#` a comment; laid by the agent of vcloud (its helper, as a file of the `paths` of the component `vpush`) and looked at again once a minute. No file, nobody denied. A top-level key: it goes above every table of the file |

An unknown key is an error. So is a value of the wrong type.

## Signals

| Signal | Effect |
|---|---|
| `SIGTERM`, `SIGINT` | Finish what is in progress, stop |
| `SIGHUP` | Re-read the config, apply the log section |

## Apps and their keys

A token of FCM belongs to one Firebase project, so each app that registers
devices brings its own keys. The id of the section is what the app sends
when it registers.

```toml
[apps."net.veydan.mobile".fcm]
service_account = "/opt/vpush/etc/secrets/fcm-veydan.json"
```

The file comes from the Firebase console: Project settings → Service
accounts → Generate new private key. On the server:

```
install -m 0600 -o vpush -g vpush fcm-veydan.json /opt/vpush/etc/secrets/
```

`check-config` reads the file and checks the key in it. A file that is
missing or is not a service account stops the start, with the reason.

| Key | Meaning |
|---|---|
| `apps.<id>.fcm.service_account` | Path of the service account file |
| `apps.<id>.apns.*` | Read and checked; delivery to iOS is not built yet |

After a change: `systemctl restart vpush`.

## Does a push arrive

```
make server-ctl ARGS="test-push --app net.veydan.mobile --token <token of the device>"
```

The push carries no text: the phone shows a test push in its own words.

A test push is tried once, and given up on after ten seconds: whoever asked
is waiting. The answer says what came of the attempt:

```json
{ "trace": "5f3a9c1e", "outcome": "delivered",
  "attempts": [ { "outcome": "delivered", "http_status": 200, "latency_ms": 212 } ] }
```

| Outcome | Meaning | What to do |
|---|---|---|
| `delivered` | The service took the push | If the device shows nothing, the cause is on the device |
| `dead_token` | The service says the token will never work: read `code` below | The app was removed, got a new token, or is of another project |
| `rejected` | The service refused and another attempt will not help | Read `code` below |
| `retry` | The service is busy, cannot be reached, or did not answer in time | Try later; look at `code` and `detail` |

Codes of `dead_token`. A device whose push ends in one of them is marked,
and is not pushed to until it registers a new token:

| `code` | Cause |
|---|---|
| `UNREGISTERED` | The token is gone: the app was removed, or was given a new one |
| `SENDER_ID_MISMATCH` | The token is of another Firebase project than the service account |
| `INVALID_ARGUMENT` | The token is not a token. FCM says the same of a push that is too big, so a push that carried the event is first sent once more without it, and only a push refused without the event means the token |

Such a device comes back without a new token too: when it registers again
and FCM vouches for the token it has, it is `active`.

Codes of `rejected` and `retry`:

| `code` | Cause |
|---|---|
| `PROVIDER_AUTH` | Google refuses the service account: the key was revoked or the file is of another project |
| `PROVIDER_AUTH_UNAVAILABLE` | Google's token service did not answer |
| `THIRD_PARTY_AUTH_ERROR` | The key the Firebase project holds for APNs or for web push is refused there |
| `NOT_FOUND` without `UNREGISTERED` | The project id in the service account file is wrong |
| `QUOTA_EXCEEDED` | Too many pushes; FCM says when to come back |
| `UNREACHABLE` | No connection to FCM from this server |
| `DEADLINE` | A test push got no answer from the service in ten seconds |

The `trace` of the answer is in the log lines of that push:

```
make logs ARGS="-n 500 --grep 5f3a9c1e"
make server-ctl ARGS="log set delivery=debug --for 10m"    # a line per attempt
```

Device tokens are written to the log as `#1a2b3c4d`: the same token always
gives the same mark, and the token cannot be read back from it.

## Database

One SQLite file, `[store] path`, made by the server when missing. The deploy
saves it to `data/backups/` before every switch, with the server stopped.
The copy is made by the database itself, so what was still in the
write-ahead log is in it; by hand:

```
vpush backup --config /opt/vpush/etc/vpush.toml --out /root/vpush-copy.db
```

```toml
[store]
path = "/opt/vpush/data/vpush.db"
```

Looking into it by hand is safe while the server runs:

```
sqlite3 /opt/vpush/data/vpush.db "select pubkey, device_id, state, app_version from devices"
```

Tokens are in the database as they are: whoever reads the file can send
pushes to the devices in it. The file belongs to `vpush`, mode 0640, in a
directory nobody else enters.

## Relays

```toml
[relays]
policy = "allow_list"

[[relays.allow]]
url = "wss://relay.example.org"
api_key_file = "/opt/vpush/etc/secrets/relay.example.org.key"

[[relays.allow]]
url = "wss://nos.lol"
```

Only the relays named here are watched. A device that names another relay is
registered all the same, and told that this relay is `not_allowed`.

The key of a relay behind a gate lives in a file of its own
(`install -m 0600 -o vpush -g vpush`), one line. An address with a key in it
is refused, in the config and from clients, so that a key cannot end up in a
log.

A key is 16 or more letters, digits, `.`, `_`, `~` or `-`. It is put into
the address of the relay as it is and taken out of the log as it is: a key
with `&` or `#` in it would break the address, and a short one would be
found in words that are no secret. `check-config` names the file of a key
that will not do, never the key.

After a change: `systemctl restart vpush`.

## Limits

| Key | Default | Meaning |
|---|---|---|
| `limits.devices_per_pubkey` | 10 | Devices of one owner |
| `limits.relays_per_device` | 16 | Relays a device may name |
| `limits.groups_per_device` | 500 | Groups a device may name |
| `limits.registration_days` | 30 | A registration nobody renewed for this long is forgotten |
| `limits.test_per_hour` | 3 | Test pushes a device may ask for in an hour. Counted by its token: the same phone under a new `device_id` gets no more |
| `limits.devices_total` | 100000 | Devices of all owners together. A registration that would be one more is refused with `limit_devices_total`; those who are registered renew as before |

Two more limits are not in the config:

| What | How often | Refused with |
|---|---|---|
| Registrations (`PUT`) of one owner | 10 in a minute | `rate_limited` |
| New devices from one address | 20 in an hour, when `server.trusted_proxy` is on | `rate_limited` |
| Any request to `/v1/devices` from one address | 120 in a minute, when `server.trusted_proxy` is on | `rate_limited` |

A new device is a `device_id` its owner has not registered, or another
token for one that is: each costs a question to the push service and, when
it is taken, a row. An IPv6 address is counted by its first 64 bits.

### The token is asked about

A registration with a new token, or of a device that is `dead_token`, waits
while FCM is asked whether the token is one it can push to (`validate_only`:
nothing is sent to the phone). FCM's `UNREGISTERED`, `SENDER_ID_MISMATCH` or
`INVALID_ARGUMENT` refuses the registration with `token_invalid`. When FCM
cannot say within five seconds, is busy, or refuses the service account,
the device is registered all the same, and `vpush ctl devices` shows it
with `token_checked_at: null` until FCM vouches for the token or takes a
push for it. A registration that renews a token FCM has vouched for asks
nobody.

## Waking a device

| Key | Default | Meaning |
|---|---|---|
| `pipeline.throttle_secs` | 20 | A device is woken about one chat once in this many seconds; what comes in between is counted, and told in one push when the time is up |
| `pipeline.device_cap` | 30 | Pushes to one device within `device_window_secs`, whatever they are about |
| `pipeline.device_window_secs` | 60 | See above. What comes over the cap is counted and told in one push of the type `sync`, this long after the first of it; at most one such push in this time |

None of them may be zero.

## Who is registered

```
make server-ctl ARGS="stats"
make server-ctl ARGS="devices npub1…"
```

`devices` takes an npub or 64 hex characters, and shows every device of the
owner: the app, the state, the relays and groups watched for it, what
became of the last push, and when the push service vouched for the token
(`token_checked_at`). The token is shown as its mark. A group is shown by
the beginning of its id and the number of push keys the device registered
for it, never the keys: `111111111111 (2 keys)`.

`devices-forget npub1…` forgets every device of the owner at once, with the
relays and groups watched for them, and says how many went: the step
`push.forget` of the deletion of an account of Veydan Cloud, which the
agent of vcloud runs over the admin socket (`{"cmd":"devices_forget"}`).
A device registers again on the next start of the app, as a new one.

`stats` says how many devices and owners there are, and under `since_start`
what the server has counted since it started:

```json
{ "devices": 412, "owners": 380, "dead_tokens": 3,
  "since_start": { "refused_rate_owner": 0, "refused_rate_ip": 0,
                   "refused_devices_total": 0, "tokens_invalid": 2,
                   "refused_denied": 0,
                   "sync_pushes": 0, "lines_started_over": 0,
                   "group_events_unmarked": 0 } }
```

| Counter | Counts | When it grows |
|---|---|---|
| `refused_rate_owner` | Registrations refused: more than 10 a minute of one owner | An app that registers in a loop, or somebody trying the server |
| `refused_rate_ip` | New devices refused: more than 20 an hour from one address | Somebody registering devices in bulk; or many users behind one address, if it grows slowly |
| `refused_devices_total` | Registrations refused: `limits.devices_total` is reached | The server is full: raise the limit, or look at who registered |
| `tokens_invalid` | Registrations refused: FCM does not know the token | Made-up registrations; a few are phones whose token ran out |
| `refused_denied` | Requests refused: the key is denied on the servers of the network | An app of a denied key that keeps trying; the journal says when the list was read (`denied keys`) |
| `sync_pushes` | Pushes of the type `sync` | Some device gets more than `pipeline.device_cap` pushes a minute |
| `lines_started_over` | Times a relay's line started over | See "Relays on the line" |
| `group_events_unmarked` | Events of watched groups without the mark of a push key registered for the group | Somebody who is not a member writes into a group by its id; or an app older than the mark. See "Group events from strangers" |

They begin at zero with every start.

A user who says "I get no pushes" is asked for their npub; then:

| What `devices` shows | Meaning |
|---|---|
| Nothing | The app never registered, or the registration ran out. The user turns pushes on again |
| `state: dead_token` | The push service gave up on the token. The app registers again when it is opened |
| `relays` lacks the user's relay | The relay is not on the allow-list |
| `watched_groups` says `(no key)` | The device registered before 0.3.0. It is told of direct messages, and of no group until the app registers again |
| `last_outcome: delivered` | Pushes reach the push service; the cause is on the phone |
| `last_outcome: rejected` or `retry` | See "Does a push arrive" above |

### Group events from strangers

An event of a group is pushed to the devices that registered a push key
whose mark (`gp`) is on the event, and to nobody when it carries no such
mark. Every event of the second kind is counted once in
`group_events_unmarked`, and is `Unmarked` in the log at `debug`.

A few are events of an app older than the mark. A number that keeps
growing is somebody who knows the id of a group and not its key: nobody is
woken by it, and each event costs the server one reading of the keys of
the group and one row in `seen_events`, which is forgotten after three
days. To see the events: `log set pipeline=debug --for 10m`. The `event`
lines with `dealt=Unmarked` have the id of each and the relay it came
from; the relay has the event, with its group in `h`.

`a group has more push keys than an event is held against` at `warn`: more
than 64 different push keys are registered for the group the line names. A
group has one or two in use, so the rest were made up by devices that are
not its members. The 64 that the most devices registered are tried, which
keeps the key of the members among them; the devices that registered the
others are told nothing of the group. The line is written for every event
of such a group that carries a mark.

After the migration to 0.3.0 no device has a push key for any group until
its app registers again. Until then the events of its groups are `Nobody`,
not `Unmarked`, and are not counted.

## A config from a newer release

A release refuses a config with keys it does not know. So a config that
gained a section for a new release cannot be read by the release before it:
after such a change, `make rollback` needs the section taken out again by
hand. The deploy itself is safe: the new release is switched to with the old
config first, and the config is changed afterwards.

## Relays on the line

```
make server-ctl ARGS="relays"
```

| Field | Meaning |
|---|---|
| `state` | `connecting`, `connected`, `disconnected`; `since` says from when |
| `dm`, `groups` | How many keys and groups are watched there |
| `events` | Events that came since the server started |
| `refused` | What the relay said when it refused a subscription |

A relay is connected to when the first device registers for it, and let go
when the last one leaves. A relay that is down is tried again by itself, for
as long as it takes; the log says when it goes and when it is back.

A relay that is `disconnected` from the start, with `not an address
nostr-sdk takes` in the log once a minute, has an address the config took
and the library does not: one with `://` in its path, say. It is tried
again every time the plan is read, and watched after the address is put
right and the server restarted.

`refused: auth-required…` means the relay gives sealed messages to their
recipients only. Nothing can be done about it from here; the devices that
named the relay see `restricted`.

What is watched on a relay changes no more often than once in 15 seconds.
The first registration after a quiet time is asked for at once; what
changes within the 15 seconds after it is asked for together, when they
are over. Every asking makes the relay send two days of direct messages
again, and a burst of registrations must not turn into a burst of those.

### A line that starts over

What a relay sends waits in a queue of 10 000 until it is dealt with. When
more comes than that, the line of the relay starts over: it lets the relay
go, connects anew, and asks again, as after any time away. The log says so
at `warn`, with the relay and the reason: `the line starts over: the relay
is asked again for what was not dealt with`.

Nothing is lost by it: what was in the queue is dealt with first, the relay
still has the rest, and what was dealt with is not taken a second time. A
line starts over at once the first time and no more often than once in 15
seconds after that. `lines_started_over` in `stats` counts them. Once after
a flood is the server coping; a number that keeps growing is a relay that
sends more than the server deals with, and is worth a look at what is sent
there and to whom (`log set pipeline=debug,relay=debug`).

## One message through the log

Every event that leads to a push has a `trace`. It is in the push, so a
user can read it off the phone's log, and in the server's log:

```
make logs ARGS="-n 1000 --grep <trace>"
```

At the level `info` that shows the pushes. To see every event and what
became of it, for ten minutes:

```
make server-ctl ARGS="log set pipeline=debug,relay=debug --for 10m"
```

| `dealt` | Meaning |
|---|---|
| `Pushed { now, later, own, over }` | Pushed at once to `now` devices; counted for `later` devices that were woken about this chat a moment ago; `own` devices belong to the author; `over` devices had their 30 pushes of the minute, and the event is in the count of their `sync` push |
| `Baseline` | Was on the relay before the watch of that key or group began |
| `Quiet` | Marked by its author as not worth a push |
| `Nobody` | For nobody who is registered; of a group, also when no device registered a push key for it |
| `Unmarked` | Of a group, and without the mark of a push key registered for it: not from a member. Noted and pushed to nobody |
| `Duplicate` | Dealt with before: from another relay, or before a reconnect |
| `Misdated` | Dated more than 15 minutes ahead or more than three days back. Noted and not pushed: the server forgets an event after three days, and such a one would be pushed again every time a relay gave it after that |

## The keys of relays and the log

The key of a gated relay has to be put into the address the server connects
to, and libraries log addresses. So everything that is logged goes through
a filter that knows the keys and writes `***` in their place. In the log
the address of such a relay reads `wss://relay.example.org/?key=***`.

## When a release falls

A release that starts and then falls is not switched to: the deploy waits
until the release has been up for eight seconds without a restart, and goes
back when that does not happen in thirty.

What it cannot see is a fall that needs something to happen first. 0.1.0
fell on its first TLS connection to a relay, and connected to none until a
device registered, minutes after the deploy. After a deploy that changes
how relays are talked to, look at the log for a while:

```
make status
make logs
```
