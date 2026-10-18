-- The tickets of a gateway (spec/protocol.md, "The gateway"): a ticket
-- stands for the token of one device, without naming it, and a private
-- push server wakes the device by it.
--
-- The ticket itself is said once, to the device, and is kept nowhere: what
-- is kept is its hash, and its first 8 hex, by which the device names it.
-- Whom the ticket was handed to, the gateway never learns. A ticket goes
-- with its device.
CREATE TABLE tickets (
    -- SHA-256 of the 64 hex characters of the ticket, hex. A table with
    -- a rowid: of tickets issued in one second, the later is the newer.
    hash         TEXT PRIMARY KEY,
    prefix       TEXT NOT NULL,
    device       INTEGER NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    issued_at    INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    last_wake_at INTEGER
);
CREATE INDEX tickets_device ON tickets (device, issued_at);
CREATE INDEX tickets_expiry ON tickets (expires_at);
