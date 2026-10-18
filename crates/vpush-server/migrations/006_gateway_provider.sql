-- no-transaction
-- A device of a private push server is registered through the gateway
-- (spec/protocol.md, "The gateway"): its provider is `gateway`, its token
-- the ticket, and `channel_json` holds its push key.
--
-- SQLite cannot change the CHECK of a column, so the table is made again
-- with the wider one, as SQLite says to (https://sqlite.org/lang_altertable.html,
-- "Making Other Kinds Of Table Schema Changes"): with foreign keys off, or
-- the drop of the old table would take the relays and the groups of every
-- device with it. The pragma does nothing inside a transaction, so this one
-- opens its own. The table is the same in everything else, and a release
-- before this one runs on it as on its own.
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE devices_new (
    id           INTEGER PRIMARY KEY,
    pubkey       TEXT NOT NULL,
    device_id    TEXT NOT NULL,
    app_id       TEXT NOT NULL,
    provider     TEXT NOT NULL CHECK (provider IN ('fcm', 'apns', 'unifiedpush', 'gateway')),
    token        TEXT NOT NULL,
    channel_json TEXT,
    app_version  TEXT,
    pref_dm      INTEGER NOT NULL DEFAULT 1,
    pref_groups  INTEGER NOT NULL DEFAULT 1,
    author_key   TEXT,
    state        TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'dead_token')),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    last_push_at INTEGER,
    last_outcome TEXT,
    token_checked_at INTEGER,
    UNIQUE (pubkey, device_id)
);

INSERT INTO devices_new
    (id, pubkey, device_id, app_id, provider, token, channel_json, app_version,
     pref_dm, pref_groups, author_key, state, created_at, updated_at, expires_at,
     last_push_at, last_outcome, token_checked_at)
SELECT id, pubkey, device_id, app_id, provider, token, channel_json, app_version,
       pref_dm, pref_groups, author_key, state, created_at, updated_at, expires_at,
       last_push_at, last_outcome, token_checked_at
FROM devices;

DROP TABLE devices;
ALTER TABLE devices_new RENAME TO devices;

CREATE UNIQUE INDEX devices_token ON devices (app_id, provider, token);
CREATE INDEX devices_expiry ON devices (expires_at);

COMMIT;
PRAGMA foreign_keys = ON;
