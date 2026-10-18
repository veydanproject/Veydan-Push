# VPush

Push server of Veydan: watches Nostr relays for the users who registered and
wakes their devices when something arrives for them.

The server never sees message contents. It sees that an encrypted event
addressed to a user appeared on a relay, and tells the user's device.

**State: 0.4.0.** Devices register, relays are watched, and a message on a
relay becomes a push through FCM that the phone opens itself. What is
ahead: commands of the admin by direct message, UnifiedPush.

## Layout

```
crates/vpush-proto    what the server and its clients exchange (serde only)
crates/vpush-server   the server, as a library
crates/vpush          the binary: server and admin command line in one file
crates/vguard         VGuard, the read policy of a relay: a proxy between nginx
                      and strfry that answers a question for another key's
                      gift wraps or for groups it does not name itself
deploy/               systemd unit, example config, example deploy settings
scripts/              build, deploy, rollback
golden/               examples of the protocol, the contract with the clients
spec/                 the protocol for clients, and how to run the server
```

The protocol a client speaks is in [spec/protocol.md](spec/protocol.md);
running, logging and every setting are in
[spec/operations.md](spec/operations.md). VGuard, which shares the nostr and
WebSocket stack of VPush and is built and shipped with it, is in
[spec/vguard.md](spec/vguard.md).

## Working on it

```
make test            unit tests and tests of the real binary
make check           clippy, warnings are errors
make run             the server, here, with vpush.toml next to the Makefile
make ctl ARGS="log set relay=debug --for 15m"
```

The Rust version is the one `rust-toolchain.toml` names.

## Putting it on a server

Once per server:

```
cp deploy/deploy.example deploy.env     # the address and the login
make bootstrap                          # user, directories, systemd unit
```

Then set `public_url` in `/opt/vpush/etc/vpush.toml` on the server.

Every release:

```
make deploy
```

It builds one static binary, sends it, lets the new binary read the live
config, saves the database, switches, and waits for the health check. A
release that does not answer is replaced by the previous one, and the command
fails.

```
make status          which release runs, is it alive
make logs            follow the log
make rollback        back to the release before this one
make server-ctl ARGS="log set relay=debug --for 15m"
```

## Licence

VPush is under the PolyForm Perimeter License 1.0.1, the licence of every
part of Veydan: [LICENSE](LICENSE) is the text that counts, and
[LICENSE-SUMMARY.md](LICENSE-SUMMARY.md) says it in short. In plain words:
you may run the server, for yourself or for anyone, and read, build, change
and share this source, as long as you do not use it to offer a product that
competes with it. The crates the server is built from are listed with their
licences in [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md).
