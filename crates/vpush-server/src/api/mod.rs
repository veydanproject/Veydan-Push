//! Public HTTP API.

mod devices;
pub mod tickets;

use std::net::{IpAddr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use tower_http::timeout::TimeoutLayer;
use vpush_proto::denied::{DeniedKeys, Refreshed, RELOAD_EVERY};
use vpush_proto::{AppInfo, ErrorBody, ErrorCode, ErrorDetail, Info, Limits, TicketsInfo};

use crate::auth::Nip98;
use crate::config::Config;
use crate::counters::Counters;
use crate::delivery::Providers;
use crate::limit::{Budget, Limiter, Over};
use crate::relays::RelayPolicy;
use crate::store::Store;
use crate::version;
use crate::relay::Watch;

const REQUEST_ID: &str = "x-request-id";

/// Id of one request, carried into handlers and into the answer.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// What the handlers work with.
#[derive(Clone)]
pub struct Api {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub providers: Arc<Providers>,
    pub relays: Arc<RelayPolicy>,
    /// How the relays are doing, and the way to say that what is watched changed.
    pub watch: Arc<Watch>,
    pub auth: Arc<Nip98>,
    pub limits: Arc<Limiters>,
    pub counters: Arc<Counters>,
    /// The keys denied on every server (`Config::denied_keys_path`).
    pub denied: Arc<DeniedKeys>,
}

impl Api {
    pub fn new(
        config: Arc<Config>,
        store: Arc<dyn Store>,
        providers: Arc<Providers>,
        relays: Arc<RelayPolicy>,
        watch: Arc<Watch>,
        counters: Arc<Counters>,
    ) -> Self {
        Self {
            denied: Arc::new(DeniedKeys::from_file(&config.denied_keys_path())),
            auth: Arc::new(Nip98::new(&config.public_url)),
            limits: Arc::new(Limiters::new(config.limits.test_per_hour)),
            counters,
            config,
            store,
            providers,
            relays,
            watch,
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Looks at the file of the denied keys every minute, for as long as the
/// task lives: the agent of vcloud lays a new one within a minute of a
/// denial. What changed goes to the log, the keys never.
pub async fn watch_denied(denied: Arc<DeniedKeys>) {
    let path = denied.path().map(|p| p.display().to_string()).unwrap_or_default();
    tracing::info!(file = %path, keys = denied.len(), "denied keys");
    let mut tick = tokio::time::interval(RELOAD_EVERY);
    tick.tick().await;
    loop {
        tick.tick().await;
        match denied.refresh() {
            Refreshed::Same => {}
            Refreshed::Loaded(keys) => tracing::info!(file = %path, keys, "denied keys read again"),
            Refreshed::Missing => tracing::info!(file = %path, "no file of denied keys: nobody is denied"),
            Refreshed::Refused(why) => tracing::warn!(file = %path, %why, "the file of denied keys does not read; the list before stays"),
        }
    }
}

/// Registrations one owner may make in a minute. An app registers when
/// something changed and once in seven days; ten a minute is a burst of
/// changes and more, and every one past that is somebody trying the server.
pub const PUTS_PER_MINUTE: usize = 10;

/// Requests to the devices of the API one address (a /64 of IPv6) may make in
/// a minute, whoever signs them. An app asks once in a while; the room is for
/// many phones behind one address.
pub const REQUESTS_PER_MINUTE: usize = 120;

/// Devices the server did not know that may be registered from one address
/// in an hour. Each of them costs a question to the push service and a row.
pub const NEW_DEVICES_PER_HOUR: usize = 20;

/// Wakes of one ticket a minute (`spec/protocol.md`, "The wake"): a device is
/// pushed to at most thirty times a minute by any server, and this is the
/// most the gateway learns of the traffic of a ticket.
pub const WAKES_PER_MINUTE: usize = 30;

/// Wakes of one device a minute, summed over all its tickets: a device
/// holds up to eight, each handed to a server that may be hostile, and
/// together they wake it no more often than one ticket may.
pub const DEVICE_WAKES_PER_MINUTE: usize = 30;

/// Wakes of one device an hour, summed over all its tickets.
pub const DEVICE_WAKES_PER_HOUR: usize = 300;

/// Urgent wakes (a call) of one device a minute, summed over all its
/// tickets. They have a budget of their own, beside the one above and not
/// in it: a flood of ordinary wakes from one server never silences a call
/// from another.
pub const DEVICE_URGENT_WAKES_PER_MINUTE: usize = 20;

/// Urgent wakes of one device an hour, summed over all its tickets.
pub const DEVICE_URGENT_WAKES_PER_HOUR: usize = 120;

/// Wakes one address may ask for a minute, whatever the ticket, answered
/// or not: each costs a hash and a question to the database. Generous, as
/// a private push server sends the wakes of all its users from one.
pub const WAKE_REQUESTS_PER_MINUTE: usize = 600;

/// How often what may be asked.
pub struct Limiters {
    /// Test pushes, by the token they go to. A token is one phone, whatever
    /// it is registered as: the same token under a new device id has no
    /// allowance of its own.
    tests: Limiter<[u8; 32]>,
    /// Registrations, by their owner.
    puts: Limiter<String>,
    /// Registrations of a device the server did not know, by the address
    /// they came from.
    new_devices: Limiter<IpAddr>,
    /// Every request to a device, by the address it came from.
    requests: Limiter<IpAddr>,
    /// Wakes, by the hash of their ticket.
    wakes: Limiter<String>,
    /// Ordinary wakes, by the device woken: the hash of its token, as for
    /// `tests`.
    device_wakes: Budget<[u8; 32]>,
    /// Urgent wakes, by the device woken, counted apart from the others.
    device_urgent_wakes: Budget<[u8; 32]>,
    /// Every request to wake, by the address it came from. A private push
    /// server wakes the devices of all its users from one, so the room is
    /// wide; it is there for the one who tries tickets at random.
    wake_requests: Limiter<IpAddr>,
}

impl Limiters {
    const MINUTE: Duration = Duration::from_secs(60);
    const HOUR: Duration = Duration::from_secs(3600);
    /// Keys each limit remembers at one time. Past this many a key that is
    /// not remembered is told to come back later.
    const MAX_KEYS: usize = 100_000;

    pub fn new(tests_per_hour: u32) -> Self {
        Self {
            tests: Limiter::new(Self::HOUR, tests_per_hour as usize, Self::MAX_KEYS),
            puts: Limiter::new(Self::MINUTE, PUTS_PER_MINUTE, Self::MAX_KEYS),
            new_devices: Limiter::new(Self::HOUR, NEW_DEVICES_PER_HOUR, Self::MAX_KEYS),
            requests: Limiter::new(Self::MINUTE, REQUESTS_PER_MINUTE, Self::MAX_KEYS),
            wakes: Limiter::new(Self::MINUTE, WAKES_PER_MINUTE, Self::MAX_KEYS),
            device_wakes: Budget::new(
                (Self::MINUTE, DEVICE_WAKES_PER_MINUTE),
                (Self::HOUR, DEVICE_WAKES_PER_HOUR),
                Self::MAX_KEYS,
            ),
            device_urgent_wakes: Budget::new(
                (Self::MINUTE, DEVICE_URGENT_WAKES_PER_MINUTE),
                (Self::HOUR, DEVICE_URGENT_WAKES_PER_HOUR),
                Self::MAX_KEYS,
            ),
            wake_requests: Limiter::new(Self::MINUTE, WAKE_REQUESTS_PER_MINUTE, Self::MAX_KEYS),
        }
    }

    /// Counts one more test push to the token, or says after how long to
    /// come back.
    pub fn test(&self, token: &str) -> Result<(), Duration> {
        self.tests.take(token_key(token))
    }

    /// Counts one more registration of the owner.
    pub fn registration(&self, owner: &str) -> Result<(), Duration> {
        self.puts.take(owner.to_string())
    }

    /// Counts one more request from the address.
    pub fn request(&self, from: IpAddr) -> Result<(), Duration> {
        self.requests.take(network(from))
    }

    /// Counts one more new device from the address.
    pub fn new_device(&self, from: IpAddr) -> Result<(), Duration> {
        self.new_devices.take(network(from))
    }

    /// Counts one more wake of the ticket of this hash.
    pub fn wake(&self, ticket_hash: &str) -> Result<(), Duration> {
        self.wakes.take(ticket_hash.to_string())
    }

    /// Counts one more wake of the device of this token, whichever of its
    /// tickets it came by, or says which window is full. An urgent wake is
    /// counted in the urgent budget alone, and refused only when that one
    /// is spent.
    pub fn device_wake(&self, token: &str, urgent: bool) -> Result<(), Over> {
        self.device_wake_at(token, urgent, Instant::now())
    }

    fn device_wake_at(&self, token: &str, urgent: bool, now: Instant) -> Result<(), Over> {
        let budget = if urgent {
            &self.device_urgent_wakes
        } else {
            &self.device_wakes
        };
        budget.take_at(token_key(token), now)
    }

    /// Counts one more request to wake from the address.
    pub fn wake_request(&self, from: IpAddr) -> Result<(), Duration> {
        self.wake_requests.take(network(from))
    }
}

/// A token remembered by its hash: it may be thousands of bytes long.
fn token_key(token: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(digest.as_ref());
    key
}

/// What is counted as one client. An IPv6 address is counted by its first
/// 64 bits: the rest is the client's to choose, a new one for every request
/// if it likes.
fn network(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let prefix = u128::from(v6) & !u128::from(u64::MAX);
                IpAddr::V6(Ipv6Addr::from(prefix))
            }
        },
    }
}

pub fn router(api: Api) -> Router {
    let config = api.config.clone();
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/info", get(info))
        .route(
            "/v1/devices/{device_id}",
            put(devices::put).get(devices::get).delete(devices::delete),
        )
        .route("/v1/devices/{device_id}/test", post(devices::test));
    // Only a gateway issues tickets and wakes them; to every other server
    // they are routes it does not have.
    if config.gateway.accept {
        router = router
            .route("/v1/tickets", post(tickets::issue).get(tickets::list))
            .route("/v1/tickets/{ticket}", delete(tickets::delete))
            .route("/v1/gateway/wake", post(tickets::wake));
    }
    router
        .fallback(not_found)
        .with_state(api)
        .layer(DefaultBodyLimit::max(config.server.max_body_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(config.server.request_timeout_secs),
        ))
        .layer(middleware::from_fn(access_log))
}

async fn healthz() -> Json<vpush_proto::Health> {
    Json(version::health())
}

async fn info(State(api): State<Api>) -> Json<Info> {
    let limits = &api.config.limits;
    Json(Info {
        version: version::VERSION.to_string(),
        apps: api
            .providers
            .summary()
            .into_iter()
            .map(|(id, providers)| AppInfo {
                id,
                providers: providers.split(", ").map(str::to_string).collect(),
            })
            .collect(),
        relays: vpush_proto::RelayPolicy {
            policy: api.config.relays.policy.clone(),
            allowed: api.relays.urls(),
        },
        limits: Limits {
            devices_per_pubkey: limits.devices_per_pubkey,
            relays_per_device: limits.relays_per_device,
            groups_per_device: limits.groups_per_device,
            registration_days: limits.registration_days,
            test_per_hour: limits.test_per_hour,
            max_body_bytes: api.config.server.max_body_bytes as u32,
        },
        gateway: api.providers.gateway().cloned(),
        tickets: api.config.gateway.accept.then_some(TicketsInfo {
            days: api.config.tickets.days,
            per_device: api.config.tickets.per_device,
        }),
    })
}

async fn not_found(request: Request) -> Response {
    let id = request
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_default();
    error(StatusCode::NOT_FOUND, ErrorCode::NotFound, "no such route", id)
}

pub fn error(
    status: StatusCode,
    code: ErrorCode,
    message: impl Into<String>,
    request_id: String,
) -> Response {
    let body = ErrorBody {
        error: ErrorDetail {
            code,
            message: message.into(),
            server_time: (code == ErrorCode::AuthExpired).then(now),
        },
        request_id,
    };
    (status, Json(body)).into_response()
}

/// Ids only have to tell requests apart in a log, so a counter seeded by the
/// start time is enough.
pub fn next_request_id() -> String {
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = *SEED.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    });
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Spread the bits so neighbours do not look alike.
    let mixed = (seed ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:08x}", (mixed >> 32) as u32)
}

/// The path as the log writes it. A ticket in the path of its removal is cut
/// to its first 8 hex: whoever reads the log cannot wake with it.
fn shown_path(path: &str) -> String {
    match path.strip_prefix("/v1/tickets/") {
        Some(ticket) if ticket.len() > 8 => {
            let cut = (0..=8).rev().find(|i| ticket.is_char_boundary(*i)).unwrap_or(0);
            format!("/v1/tickets/{}…", &ticket[..cut])
        }
        _ => path.to_string(),
    }
}

/// One INFO line per request, and the request id on the way in and out.
async fn access_log(mut request: Request, next: Next) -> Response {
    let id = next_request_id();
    let method = request.method().clone();
    let path = shown_path(request.uri().path());
    request.extensions_mut().insert(RequestId(id.clone()));

    let started = Instant::now();
    let mut response = next.run(request).await;
    let status = response.status().as_u16();
    let ms = started.elapsed().as_millis() as u64;

    // The health check is asked every few seconds; it would drown the rest.
    if path == "/healthz" {
        tracing::debug!(request_id = %id, %method, path, status, ms, "request");
    } else {
        tracing::info!(request_id = %id, %method, path, status, ms, "request");
    }

    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_in_a_path_is_logged_by_its_first_8_hex() {
        let ticket = "7a".repeat(32);
        assert_eq!(shown_path(&format!("/v1/tickets/{ticket}")), "/v1/tickets/7a7a7a7a…");
        for kept in ["/v1/tickets", "/v1/tickets/7a7a7a7a", "/v1/devices/phone-0001", "/v1/gateway/wake"] {
            assert_eq!(shown_path(kept), kept);
        }
        assert_eq!(shown_path("/v1/tickets/жжжжжж"), "/v1/tickets/жжжж…");
    }

    #[test]
    fn the_test_limit_is_per_token() {
        let limits = Limiters::new(2);
        limits.test("token-of-the-phone").unwrap();
        limits.test("token-of-the-phone").unwrap();
        let wait = limits.test("token-of-the-phone").unwrap_err();
        assert!(wait <= Limiters::HOUR && wait > Duration::from_secs(3590), "{wait:?}");
        limits.test("token-of-the-tablet").unwrap();
    }

    #[test]
    fn an_owner_registers_ten_times_a_minute_and_no_more() {
        let limits = Limiters::new(3);
        for _ in 0..PUTS_PER_MINUTE {
            limits.registration("alice").unwrap();
        }
        let wait = limits.registration("alice").unwrap_err();
        assert!(wait <= Limiters::MINUTE && wait > Duration::from_secs(50), "{wait:?}");
        limits.registration("bob").unwrap();
    }

    #[test]
    fn requests_are_counted_by_the_address_and_an_ipv6_network_is_one_address() {
        let limits = Limiters::new(3);
        let from = |address: &str| limits.request(address.parse().unwrap());
        for _ in 0..REQUESTS_PER_MINUTE {
            from("203.0.113.7").unwrap();
        }
        let wait = from("203.0.113.7").unwrap_err();
        assert!(wait <= Limiters::MINUTE && wait > Duration::from_secs(50), "{wait:?}");
        from("203.0.113.8").unwrap();
        assert!(from("::ffff:203.0.113.7").is_err());
        from("2001:db8:1:2::1").unwrap();
        for _ in 1..REQUESTS_PER_MINUTE {
            from("2001:db8:1:2:aaaa::1").unwrap();
        }
        assert!(from("2001:db8:1:2::9").is_err());
    }

    #[test]
    fn new_devices_are_counted_by_the_address_and_an_ipv6_network_is_one_address() {
        let limits = Limiters::new(3);
        let from = |address: &str| limits.new_device(address.parse().unwrap());
        for _ in 0..NEW_DEVICES_PER_HOUR {
            from("203.0.113.7").unwrap();
        }
        let wait = from("203.0.113.7").unwrap_err();
        assert!(wait <= Limiters::HOUR && wait > Duration::from_secs(3590), "{wait:?}");
        from("203.0.113.8").unwrap();
        // The same address, written as IPv6 writes an IPv4 one.
        assert!(from("::ffff:203.0.113.7").is_err());

        // Whoever holds a /64 has as many addresses as it likes, and one allowance.
        for host in 0..NEW_DEVICES_PER_HOUR {
            from(&format!("2001:db8:1:2::{host:x}")).unwrap();
        }
        assert!(from("2001:db8:1:2:ffff:ffff:ffff:ffff").is_err());
        from("2001:db8:1:3::1").unwrap();
    }

    #[test]
    fn wakes_are_counted_by_the_address_and_by_the_device() {
        let limits = Limiters::new(3);
        let from = |address: &str| limits.wake_request(address.parse().unwrap());
        for _ in 0..WAKE_REQUESTS_PER_MINUTE {
            from("203.0.113.7").unwrap();
        }
        let wait = from("203.0.113.7").unwrap_err();
        assert!(wait <= Limiters::MINUTE && wait > Duration::from_secs(50), "{wait:?}");
        assert!(from("::ffff:203.0.113.7").is_err());
        from("203.0.113.8").unwrap();
        // A request to wake is not one to the devices: each has its own room.
        limits.request("203.0.113.7".parse().unwrap()).unwrap();

        for _ in 0..DEVICE_WAKES_PER_MINUTE {
            limits.device_wake("token-of-the-phone", false).unwrap();
        }
        assert!(matches!(
            limits.device_wake("token-of-the-phone", false),
            Err(Over::Short(wait)) if wait <= Limiters::MINUTE && wait > Duration::from_secs(50)
        ));
        limits.device_wake("token-of-the-tablet", false).unwrap();
    }

    /// One hostile server spends the ordinary hour of a device in ten
    /// minutes; a call from another still rings, within a budget of its own.
    #[test]
    fn a_flood_of_ordinary_wakes_never_silences_a_call() {
        let limits = Limiters::new(3);
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let phone = "token-of-the-phone";
        for minute in 0..(DEVICE_WAKES_PER_HOUR / DEVICE_WAKES_PER_MINUTE) as u64 {
            for i in 0..DEVICE_WAKES_PER_MINUTE as u64 {
                limits.device_wake_at(phone, false, at(minute * 60 + i)).unwrap();
            }
        }
        let late = at(11 * 60);
        assert!(
            matches!(limits.device_wake_at(phone, false, late), Err(Over::Long(_))),
            "{DEVICE_WAKES_PER_HOUR} ordinary wakes in the hour"
        );
        // The urgent budget is untouched, and has limits of its own.
        for _ in 0..DEVICE_URGENT_WAKES_PER_MINUTE {
            limits.device_wake_at(phone, true, late).unwrap();
        }
        assert!(matches!(limits.device_wake_at(phone, true, late), Err(Over::Short(_))));
        // Urgent wakes are not counted in the ordinary budget: they fill
        // their own hour and no more.
        for minute in 12..(12 + 5) {
            for _ in 0..DEVICE_URGENT_WAKES_PER_MINUTE {
                limits.device_wake_at(phone, true, at(minute * 60)).unwrap();
            }
        }
        assert!(matches!(
            limits.device_wake_at(phone, true, at(18 * 60)),
            Err(Over::Long(_))
        ));
    }
}
