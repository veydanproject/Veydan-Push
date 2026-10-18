//! Configuration: one TOML file, `/opt/vguard/etc/vguard.toml` on a server
//! of the kit (services/server/templates/vguard.toml).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Where the config is looked for when `--config` is not given.
pub const DEFAULT_PATH: &str = "/opt/vguard/etc/vguard.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where nginx connects: `127.0.0.1:7778`.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// strfry: `127.0.0.1:7777`.
    #[serde(default = "default_upstream")]
    pub upstream: SocketAddr,
    /// The names the relay answers under (strfry's `serviceUrl` and the
    /// aliases): the `relay` tag of an AUTH must name one of them, or a
    /// relay elsewhere could pass on our challenge and sign in as its own
    /// user here.
    pub relay_hosts: Vec<String>,
    /// The keys denied on every server of the network, one a line, laid by
    /// the agent of vcloud (spec/vguard.md, "Denied keys"): read again when
    /// its mtime changes, at most once a minute. Not given, or no file:
    /// nobody is denied.
    #[serde(default)]
    pub denied_keys_file: Option<PathBuf>,
    /// The groups closed on our relays, one id a line, laid by the agent of
    /// vcloud beside the keys (spec/vguard.md, "Denied groups"); read again
    /// as the keys are. Not given, or no file: no group is closed.
    #[serde(default)]
    pub denied_groups_file: Option<PathBuf>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// The largest frame either way, bytes: strfry's `maxWebsocketPayloadSize`.
    pub max_frame: usize,
    /// Open subscriptions (REQ and NEG-OPEN) of one connection: strfry's
    /// `maxSubsPerConnection`.
    pub max_subs: usize,
    /// Filters in one REQ or COUNT: strfry's `maxReqFilterSize`.
    pub max_filters: usize,
    /// Connections at once.
    pub max_connections: usize,
    /// The WebSocket handshake of nginx, seconds.
    pub handshake_secs: u64,
    /// The connection to strfry, seconds.
    pub connect_secs: u64,
    /// No frame either way for this long closes the connection, seconds:
    /// nginx's `proxy_read_timeout`.
    pub idle_secs: u64,
    /// How far the `created_at` of an AUTH may be from now, seconds. Wide
    /// (3 days) because the clock of many a phone is off by hours, and a
    /// phone that cannot sign in gets no message at all. The window opens
    /// no replay: an AUTH is bound to a challenge this connection was
    /// given, which signs in once, and to the name of this relay.
    pub auth_skew_secs: u64,
    /// How long a question that needs a signed-in key waits for strfry's
    /// challenge before vguard sends one of its own, milliseconds.
    pub challenge_wait_ms: u64,
    /// How long a question that needs a signed-in key waits for the AUTH
    /// once a challenge is out, milliseconds. The app asks for its history
    /// as the connection opens, before it has signed in, and `auth-required`
    /// sends it to a fallback that gets only the newest events. 0: no
    /// wait, such a question is refused at once.
    pub auth_wait_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame: 262_144,
            max_subs: 200,
            max_filters: 200,
            max_connections: 20_000,
            handshake_secs: 10,
            connect_secs: 5,
            idle_secs: 300,
            auth_skew_secs: 259_200,
            challenge_wait_ms: 2_000,
            auth_wait_ms: 5_000,
        }
    }
}

impl Limits {
    pub fn handshake(&self) -> Duration {
        Duration::from_secs(self.handshake_secs)
    }
    pub fn connect(&self) -> Duration {
        Duration::from_secs(self.connect_secs)
    }
    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.idle_secs)
    }
    pub fn challenge_wait(&self) -> Duration {
        Duration::from_millis(self.challenge_wait_ms)
    }
    pub fn auth_wait(&self) -> Duration {
        Duration::from_millis(self.auth_wait_ms)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// `error`, `warn`, `info`, `debug`, `trace`.
    pub level: String,
    /// How often the counters go to the log, seconds; 0: never.
    pub metrics_secs: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            metrics_secs: 300,
        }
    }
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:7778".parse().expect("a socket address")
}

fn default_upstream() -> SocketAddr {
    "127.0.0.1:7777".parse().expect("a socket address")
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.check()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Every problem at once.
    fn check(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        for (name, file) in [("denied_keys_file", &self.denied_keys_file), ("denied_groups_file", &self.denied_groups_file)] {
            if let Some(path) = file {
                if !path.is_absolute() {
                    problems.push(format!("{name}: {} is not an absolute path", path.display()));
                }
            }
        }
        if self.relay_hosts.is_empty() {
            problems.push("relay_hosts: no name of the relay; nobody could sign in".to_string());
        }
        for h in &self.relay_hosts {
            if h.is_empty() || h.contains(['/', ':', ' ']) {
                problems.push(format!("relay_hosts: {h:?} is not a host name"));
            }
        }
        let l = &self.limits;
        for (name, v) in [
            ("max_frame", l.max_frame as u64),
            ("max_subs", l.max_subs as u64),
            ("max_filters", l.max_filters as u64),
            ("max_connections", l.max_connections as u64),
            ("handshake_secs", l.handshake_secs),
            ("connect_secs", l.connect_secs),
            ("idle_secs", l.idle_secs),
            ("auth_skew_secs", l.auth_skew_secs),
        ] {
            if v == 0 {
                problems.push(format!("limits.{name}: must be above 0"));
            }
        }
        if self.log.level.parse::<tracing::Level>().is_err() {
            problems.push(format!(
                "log.level: {:?} is not error, warn, info, debug or trace",
                self.log.level
            ));
        }
        if self.listen == self.upstream {
            problems.push("listen and upstream are the same address".into());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Whether `host` is a name of this relay (ASCII, any case).
    pub fn is_relay_host(&self, host: &str) -> bool {
        self.relay_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_config_takes_the_defaults() {
        let c = Config::parse("relay_hosts = [\"relay.example.org\"]\n").unwrap();
        assert_eq!(c.listen.to_string(), "127.0.0.1:7778");
        assert_eq!(c.upstream.to_string(), "127.0.0.1:7777");
        assert_eq!(c.limits.max_frame, 262_144);
        assert_eq!(c.limits.max_subs, 200);
        assert_eq!(c.limits.auth_skew_secs, 3 * 24 * 3600, "a phone's clock may be off by days");
        assert_eq!(c.limits.auth_wait_ms, 5_000);
        assert!(c.is_relay_host("RELAY.example.org"));
        assert_eq!(c.denied_keys_file, None, "nobody denied unless a file is named");
        let c = Config::parse("relay_hosts = [\"r\"]\ndenied_keys_file = \"/opt/vguard/data/denied-keys.txt\"\n").unwrap();
        assert_eq!(c.denied_keys_file.as_deref(), Some(Path::new("/opt/vguard/data/denied-keys.txt")));
        assert!(Config::parse("relay_hosts = [\"r\"]\ndenied_keys_file = \"denied.txt\"\n").unwrap_err().contains("absolute"));
        assert_eq!(c.denied_groups_file, None, "no group closed unless a file is named");
        let g = Config::parse("relay_hosts = [\"r\"]\ndenied_groups_file = \"/opt/vguard/data/denied-groups.txt\"\n").unwrap();
        assert_eq!(g.denied_groups_file.as_deref(), Some(Path::new("/opt/vguard/data/denied-groups.txt")));
        assert!(Config::parse("relay_hosts = [\"r\"]\ndenied_groups_file = \"g.txt\"\n").unwrap_err().contains("denied_groups_file"));
        assert!(!c.is_relay_host("relay.example.org.evil"));
    }

    #[test]
    fn every_problem_is_told() {
        let e =
            Config::parse("relay_hosts = []\n[limits]\nmax_subs = 0\n[log]\nlevel = \"loud\"\n")
                .unwrap_err();
        assert!(e.contains("relay_hosts"), "{e}");
        assert!(e.contains("max_subs"), "{e}");
        assert!(e.contains("log.level"), "{e}");
        assert!(
            Config::parse("relay_hosts = [\"a\"]\nnope = 1\n").is_err(),
            "an unknown key is an error"
        );
        assert!(
            Config::parse("relay_hosts = [\"wss://a\"]\n").is_err(),
            "a host, not a URL"
        );
    }
}
