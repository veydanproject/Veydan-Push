//! From an event on a relay to a push at the push service: the watcher and
//! the pipeline against real relays (in this process) and a push service
//! played by a mock.
//!
//! A test that counts pushes proves that no more came by a fence, not by a
//! quiet while (`Server::settled`): a marker sent through every line after
//! what the test published, the window of a chat waited out by its own
//! length, and every send the pipeline began seen to its end.

#[path = "support/key.rs"]
mod key;
#[path = "support/p8.rs"]
mod p8;

use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr::prelude::*;
use nostr_sdk::local_relay::{LocalRelay, QueryPolicy, QueryPolicyResult};
use serde_json::{json, Value};
use tokio::sync::watch;
use vpush_proto::Prefs;
use vpush_server::config::Config;
use vpush_server::counters::Counters;
use vpush_server::delivery::apns::{ApnsClient, SigningKey};
use vpush_server::delivery::fcm::{FcmClient, ServiceAccount};
use vpush_server::delivery::retry::RetryPolicy;
use vpush_server::delivery::{
    CheckFuture, Message, Outcome, ProviderKind, Providers, PushProvider, SendFuture, Target,
};
use vpush_server::pipeline::classify::mark_of;
use vpush_server::pipeline::{Dealt, Pipeline, Waking};
use vpush_server::relays::{normalize, RelayPolicy};
use vpush_server::auth::sha256_hex;
use vpush_server::store::{
    AllStore, Counts, Device, DeviceInput, DeviceLimits, Recipient, RelayPlan, Seen, SqliteStore,
    Store, Ticket, TicketGone, TicketInput, TicketRef, WatchKind, WatchStore, WatchedGroup,
    WatchedRelay,
};
use vpush_server::relay::{Tuning, Watch};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The one deadline of the waits: a loaded host makes everything slow, and
/// what does not happen within it is not going to.
const WAIT: Duration = Duration::from_secs(30);
/// A window of a chat that stays open for the whole of a test: what it
/// holds back is not pushed while the test looks.
const HOUR: Duration = Duration::from_secs(3600);

const APP: &str = "net.veydan.mobile";
/// An app whose devices are pushed to through APNs.
const IOS: &str = "net.veydan.ios";
const SEND: &str = "/v1/projects/veydan-test/messages:send";
const GROUP: &str = "1111111111111111111111111111111111111111111111111111111111111111";

struct Relay {
    relay: LocalRelay,
    url: String,
    asked: Asked,
    /// The address the server knows, when it is not the relay's own.
    door: Option<Door>,
}

/// How many times a relay was asked for direct messages. The server asks
/// for them in one request while the keys are few, so this is how many
/// times it made its requests anew.
#[derive(Debug, Clone, Default)]
struct Asked(Arc<AtomicUsize>);

impl QueryPolicy for Asked {
    fn admit_query<'a>(
        &'a self,
        query: &'a mut Filter,
        _addr: &'a SocketAddr,
    ) -> Pin<Box<dyn Future<Output = QueryPolicyResult> + Send + 'a>> {
        Box::pin(async move {
            if query.kinds.as_ref().is_some_and(|kinds| kinds.contains(&Kind::GiftWrap)) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            QueryPolicyResult::Accept
        })
    }
}

/// Left to itself, a local relay picks a random port by trying it, and a
/// test running next to this one may take that port a moment later. A port
/// the system hands out collides far less, and a relay that lost its port
/// anyway is replaced: it keeps the first port it was given.
async fn relay() -> Relay {
    let mut last = None;
    for _ in 0..5 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .unwrap()
            .port();
        let asked = Asked::default();
        let relay = LocalRelay::builder().port(port).query_policy(asked.clone()).build();
        match relay.run().await {
            Ok(()) => {
                let url = normalize(relay.url().await.as_str()).unwrap();
                return Relay { relay, url, asked, door: None };
            }
            Err(e) => last = Some(e),
        }
    }
    panic!("no port for the local relay: {last:?}");
}

/// A relay behind an address the test holds for as long as it runs, so it
/// can go away and come back at that address: a relay bound anew to the
/// port it had would find the port taken by whatever ran next to the test
/// in between. The door passes every connection on to the relay of the
/// moment; while there is none it closes them as they come.
async fn relay_behind_a_door() -> Relay {
    let mut relay = relay().await;
    let door = Door::open(&relay).await;
    relay.url = door.url.clone();
    relay.door = Some(door);
    relay
}

struct Door {
    url: String,
    state: Arc<Mutex<DoorState>>,
}

#[derive(Default)]
struct DoorState {
    /// The relay of the moment, by its own address.
    to: Option<SocketAddr>,
    passing: Vec<tokio::task::AbortHandle>,
}

impl Door {
    async fn open(relay: &Relay) -> Door {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = normalize(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(Mutex::new(DoorState { to: Some(Self::address(relay).await), passing: Vec::new() }));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else { return };
                let mut state = shared.lock().unwrap();
                let Some(to) = state.to else { continue };
                let passing = tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(to).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                state.passing.push(passing.abort_handle());
            }
        });
        Door { url, state }
    }

    async fn address(relay: &Relay) -> SocketAddr {
        let url = relay.relay.url().await;
        let url = url.as_str().trim_end_matches('/');
        url.trim_start_matches("ws://").parse().unwrap()
    }
}

impl Relay {
    async fn publish(&self, event: &Event) {
        self.relay.add_event(event.clone()).await.unwrap();
    }

    fn asked_for_direct_messages(&self) -> usize {
        self.asked.0.load(Ordering::Relaxed)
    }

    /// The relay goes off the line, as when it is restarted: what is
    /// connected is cut, and what connects is closed at once.
    fn goes_away(&self) {
        let door = self.door.as_ref().expect("a relay that goes away is behind a door");
        let mut state = door.state.lock().unwrap();
        state.to = None;
        for passing in state.passing.drain(..) {
            passing.abort();
        }
        self.relay.shutdown();
    }

    /// A relay at the same address again, with nothing stored.
    async fn comes_back(&self) -> Relay {
        let door = self.door.as_ref().expect("a relay that comes back is behind a door");
        let mut back = relay().await;
        let to = Door::address(&back).await;
        back.url = door.url.clone();
        back.door = Some(Door { url: door.url.clone(), state: Arc::clone(&door.state) });
        door.state.lock().unwrap().to = Some(to);
        back
    }
}

/// What the store of the server was asked, as far as the tests wait on it.
#[derive(Default)]
struct Noted {
    /// Every event the pipeline dealt with: it notes each, whatever
    /// becomes of it.
    events: Mutex<HashSet<String>>,
    /// Outcomes of pushes written down: the last step of every send.
    outcomes: AtomicUsize,
    /// VoIP tokens forgotten as dead: each follows an attempt that is
    /// not the last of its send (the ring goes on as an alert).
    voip_dropped: AtomicUsize,
    /// Times the watcher read the plan and gave the relays their parts.
    plans: AtomicUsize,
}

impl Noted {
    fn dealt_with(&self, id: &EventId) -> bool {
        self.events.lock().unwrap().contains(&id.to_hex())
    }
}

/// The store of the server as it is, and a note of what the tests wait on.
struct Witness {
    store: Arc<SqliteStore>,
    noted: Arc<Noted>,
}

#[async_trait]
impl Store for Witness {
    async fn put_device(&self, device: DeviceInput, limits: DeviceLimits) -> vpush_server::store::Result<()> {
        self.store.put_device(device, limits).await
    }
    async fn device(&self, pubkey: &str, device_id: &str) -> vpush_server::store::Result<Option<Device>> {
        self.store.device(pubkey, device_id).await
    }
    async fn devices_of(&self, pubkey: &str) -> vpush_server::store::Result<Vec<Device>> {
        self.store.devices_of(pubkey).await
    }
    async fn delete_device(&self, pubkey: &str, device_id: &str) -> vpush_server::store::Result<bool> {
        self.store.delete_device(pubkey, device_id).await
    }
    async fn forget_owner(&self, pubkey: &str) -> vpush_server::store::Result<u64> {
        self.store.forget_owner(pubkey).await
    }
    async fn record_outcome(
        &self,
        pubkey: &str,
        device_id: &str,
        token: &str,
        outcome: &str,
        now: u64,
    ) -> vpush_server::store::Result<()> {
        let written = self.store.record_outcome(pubkey, device_id, token, outcome, now).await;
        self.noted.outcomes.fetch_add(1, Ordering::SeqCst);
        written
    }
    async fn drop_voip_token(&self, pubkey: &str, device_id: &str, voip_token: &str) -> vpush_server::store::Result<bool> {
        let dropped = self.store.drop_voip_token(pubkey, device_id, voip_token).await;
        self.noted.voip_dropped.fetch_add(1, Ordering::SeqCst);
        dropped
    }
    async fn purge_expired(&self, now: u64) -> vpush_server::store::Result<u64> {
        self.store.purge_expired(now).await
    }
    async fn counts(&self) -> vpush_server::store::Result<Counts> {
        self.store.counts().await
    }
    async fn issue_ticket(
        &self,
        pubkey: &str,
        device_id: &str,
        ticket: &TicketInput,
        per_device: u32,
    ) -> vpush_server::store::Result<()> {
        self.store.issue_ticket(pubkey, device_id, ticket, per_device).await
    }
    async fn tickets_of(&self, pubkey: &str, now: u64) -> vpush_server::store::Result<Vec<Ticket>> {
        self.store.tickets_of(pubkey, now).await
    }
    async fn delete_ticket(&self, pubkey: &str, which: &TicketRef) -> vpush_server::store::Result<TicketGone> {
        self.store.delete_ticket(pubkey, which).await
    }
    async fn ticket_device(&self, hash: &str, now: u64) -> vpush_server::store::Result<Option<Recipient>> {
        self.store.ticket_device(hash, now).await
    }
    async fn ticket_woken(&self, hash: &str, now: u64) -> vpush_server::store::Result<()> {
        self.store.ticket_woken(hash, now).await
    }
    async fn purge_tickets(&self, now: u64) -> vpush_server::store::Result<u64> {
        self.store.purge_tickets(now).await
    }
}

#[async_trait]
impl WatchStore for Witness {
    async fn watch_plan(&self, now: u64) -> vpush_server::store::Result<Vec<RelayPlan>> {
        self.store.watch_plan(now).await
    }
    async fn baselined(&self, url: &str) -> vpush_server::store::Result<Vec<(WatchKind, String)>> {
        self.store.baselined(url).await
    }
    async fn set_baselined(&self, url: &str, targets: &[(WatchKind, String)], now: u64) -> vpush_server::store::Result<()> {
        self.store.set_baselined(url, targets, now).await
    }
    async fn unset_baselined(&self, url: &str, targets: &[(WatchKind, String)]) -> vpush_server::store::Result<()> {
        self.store.unset_baselined(url, targets).await
    }
    /// The watcher calls it in every reading of the plan, after the relays
    /// that are let go are gone and before the others are given their
    /// parts, which takes no wait: by the time the test runs again after
    /// it is counted, every relay has its part.
    async fn keep_baselined(&self, urls: &[String]) -> vpush_server::store::Result<u64> {
        let kept = self.store.keep_baselined(urls).await;
        self.noted.plans.fetch_add(1, Ordering::SeqCst);
        kept
    }
    async fn first_seen(&self, event_id: &str, what: Seen, now: u64) -> vpush_server::store::Result<bool> {
        self.noted.events.lock().unwrap().insert(event_id.to_string());
        self.store.first_seen(event_id, what, now).await
    }
    async fn purge_seen(&self, before: u64) -> vpush_server::store::Result<u64> {
        self.store.purge_seen(before).await
    }
    async fn dm_recipients(&self, pubkey: &str, now: u64) -> vpush_server::store::Result<Vec<Recipient>> {
        self.store.dm_recipients(pubkey, now).await
    }
    async fn group_keys(&self, group_id: &str, now: u64, limit: usize) -> vpush_server::store::Result<Vec<String>> {
        self.store.group_keys(group_id, now, limit).await
    }
    async fn group_recipients(
        &self,
        group_id: &str,
        keys: &[String],
        now: u64,
    ) -> vpush_server::store::Result<Vec<Recipient>> {
        self.store.group_recipients(group_id, keys, now).await
    }
    async fn relay_alive(&self, url: &str, now: u64) -> vpush_server::store::Result<()> {
        self.store.relay_alive(url, now).await
    }
    async fn relay_last_alive(&self, url: &str) -> vpush_server::store::Result<Option<u64>> {
        self.store.relay_last_alive(url).await
    }
}

/// Attempts of pushes begun and ended, of every service.
#[derive(Default)]
struct Flight {
    begun: AtomicUsize,
    ended: AtomicUsize,
    /// Attempts the service asked to have made again. None of the mocks
    /// does, and a send between two attempts is out of sight of the count
    /// above, so a fence that meets one says so.
    retried: AtomicUsize,
}

/// A push service as it is, with its attempts counted.
struct Counted {
    service: Arc<dyn PushProvider>,
    flight: Arc<Flight>,
}

/// Counts an attempt as ended however it ends, dropped half-way too.
struct Ended(Arc<Flight>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.ended.fetch_add(1, Ordering::SeqCst);
    }
}

impl PushProvider for Counted {
    fn kind(&self) -> ProviderKind {
        self.service.kind()
    }

    fn send<'a>(&'a self, target: &'a Target, message: &'a Message) -> SendFuture<'a> {
        // Counted when the attempt is made, before anything is awaited: a
        // send polled once has begun.
        self.flight.begun.fetch_add(1, Ordering::SeqCst);
        let ended = Ended(Arc::clone(&self.flight));
        let attempt = self.service.send(target, message);
        let flight = Arc::clone(&self.flight);
        Box::pin(async move {
            let _ended = ended;
            let attempt = attempt.await;
            if attempt.outcome == Outcome::Retry {
                flight.retried.fetch_add(1, Ordering::SeqCst);
            }
            attempt
        })
    }

    fn check<'a>(&'a self, target: &'a Target) -> CheckFuture<'a> {
        self.service.check(target)
    }
}

/// The server, without its HTTP side: a store, a watcher and a pipeline.
struct Server {
    store: Arc<SqliteStore>,
    /// The same store, as the watcher and the pipelines use it.
    witness: Arc<Witness>,
    noted: Arc<Noted>,
    flight: Arc<Flight>,
    /// The relays on the list of the server, by the address it knows: the
    /// way a fence reaches each line.
    relays: Mutex<Vec<(String, LocalRelay)>>,
    policy: Arc<RelayPolicy>,
    providers: Arc<Providers>,
    google: MockServer,
    /// Plays APNs, for the devices of `IOS`.
    apple: MockServer,
    waking: Waking,
    tuning: Tuning,
    counters: Arc<Counters>,
    running: Option<Running>,
}

struct Running {
    watch: Arc<Watch>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// A chat is held back for `window`, and a device is not: the tests that
/// are not about the cap of a device push as often as they like.
fn waking(window: Duration) -> Waking {
    Waking {
        chat_every: window,
        device_cap: 10_000,
        device_window: Duration::from_secs(60),
        call_cap: 10_000,
        call_window: Duration::from_secs(60),
    }
}

/// A change of what is watched is asked for at once, as the tests that are
/// not about the pace of asking expect.
fn at_once() -> Tuning {
    Tuning {
        ask_every: Duration::ZERO,
        ..Tuning::default()
    }
}

async fn server(relays: &[&Relay], window: Duration) -> Server {
    server_with(relays, waking(window), at_once()).await
}

async fn server_with(relays: &[&Relay], waking: Waking, tuning: Tuning) -> Server {
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1", "expires_in": 3600
        })))
        .mount(&google)
        .await;
    Mock::given(method("POST"))
        .and(path(SEND))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "name": "m/1" })))
        .mount(&google)
        .await;

    let allow: String = relays
        .iter()
        .map(|r| format!("[[relays.allow]]\nurl = \"{}\"\n", r.url))
        .collect();
    let config = Config::parse(&format!("public_url = \"http://localhost:1\"\n{allow}")).unwrap();

    let account = ServiceAccount::parse(
        &json!({
            "project_id": "veydan-test",
            "private_key": key::TEST_RSA_KEY,
            "client_email": "push@veydan-test.iam.gserviceaccount.com",
            "token_uri": format!("{}/token", google.uri()),
        })
        .to_string(),
    )
    .unwrap();
    let flight = Arc::new(Flight::default());
    let counted = |service: Arc<dyn PushProvider>| Arc::new(Counted { service, flight: Arc::clone(&flight) });
    let mut providers = Providers::default();
    providers.insert(APP, counted(Arc::new(FcmClient::new(account, &google.uri()).unwrap())));
    let apple = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&apple)
        .await;
    let key = SigningKey::parse(&p8::p8().0).unwrap();
    let apns = ApnsClient::new(key, "ABC123DEFG", "TEAM123456", IOS, Some(&apple.uri())).unwrap();
    providers.insert(IOS, counted(Arc::new(apns)));

    let store = Arc::new(SqliteStore::in_memory().await.unwrap());
    let noted = Arc::new(Noted::default());
    let mut server = Server {
        witness: Arc::new(Witness { store: Arc::clone(&store), noted: Arc::clone(&noted) }),
        store,
        noted,
        flight,
        relays: Mutex::new(relays.iter().map(|r| (r.url.clone(), r.relay.clone())).collect()),
        policy: Arc::new(RelayPolicy::from_config(&config).unwrap()),
        providers: Arc::new(providers),
        google,
        apple,
        waking,
        tuning,
        counters: Arc::new(Counters::default()),
        running: None,
    };
    server.start();
    server
}

impl Server {
    /// A pipeline over the server's store, pushing through its mock.
    fn pipeline(&self) -> Arc<Pipeline> {
        Pipeline::with_retry(
            self.witness.clone(),
            self.providers.clone(),
            self.waking,
            self.counters.clone(),
            RetryPolicy {
                max_attempts: 2,
                base: Duration::from_millis(20),
                max_wait: Duration::from_secs(1),
            },
        )
    }

    fn start(&mut self) {
        let store: Arc<dyn AllStore> = self.witness.clone();
        let watch = Watch::new();
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(vpush_server::relay::run(
            watch.clone(),
            store,
            self.policy.clone(),
            self.pipeline(),
            self.counters.clone(),
            self.tuning,
            stopped,
        ));
        self.running = Some(Running { watch, stop, task });
    }

    /// As when the server is taken down for a new release.
    async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.stop.send(true);
            let _ = running.task.await;
        }
    }

    fn watch(&self) -> &Arc<Watch> {
        &self.running.as_ref().unwrap().watch
    }

    /// A phone that watches direct messages and the group on every relay given.
    async fn register(&self, owner: &Keys, device: &str, relays: &[&Relay]) -> DeviceInput {
        let input = phone(owner, device, relays);
        self.put(input.clone()).await;
        input
    }

    /// Writes a registration down, as the API does when it has taken one.
    async fn put(&self, input: DeviceInput) {
        let roomy = DeviceLimits { per_owner: 10, total: 1000 };
        self.store.put_device(input, roomy).await.unwrap();
        self.watch().plan_changed();
    }

    /// How many events the pushes stand for, all together.
    async fn told(&self) -> u64 {
        let count = |push: &Value| match push["data"]["count"].as_str() {
            Some(count) => count.parse().unwrap(),
            None => 1,
        };
        self.pushes().await.iter().map(count).sum()
    }

    /// Was stock taken of a key or a group on the relay.
    async fn knows(&self, relay: &Relay, target: &str) -> bool {
        let known = self.store.baselined(&relay.url).await.unwrap();
        known.iter().any(|(_, known)| known == target)
    }

    /// Waits until stock was taken of everything the device watches.
    async fn watching(&self, owner: &Keys, relays: &[&Relay]) {
        let owner = owner.public_key().to_hex();
        for relay in relays {
            eventually("the relay is watched", || async {
                self.knows(relay, &owner).await && self.knows(relay, GROUP).await
            })
            .await;
        }
    }

    async fn pushes(&self) -> Vec<Value> {
        self.google
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == SEND)
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["message"].clone())
            .collect()
    }

    async fn pushes_are(&self, count: usize) -> Vec<Value> {
        eventually(&format!("{count} pushes"), || async { self.pushes().await.len() >= count }).await;
        self.settled().await;
        let pushes = self.pushes().await;
        assert_eq!(pushes.len(), count, "{pushes:#?}");
        pushes
    }

    /// Whatever the server was to push of what came before now is out.
    ///
    /// A line deals with what its relay sends one thing at a time, in the
    /// order it came, so when a marker published after everything else has
    /// been dealt with, so has everything before it; the marker is marked
    /// as not worth a push and pushes nothing itself. What the pipeline
    /// counted for the end of a window is pushed when the window ends, no
    /// later than its length from now, and the length is waited out when
    /// it is short; a window of an hour is open for the whole test. Then
    /// every send the pipeline began is followed to its end: a send is a
    /// task of its own, and a task woken or spawned before this one
    /// yielded runs before it resumes, so after two passes that find no
    /// attempt on its way and nothing begun since, none is.
    async fn settled(&self) {
        let plans = self.store.watch_plan(Timestamp::now().as_secs()).await.unwrap();
        let relays: Vec<(String, LocalRelay)> = self.relays.lock().unwrap().clone();
        let lines: Vec<String> = self.watch().health().into_iter().map(|h| h.url).collect();
        for (url, relay) in relays {
            // A relay nobody watches has no line, and nothing it holds
            // reaches the server.
            let Some(plan) = plans.iter().find(|p| p.url == url) else { continue };
            if !lines.contains(&url) {
                continue;
            }
            let marker = match (plan.dm.first(), plan.groups.first()) {
                (Some(key), _) => silent_wrap(PublicKey::from_hex(key).unwrap()),
                (None, Some(group)) => marked(group, &Keys::generate(), &[], &[&["silent", "1"]]),
                (None, None) => continue,
            };
            relay.add_event(marker.clone()).await.unwrap();
            eventually(&format!("the marker on {url} is dealt with"), || async {
                self.noted.dealt_with(&marker.id)
            })
            .await;
        }
        if self.waking.chat_every <= Duration::from_secs(5) {
            tokio::time::sleep(self.waking.chat_every).await;
        }
        let deadline = Instant::now() + WAIT;
        let mut quiet = 0;
        let mut last = None;
        while quiet < 2 {
            tokio::task::yield_now().await;
            let now = (
                self.flight.begun.load(Ordering::SeqCst),
                self.flight.ended.load(Ordering::SeqCst),
                self.noted.outcomes.load(Ordering::SeqCst) + self.noted.voip_dropped.load(Ordering::SeqCst),
            );
            let (begun, ended, written) = now;
            if begun == ended && ended == written && last.is_none_or(|last| last == now) {
                quiet += 1;
            } else {
                quiet = 0;
                if Instant::now() > deadline {
                    panic!("the sends never ended: begun {begun}, ended {ended}, outcomes and dropped VoIP tokens written {written}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            last = Some(now);
        }
        assert_eq!(self.flight.retried.load(Ordering::SeqCst), 0, "a push service asked for another attempt");
    }

    /// The watcher read the plan after this was called, and gave every
    /// relay its part.
    async fn plan_read(&self) {
        let before = self.noted.plans.load(Ordering::SeqCst);
        self.watch().plan_changed();
        eventually("the watcher reads the plan", || async {
            self.noted.plans.load(Ordering::SeqCst) > before
        })
        .await;
    }
}

/// A direct message to `to` marked as not worth a push.
fn silent_wrap(to: PublicKey) -> Event {
    EventBuilder::new(Kind::GiftWrap, "sealed")
        .tags([Tag::public_key(to), Tag::parse(["silent", "1"]).unwrap()])
        .custom_created_at(Timestamp::from_secs(Timestamp::now().as_secs() - 86_400))
        .finalize(&Keys::generate())
        .unwrap()
}

/// The registration of a phone that watches direct messages and the group
/// on every relay given.
fn phone(owner: &Keys, device: &str, relays: &[&Relay]) -> DeviceInput {
    DeviceInput {
        pubkey: owner.public_key().to_hex(),
        device_id: device.to_string(),
        app_id: APP.to_string(),
        provider: "fcm".to_string(),
        token: format!("token-of-{device}"),
        channel_json: None,
        app_version: None,
        prefs: Prefs::default(),
        author_key: Some(author_key(owner)),
        relays: relays
            .iter()
            .map(|r| WatchedRelay { url: r.url.clone(), dm: true, groups: true })
            .collect(),
        groups: vec![member_of(GROUP)],
        now: Timestamp::now().as_secs(),
        expires_at: Timestamp::now().as_secs() + 86_400,
        token_checked_at: None,
    }
}

async fn eventually<F, Fut>(what: &str, check: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    eventually_within(WAIT, what, check).await
}

async fn eventually_within<F, Fut>(within: Duration, what: &str, check: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    while !check().await {
        if Instant::now() > deadline {
            panic!("never happened within {within:?}: {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The key of the marks, as the messenger makes it. Here any 32 bytes that
/// belong to the owner will do.
fn author_key(owner: &Keys) -> String {
    owner.public_key().to_hex().chars().rev().collect()
}

/// The push key of a group, as the messenger makes it from the key of the
/// group. Here any 32 bytes the members of the group share will do.
fn push_key(group: &str) -> String {
    sha256_hex(format!("the key of {group}").as_bytes())
}

/// The push key of the key the group had before its key was changed.
fn push_key_before(group: &str) -> String {
    sha256_hex(format!("the key of {group} before it was changed").as_bytes())
}

/// What a member of a group registers: the group, and the push keys held.
fn with_keys(group: &str, keys: &[String]) -> WatchedGroup {
    WatchedGroup { id: group.to_string(), keys: keys.to_vec() }
}

fn member_of(group: &str) -> WatchedGroup {
    with_keys(group, &[push_key(group)])
}

/// A sealed direct message, as a relay sees it: signed by a key made for
/// it, dated somewhere in the last two days.
fn wrap(to: &Keys, tags: &[&[&str]]) -> Event {
    wrap_with(to, "sealed", tags)
}

fn wrap_with(to: &Keys, content: &str, tags: &[&[&str]]) -> Event {
    wrap_dated(to, content, tags, Timestamp::now().as_secs() - 86_400)
}

/// A sealed direct message that says it was written at `at`.
fn wrap_dated(to: &Keys, content: &str, tags: &[&[&str]], at: u64) -> Event {
    let mut all = vec![Tag::public_key(to.public_key())];
    all.extend(tags.iter().map(|t| Tag::parse(t.iter().copied()).unwrap()));
    EventBuilder::new(Kind::GiftWrap, content)
        .tags(all)
        .custom_created_at(Timestamp::from_secs(at))
        .finalize(&Keys::generate())
        .unwrap()
}

/// The event a push carries, opened.
fn carried(data: &Value) -> Value {
    serde_json::from_str(data["event"].as_str().expect("the push carries the event")).unwrap()
}

/// An event of the group, written by `author`.
fn group_event(author: &Keys, tags: &[&[&str]]) -> Event {
    event_of(GROUP, author, tags)
}

/// An event of the group `group`, written by `author`, a member: it carries
/// the mark of the group's push key.
fn event_of(group: &str, author: &Keys, tags: &[&[&str]]) -> Event {
    marked(group, author, &[push_key(group)], tags)
}

/// An event of the group `group`, written by `author`, with the marks of
/// these push keys on it: what a holder of each of them would have put.
fn marked(group: &str, author: &Keys, push_keys: &[String], tags: &[&[&str]]) -> Event {
    let signer = Keys::generate();
    let signed_by = signer.public_key().to_hex();
    let mark = mark_of(&author_key(author), &signed_by).unwrap();
    let mut all = vec![
        Tag::parse(["h", group]).unwrap(),
        Tag::parse(["k", "key-1"]).unwrap(),
        Tag::parse(["vp", mark.as_str()]).unwrap(),
    ];
    for key in push_keys {
        let mark = mark_of(key, &signed_by).unwrap();
        all.push(Tag::parse(["gp", mark.as_str()]).unwrap());
    }
    all.extend(tags.iter().map(|t| Tag::parse(t.iter().copied()).unwrap()));
    EventBuilder::new(Kind::from(9u16), "sealed").tags(all).finalize(&signer).unwrap()
}

const SHORT: Duration = Duration::from_millis(100);

#[tokio::test]
async fn a_direct_message_becomes_a_push_that_carries_it() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    let message = wrap(&alice, &[]);
    r.publish(&message).await;

    let pushes = server.pushes_are(1).await;
    assert_eq!(pushes[0]["token"], "token-of-phone");
    let data = &pushes[0]["data"];
    assert_eq!(data["v"], "2");
    assert_eq!(data["type"], "dm");
    assert_eq!(carried(data), serde_json::to_value(&message).unwrap());
    // The event is there, so where to find it is not; and no text: the
    // device writes its own.
    for absent in ["event_id", "relay", "count", "title", "body"] {
        assert!(data.get(absent).is_none(), "{absent}: {data}");
    }
    assert!(data["trace"].as_str().unwrap().len() >= 8);
    assert!(pushes[0].get("notification").is_none());
    assert_eq!(pushes[0]["android"]["priority"], "HIGH");

    let device = server.store.device(&alice.public_key().to_hex(), "phone").await.unwrap().unwrap();
    assert_eq!(device.last_outcome.as_deref(), Some("delivered"));
    assert_eq!(server.watch().status(&r.url), vpush_proto::RelayStatus::Ok);
}

#[tokio::test]
async fn a_message_too_big_for_a_push_is_named_and_left_on_the_relay() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    let message = wrap_with(&alice, &"x".repeat(5_000), &[]);
    r.publish(&message).await;

    let pushes = server.pushes_are(1).await;
    let data = &pushes[0]["data"];
    assert_eq!(data["type"], "dm");
    assert_eq!(data["event_id"], message.id.to_hex());
    assert_eq!(data["relay"], r.url);
    assert!(data.get("event").is_none(), "{data}");
}

#[tokio::test]
async fn what_was_on_the_relay_before_the_watch_began_is_not_pushed() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    for _ in 0..3 {
        r.publish(&wrap(&alice, &[])).await;
    }
    r.publish(&group_event(&Keys::generate(), &[])).await;

    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;
    server.pushes_are(0).await;

    let fresh = wrap(&alice, &[]);
    r.publish(&fresh).await;
    let pushes = server.pushes_are(1).await;
    assert_eq!(carried(&pushes[0]["data"])["id"], fresh.id.to_hex());
}

#[tokio::test]
async fn what_is_marked_as_not_worth_a_push_is_not_pushed() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    // A copy for the author's other devices, a signal, an operation of a group.
    r.publish(&wrap(&alice, &[&["silent", "1"]])).await;
    r.publish(&group_event(&Keys::generate(), &[&["silent", "1"]])).await;
    // And something for somebody who is not registered.
    r.publish(&wrap(&Keys::generate(), &[])).await;
    let message = wrap(&alice, &[]);
    r.publish(&message).await;

    let pushes = server.pushes_are(1).await;
    assert_eq!(carried(&pushes[0]["data"])["id"], message.id.to_hex());
}

#[tokio::test]
async fn one_event_on_two_relays_is_one_push() {
    let (a, b) = (relay().await, relay().await);
    let server = server(&[&a, &b], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&a, &b]).await;
    server.watching(&alice, &[&a, &b]).await;

    let message = wrap(&alice, &[]);
    a.publish(&message).await;
    b.publish(&message).await;

    server.pushes_are(1).await;
}

#[tokio::test]
async fn every_device_of_the_owner_is_told() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.register(&alice, "tablet", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    r.publish(&wrap(&alice, &[])).await;

    let mut tokens: Vec<_> = server
        .pushes_are(2)
        .await
        .iter()
        .map(|p| p["token"].as_str().unwrap().to_string())
        .collect();
    tokens.sort();
    assert_eq!(tokens, ["token-of-phone", "token-of-tablet"]);
}

/// The event carries both marks: the one of the group (`gp`), by which bob
/// and carol are told, and the one of the author (`vp`), by which alice,
/// who holds the key of the group as they do, is not.
#[tokio::test]
async fn a_group_message_is_told_to_everybody_but_its_author() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let (alice, bob, carol) = (Keys::generate(), Keys::generate(), Keys::generate());
    server.register(&alice, "alice-phone", &[&r]).await;
    server.register(&alice, "alice-tablet", &[&r]).await;
    server.register(&bob, "bob-phone", &[&r]).await;
    server.register(&carol, "carol-phone", &[&r]).await;
    for owner in [&alice, &bob, &carol] {
        server.watching(owner, &[&r]).await;
    }

    let message = group_event(&alice, &[]);
    r.publish(&message).await;

    let pushes = server.pushes_are(2).await;
    let told: std::collections::BTreeMap<_, _> = pushes
        .iter()
        .map(|p| (p["token"].as_str().unwrap(), p["data"].clone()))
        .collect();
    assert!(!told.contains_key("token-of-alice-phone"), "the author is not told of her own message");
    assert!(!told.contains_key("token-of-alice-tablet"), "on none of her devices");

    for token in ["token-of-bob-phone", "token-of-carol-phone"] {
        let data = &told[token];
        assert_eq!(data["type"], "group");
        assert_eq!(data["group_id"], GROUP);
        assert_eq!(carried(data), serde_json::to_value(&message).unwrap());
        assert!(data.get("title").is_none() && data.get("group_name").is_none(), "{data}");
    }
}

/// The tokens the pushes went to, sorted.
fn tokens(pushes: &[Value]) -> Vec<&str> {
    let mut tokens: Vec<_> = pushes.iter().map(|p| p["token"].as_str().unwrap()).collect();
    tokens.sort();
    tokens
}

/// The id of a group is on every one of its events, in the open: anybody
/// can sign an event that names it. Without the mark of the group such an
/// event wakes nobody, and is carried to nobody.
#[tokio::test]
async fn a_group_event_without_the_mark_of_the_group_is_told_to_nobody_and_counted() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let (bob, stranger) = (Keys::generate(), Keys::generate());
    server.register(&bob, "bob-phone", &[&r]).await;
    server.watching(&bob, &[&r]).await;

    // No mark at all, as an app older than the mark writes and as whoever
    // knows nothing of it does; and the mark of a key the stranger made up.
    let bare = marked(GROUP, &stranger, &[], &[]);
    let forged = marked(GROUP, &stranger, &[push_key("a guess")], &[]);
    r.publish(&bare).await;
    r.publish(&forged).await;
    eventually("both are counted", || async { server.counters.group_events_unmarked.get() >= 2 }).await;
    server.pushes_are(0).await;

    // They are noted, and not counted a second time when a relay gives
    // them again.
    let pipeline = server.pipeline();
    let now = Timestamp::now().as_secs();
    for event in [&bare, &forged] {
        assert!(!server.store.first_seen(&event.id.to_hex(), Seen::Pushed, now).await.unwrap());
        assert_eq!(pipeline.event(&r.url, event, false).await, Dealt::Unmarked);
    }
    assert_eq!(server.counters.group_events_unmarked.get(), 2);

    // A member writes, and bob is told.
    let message = event_of(GROUP, &Keys::generate(), &[]);
    r.publish(&message).await;
    let pushes = server.pushes_are(1).await;
    assert_eq!(pushes[0]["token"], "token-of-bob-phone");
    assert_eq!(carried(&pushes[0]["data"]), serde_json::to_value(&message).unwrap());
    assert_eq!(server.counters.group_events_unmarked.get(), 2);
}

/// What stands in a `gp` tag and is no mark is passed over, and no more
/// than the first four tags are read.
#[tokio::test]
async fn what_is_no_mark_is_passed_over_and_the_fifth_tag_is_not_read() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let bob = Keys::generate();
    server.register(&bob, "bob-phone", &[&r]).await;
    let pipeline = server.pipeline();

    // An event of the group with the `gp` tags `tags` makes of the mark of
    // the group.
    let with = |tags: fn(String) -> Vec<String>| {
        let signer = Keys::generate();
        let mark = mark_of(&push_key(GROUP), &signer.public_key().to_hex()).unwrap();
        let mut all = vec![Tag::parse(["h", GROUP]).unwrap()];
        all.extend(tags(mark).iter().map(|value| Tag::parse(["gp", value.as_str()]).unwrap()));
        EventBuilder::new(Kind::from(9u16), "sealed").tags(all).finalize(&signer).unwrap()
    };
    fn no() -> String {
        "not-a-mark".to_string()
    }
    let told = Dealt::Pushed { now: 1, later: 0, own: 0, over: 0 };

    let shouted = with(|mark| vec![mark.to_uppercase()]);
    assert_eq!(pipeline.event(&r.url, &shouted, false).await, Dealt::Unmarked);
    let cut = with(|mark| vec![mark[..15].to_string()]);
    assert_eq!(pipeline.event(&r.url, &cut, false).await, Dealt::Unmarked);
    let fifth = with(|mark| vec![no(), no(), no(), no(), mark]);
    assert_eq!(pipeline.event(&r.url, &fifth, false).await, Dealt::Unmarked);
    assert_eq!(server.counters.group_events_unmarked.get(), 3);
    server.pushes_are(0).await;

    let fourth = with(|mark| vec![no(), no(), no(), mark]);
    assert_eq!(pipeline.event(&r.url, &fourth, false).await, told);
    server.pushes_are(1).await;
}

/// The key of a group was changed. For a week its events carry two marks:
/// of the key of now, and of the one before, for the phones that slept
/// through the change.
#[tokio::test]
async fn after_a_change_of_the_key_an_event_is_told_by_the_marks_it_carries() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let (awake, asleep) = (Keys::generate(), Keys::generate());
    let (now, before) = (push_key(GROUP), push_key_before(GROUP));

    // One phone registered after the change, with both keys. The other
    // has not been opened since, and the server has the key before only.
    let mut input = phone(&awake, "awake-phone", &[&r]);
    input.groups = vec![with_keys(GROUP, &[now.clone(), before.clone()])];
    server.put(input).await;
    let mut input = phone(&asleep, "asleep-phone", &[&r]);
    input.groups = vec![with_keys(GROUP, std::slice::from_ref(&before))];
    server.put(input).await;
    for owner in [&awake, &asleep] {
        server.watching(owner, &[&r]).await;
    }

    // Within the week: both marks, and both phones are told, each once.
    let both = marked(GROUP, &Keys::generate(), &[now.clone(), before], &[]);
    r.publish(&both).await;
    let pushes = server.pushes_are(2).await;
    assert_eq!(tokens(&pushes), ["token-of-asleep-phone", "token-of-awake-phone"]);

    // After it: the mark of the key of now only. The phone that never
    // registered that key is not told.
    let new_only = marked(GROUP, &Keys::generate(), &[now], &[]);
    r.publish(&new_only).await;
    let pushes = server.pushes_are(3).await;
    assert_eq!(pushes[2]["token"], "token-of-awake-phone");
    assert_eq!(carried(&pushes[2]["data"])["id"], new_only.id.to_hex());
    assert_eq!(server.counters.group_events_unmarked.get(), 0, "both events came from members");
}

/// Whoever knows the id of a group can register for it. Without the key of
/// the group they register a key of their own making, and are told nothing.
#[tokio::test]
async fn a_device_that_registered_a_wrong_key_for_a_group_is_told_nothing_of_it() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let (bob, mallory) = (Keys::generate(), Keys::generate());
    server.register(&bob, "bob-phone", &[&r]).await;
    let made_up = push_key("a guess");
    let mut input = phone(&mallory, "mallory-phone", &[&r]);
    input.groups = vec![with_keys(GROUP, std::slice::from_ref(&made_up))];
    server.put(input).await;
    for owner in [&bob, &mallory] {
        server.watching(owner, &[&r]).await;
    }

    let message = event_of(GROUP, &Keys::generate(), &[]);
    r.publish(&message).await;
    let pushes = server.pushes_are(1).await;
    assert_eq!(pushes[0]["token"], "token-of-bob-phone");

    // And what somebody marks with the made-up key reaches the one who
    // made it up, and no member.
    r.publish(&marked(GROUP, &Keys::generate(), std::slice::from_ref(&made_up), &[])).await;
    let pushes = server.pushes_are(2).await;
    assert_eq!(pushes[1]["token"], "token-of-mallory-phone");
}

/// A device registered by 0.2.2 has its groups and no key of any: it waits
/// for the app to register anew.
#[tokio::test]
async fn a_group_nobody_registered_a_key_for_is_for_nobody() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let bob = Keys::generate();
    let mut input = phone(&bob, "bob-phone", &[&r]);
    input.groups = vec![with_keys(GROUP, &[])];
    server.put(input).await;
    let pipeline = server.pipeline();

    let message = event_of(GROUP, &Keys::generate(), &[]);
    assert_eq!(pipeline.event(&r.url, &message, false).await, Dealt::Nobody);
    assert_eq!(
        server.counters.group_events_unmarked.get(),
        0,
        "there is no key to hold the mark against: the event is not called a stranger's"
    );
    // Direct messages are pushed to such a device as before.
    let dealt = pipeline.event(&r.url, &wrap(&bob, &[]), false).await;
    assert_eq!(dealt, Dealt::Pushed { now: 1, later: 0, own: 0, over: 0 });
    let pushes = server.pushes_are(1).await;
    assert_eq!(pushes[0]["data"]["type"], "dm");

    // The app registers anew, with the key, and is told of the group again.
    server.register(&bob, "bob-phone", &[&r]).await;
    let next = event_of(GROUP, &Keys::generate(), &[]);
    assert_eq!(
        pipeline.event(&r.url, &next, false).await,
        Dealt::Pushed { now: 1, later: 0, own: 0, over: 0 }
    );
    server.pushes_are(2).await;
}

/// Anybody can register keys of their own making for a group they know
/// the id of. An event is held against sixty-four keys and no more, and
/// the key the members hold must not be the one that is left out.
#[tokio::test]
async fn keys_made_up_for_a_group_do_not_crowd_out_the_key_of_its_members() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    // Two members. Their key is the last of all by its letters.
    let members_key = "ff".repeat(32);
    for n in 0..2 {
        let mut input = phone(&Keys::generate(), &format!("member-{n}"), &[&r]);
        input.groups = vec![with_keys(GROUP, std::slice::from_ref(&members_key))];
        server.put(input).await;
    }
    // Seventy strangers, each with a key of its own making.
    let made_up = |n: usize| format!("{n:064x}");
    for n in 0..70 {
        let mut input = phone(&Keys::generate(), &format!("stranger-{n}"), &[&r]);
        input.groups = vec![with_keys(GROUP, &[made_up(n)])];
        server.put(input).await;
    }
    let pipeline = server.pipeline();

    let message = marked(GROUP, &Keys::generate(), std::slice::from_ref(&members_key), &[]);
    assert_eq!(
        pipeline.event(&r.url, &message, false).await,
        Dealt::Pushed { now: 2, later: 0, own: 0, over: 0 }
    );
    // Of the made-up keys, each held by one device, the first sixty-three
    // by their letters are tried, and the rest are not.
    let tried = marked(GROUP, &Keys::generate(), &[made_up(62)], &[]);
    assert_eq!(
        pipeline.event(&r.url, &tried, false).await,
        Dealt::Pushed { now: 1, later: 0, own: 0, over: 0 }
    );
    let left_out = marked(GROUP, &Keys::generate(), &[made_up(63)], &[]);
    assert_eq!(pipeline.event(&r.url, &left_out, false).await, Dealt::Unmarked);
    server.pushes_are(3).await;
}

/// Messages of one chat while its window is open, given to a pipeline
/// one after another: the first is pushed, the ones after it are counted.
async fn a_burst(server: &Server, r: &Relay, to: &Keys, tags: &[&[&str]], messages: usize) -> Vec<String> {
    let pipeline = server.pipeline();
    let mut ids = Vec::new();
    for n in 0..messages {
        let message = wrap(to, tags);
        ids.push(message.id.to_hex());
        let (now, later) = if n == 0 { (1, 0) } else { (0, 1) };
        let dealt = pipeline.event(&r.url, &message, false).await;
        assert_eq!(dealt, Dealt::Pushed { now, later, own: 0, over: 0 }, "message {n}");
    }
    ids
}

#[tokio::test]
async fn a_burst_wakes_the_phone_twice_and_the_second_push_counts_the_rest() {
    // The window is an hour: the messages after the first are held back for
    // as long as the test looks, and what is pushed went at once.
    let r = relay().await;
    let server = server(&[&r], HOUR).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    let mut ids = Vec::new();
    for _ in 0..6 {
        let message = wrap(&alice, &[]);
        ids.push(message.id.to_hex());
        r.publish(&message).await;
    }
    // Another chat is not held back by this one.
    let other = group_event(&Keys::generate(), &[]);
    r.publish(&other).await;

    let first = server.pushes_are(2).await;
    let at_once = first.iter().map(|p| &p["data"]).find(|d| d["type"] == "dm").unwrap();
    assert_eq!(carried(at_once)["id"], ids[0], "the first of the burst does not wait");
    assert!(at_once.get("count").is_none(), "{at_once}");
    let group = first.iter().map(|p| &p["data"]).find(|d| d["type"] == "group").unwrap();
    assert_eq!(carried(group)["id"], other.id.to_hex());

    // A window of two seconds, which ends while the test waits: the five
    // counted are told in one push at its end.
    let r = relay().await;
    let server = crate::server(&[&r], Duration::from_secs(2)).await;
    server.register(&alice, "phone", &[&r]).await;
    let ids = a_burst(&server, &r, &alice, &[], 6).await;

    let pushes = server.pushes_are(2).await;
    let at_once = pushes.iter().map(|p| &p["data"]).find(|d| d.get("count").is_none()).unwrap();
    assert_eq!(carried(at_once)["id"], ids[0]);
    let last = pushes.iter().map(|p| &p["data"]).find(|d| d.get("count").is_some()).unwrap();
    assert_eq!(last["type"], "dm");
    assert_eq!(last["count"], "5");
    // Five messages cannot be shown from one event: the push names one of
    // the counted, and the phone takes them from the relay.
    assert!(last.get("event").is_none(), "{last}");
    assert_eq!(last["relay"], r.url);
    let named = last["event_id"].as_str().unwrap();
    assert!(ids.iter().any(|id| id == named), "{named} is one of the messages");
    assert_ne!(carried(at_once)["id"], named, "and not the one pushed at once");
}

/// Both go at once: in one lane the message would wait for the note's
/// window, an hour here.
#[tokio::test]
async fn a_channel_note_and_a_message_use_separate_push_lanes() {
    let r = relay().await;
    let server = server(&[&r], HOUR).await;
    let owner = Keys::generate();
    server.register(&owner, "phone", &[&r]).await;
    server.watching(&owner, &[&r]).await;

    let note = wrap(&owner, &[&["channel", "1"]]);
    let message = wrap(&owner, &[]);
    r.publish(&note).await;
    r.publish(&message).await;
    let pushes = server.pushes_are(2).await;
    let channel = pushes.iter().find(|p| p["data"]["channel"] == "1").unwrap();
    let dm = pushes.iter().find(|p| p["data"].get("channel").is_none()).unwrap();
    assert_eq!(channel["data"]["type"], "dm");
    assert_eq!(carried(&channel["data"])["id"], note.id.to_hex());
    assert_eq!(carried(&dm["data"])["id"], message.id.to_hex());
    assert!(channel["data"].get("count").is_none());
    assert!(dm["data"].get("count").is_none());
    assert_ne!(channel["android"]["collapse_key"], dm["android"]["collapse_key"]);
}

#[tokio::test]
async fn three_channel_notes_count_only_channel_notes() {
    // While the window is open (an hour) the two after the first are held,
    // and a message of the owner's other lane is not counted with them.
    let r = relay().await;
    let server = server(&[&r], HOUR).await;
    let owner = Keys::generate();
    server.register(&owner, "phone", &[&r]).await;
    server.watching(&owner, &[&r]).await;

    let notes: Vec<Event> = (0..3).map(|_| wrap(&owner, &[&["channel", "1"]])).collect();
    for note in &notes {
        r.publish(note).await;
    }
    let pushes = server.pushes_are(1).await;
    assert_eq!(pushes[0]["data"]["channel"], "1");
    assert_eq!(carried(&pushes[0]["data"])["id"], notes[0].id.to_hex());
    r.publish(&wrap(&owner, &[])).await;
    let pushes = server.pushes_are(2).await;
    assert!(pushes[1]["data"].get("channel").is_none() && pushes[1]["data"].get("count").is_none());

    // When the window ends (two seconds) the two are told in one push.
    let r = relay().await;
    let server = crate::server(&[&r], Duration::from_secs(2)).await;
    server.register(&owner, "phone", &[&r]).await;
    a_burst(&server, &r, &owner, &[&["channel", "1"]], 3).await;
    let pushes = server.pushes_are(2).await;
    assert!(pushes.iter().all(|p| p["data"]["type"] == "dm" && p["data"]["channel"] == "1"));
    let first = pushes.iter().find(|p| p["data"].get("count").is_none()).unwrap();
    let counted = pushes.iter().find(|p| p["data"]["count"] == "2").unwrap();
    assert_eq!(first["android"]["collapse_key"], counted["android"]["collapse_key"]);
    assert!(counted["data"].get("event").is_none());
}

#[tokio::test]
async fn ordinary_messages_still_share_one_push_lane() {
    // One lane: while the window of the first is open (an hour) the second
    // waits for it.
    let r = relay().await;
    let server = server(&[&r], HOUR).await;
    let owner = Keys::generate();
    server.register(&owner, "phone", &[&r]).await;
    server.watching(&owner, &[&r]).await;
    let first = wrap(&owner, &[]);
    r.publish(&first).await;
    r.publish(&wrap(&owner, &[])).await;
    let pushes = server.pushes_are(1).await;
    assert_eq!(carried(&pushes[0]["data"])["id"], first.id.to_hex());

    // And when the window ends (two seconds) the second is told by itself,
    // under the same key.
    let r = relay().await;
    let server = crate::server(&[&r], Duration::from_secs(2)).await;
    server.register(&owner, "phone", &[&r]).await;
    server.watching(&owner, &[&r]).await;

    for _ in 0..2 {
        r.publish(&wrap(&owner, &[])).await;
    }
    let pushes = server.pushes_are(2).await;
    assert!(pushes.iter().all(|p| p["data"]["type"] == "dm" && p["data"].get("channel").is_none()));
    assert!(pushes.iter().all(|p| p["data"].get("count").is_none()));
    assert_eq!(pushes.iter().filter(|p| p["data"].get("event").is_some()).count(), 2);
    assert_eq!(pushes[0]["android"]["collapse_key"], pushes[1]["android"]["collapse_key"]);
}

/// An invitation to a call (`["call", "1"]` on the outside) is pushed at
/// once, however recently the chat woke the phone, lives a minute at the
/// push service and replaces the call before it; the push is otherwise a
/// message's, which the phone opens to find the call.
#[tokio::test]
async fn a_call_goes_past_the_throttle_and_lives_a_minute() {
    let r = relay().await;
    let server = server(&[&r], HOUR).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    // A message wakes the phone and starts the window of the chat; a
    // second message would be counted and told after the window, an hour
    // from now: the calls that are pushed did not wait for it.
    r.publish(&wrap(&alice, &[])).await;
    let invite = wrap(&alice, &[&["call", "1"]]);
    r.publish(&invite).await;
    let again = wrap(&alice, &[&["call", "1"]]);
    r.publish(&again).await;

    let pushes = server.pushes_are(3).await;
    let calls: Vec<&Value> = pushes.iter().filter(|p| p["android"]["ttl"] == "60s").collect();
    assert_eq!(calls.len(), 2, "{pushes:#?}");
    let ids: Vec<Value> = calls.iter().map(|p| carried(&p["data"])["id"].clone()).collect();
    assert!(ids.contains(&json!(invite.id.to_hex())) && ids.contains(&json!(again.id.to_hex())), "{ids:?}");
    for push in &calls {
        assert_eq!(push["data"]["type"], "dm");
        assert!(push["data"].get("count").is_none());
        assert_eq!(push["data"]["call"], "1", "marked as a call for the phone");
        assert_eq!(push["android"]["priority"], "HIGH");
    }
    // One call replaces the one before it, and neither replaces the last
    // word about the chat (the keys are masked, as every key is).
    let message = pushes.iter().find(|p| p["android"]["ttl"] != "60s").unwrap();
    assert_eq!(message["android"]["ttl"], "86400s");
    assert!(message["data"].get("call").is_none());
    assert_eq!(calls[0]["android"]["collapse_key"], calls[1]["android"]["collapse_key"]);
    assert_ne!(calls[0]["android"]["collapse_key"], message["android"]["collapse_key"]);
}

/// The mark of a call is on the outside of the wrap, where anyone can put
/// it, and a call goes past the throttle and the cap: a stranger who
/// publishes a flood of "calls" rings the phone a few times a minute and
/// no more. What comes over the budget is dropped, not told later.
#[tokio::test]
async fn a_flood_of_calls_rings_the_phone_a_few_times_a_minute() {
    let r = relay().await;
    let mut waking = waking(HOUR);
    waking.call_cap = 2;
    let server = server_with(&[&r], waking, at_once()).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    for _ in 0..5 {
        r.publish(&wrap(&alice, &[&["call", "1"]])).await;
    }
    let pushes = server.pushes_are(2).await;
    for push in &pushes {
        assert_eq!(push["android"]["ttl"], "60s");
        assert_eq!(push["data"]["call"], "1");
    }
    // A message is not a call: it is pushed as ever, with the chat's
    // window fresh, and no `sync` push tells of the dropped calls.
    r.publish(&wrap(&alice, &[])).await;
    let pushes = server.pushes_are(3).await;
    assert_eq!(pushes[2]["data"]["type"], "dm");
    assert!(pushes[2]["data"].get("call").is_none());
}

/// The end of a ringing (`["call", "0"]`: the call was taken on another
/// device, declined, or given up) goes the way of a call: at once, for a
/// minute, marked `call` for the phone (which opens it and sees the end),
/// under a collapse key of its own, so that it never takes the place of
/// a waiting ring (of another call: the server cannot tell). It is on a
/// budget of its own: the ends do not use up the rings of a device, nor
/// a flood of ends the real ones.
#[tokio::test]
async fn the_end_of_a_ringing_goes_like_a_call_on_its_own_budget() {
    let r = relay().await;
    // A window of an hour: what is pushed went at once.
    let mut waking = waking(HOUR);
    waking.call_cap = 1;
    let server = server_with(&[&r], waking, at_once()).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    let ring = wrap(&alice, &[&["call", "1"]]);
    r.publish(&ring).await;
    let end = wrap(&alice, &[&["call", "0"]]);
    r.publish(&end).await;
    // A second ring and a second end: each over its own cap, dropped.
    r.publish(&wrap(&alice, &[&["call", "1"]])).await;
    r.publish(&wrap(&alice, &[&["call", "0"]])).await;

    let pushes = server.pushes_are(2).await;
    let ids: Vec<Value> = pushes.iter().map(|p| carried(&p["data"])["id"].clone()).collect();
    assert!(ids.contains(&json!(ring.id.to_hex())) && ids.contains(&json!(end.id.to_hex())), "{ids:?}");
    for push in &pushes {
        assert_eq!(push["data"]["type"], "dm");
        assert_eq!(push["data"]["call"], "1", "marked as a call for the phone, as the ring is");
        assert!(push["data"].get("count").is_none());
        assert_eq!(push["android"]["ttl"], "60s");
        assert_eq!(push["android"]["priority"], "HIGH");
    }
    // The end is in the place of the end before it, never of a ring: the
    // server cannot tell which call it ends, and under one key the end of
    // one call would silence the waiting ring of another.
    let key_of = |id: &str| {
        pushes.iter().find(|p| carried(&p["data"])["id"] == json!(id)).map(|p| p["android"]["collapse_key"].clone()).unwrap()
    };
    assert_ne!(key_of(&ring.id.to_hex()), key_of(&end.id.to_hex()), "the end must not take the place of a ring");
    assert!(key_of(&end.id.to_hex()).is_string() && key_of(&ring.id.to_hex()).is_string());
}

#[tokio::test]
async fn what_came_while_the_server_was_away_is_pushed_once_when_it_is_back() {
    let r = relay().await;
    let mut server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    let before = wrap(&alice, &[]);
    r.publish(&before).await;
    server.pushes_are(1).await;

    server.stop().await;
    let missed = wrap(&alice, &[]);
    r.publish(&missed).await;
    let missed_in_group = group_event(&Keys::generate(), &[]);
    r.publish(&missed_in_group).await;
    server.start();

    let pushes = server.pushes_are(3).await;
    let mut ids: Vec<_> = pushes.iter().map(|p| carried(&p["data"])["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    let mut expected = vec![before.id.to_hex(), missed.id.to_hex(), missed_in_group.id.to_hex()];
    expected.sort();
    assert_eq!(ids, expected, "what was pushed before is not pushed again");
}

#[tokio::test]
async fn a_device_that_leaves_takes_nothing_from_the_one_that_stays() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    let owner = alice.public_key().to_hex();
    server.register(&alice, "phone", &[&r]).await;
    server.register(&alice, "tablet", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    assert!(server.store.delete_device(&owner, "tablet").await.unwrap());
    // The relay has its part of the plan as it is without the tablet.
    server.plan_read().await;

    r.publish(&wrap(&alice, &[])).await;
    let pushes = server.pushes_are(1).await;
    assert_eq!(pushes[0]["token"], "token-of-phone");

    // The last device leaves: nothing is watched, and nothing is pushed.
    assert!(server.store.delete_device(&owner, "phone").await.unwrap());
    server.watch().plan_changed();
    eventually("the relay is let go", || async { server.watch().health().is_empty() }).await;
    r.publish(&wrap(&alice, &[])).await;
    server.pushes_are(1).await;
}

#[tokio::test]
async fn what_a_relay_took_while_nobody_watched_it_is_old_when_the_watch_begins_again() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    let owner = alice.public_key().to_hex();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    // Pushes are turned off on the only phone that named this relay.
    assert!(server.store.delete_device(&owner, "phone").await.unwrap());
    server.watch().plan_changed();
    eventually("the stock taken on the relay is forgotten", || async {
        server.store.baselined(&r.url).await.unwrap().is_empty()
    })
    .await;
    let meanwhile = wrap(&alice, &[]);
    r.publish(&meanwhile).await;

    // And on again.
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;
    let fresh = wrap(&alice, &[]);
    r.publish(&fresh).await;

    let pushes = server.pushes_are(1).await;
    assert_eq!(carried(&pushes[0]["data"])["id"], fresh.id.to_hex());
}

#[tokio::test]
async fn what_came_for_a_key_while_it_was_not_watched_is_old_when_it_is_watched_again() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let (alice, bob) = (Keys::generate(), Keys::generate());
    let owner = alice.public_key().to_hex();
    server.register(&alice, "alice-phone", &[&r]).await;
    server.register(&bob, "bob-phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;
    server.watching(&bob, &[&r]).await;

    // Alice leaves; the relay stays on the line for bob.
    assert!(server.store.delete_device(&owner, "alice-phone").await.unwrap());
    server.watch().plan_changed();
    eventually("the stock taken of alice is forgotten", || async {
        !server.knows(&r, &owner).await
    })
    .await;
    assert!(server.knows(&r, &bob.public_key().to_hex()).await, "bob is known as before");
    let meanwhile = wrap(&alice, &[]);
    r.publish(&meanwhile).await;
    // Something for bob after it: when the push about that is out, the
    // server has dealt with whatever the relay sent before, and alice was
    // nobody's to tell at the time.
    let to_bob = group_event(&Keys::generate(), &[]);
    r.publish(&to_bob).await;
    server.pushes_are(1).await;

    server.register(&alice, "alice-phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;
    let fresh = wrap(&alice, &[]);
    r.publish(&fresh).await;

    let pushes = server.pushes_are(2).await;
    assert_eq!(pushes[0]["token"], "token-of-bob-phone");
    assert_eq!(carried(&pushes[0]["data"])["id"], to_bob.id.to_hex());
    assert_eq!(pushes[1]["token"], "token-of-alice-phone");
    assert_eq!(carried(&pushes[1]["data"])["id"], fresh.id.to_hex());
}

#[tokio::test]
async fn an_event_dated_far_from_now_is_noted_and_not_pushed() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    // Straight to the pipeline, as from a relay that does not look at the
    // date it was asked from: an honest one keeps most of these back.
    let pipeline = server.pipeline();
    let now = Timestamp::now().as_secs();
    let dated = |at: u64| wrap_dated(&alice, "sealed", &[], at);

    let (ahead, stale) = (dated(now + 16 * 60), dated(now - 3 * 86_400 - 60));
    for event in [&ahead, &stale] {
        assert_eq!(pipeline.event(&r.url, event, false).await, Dealt::Misdated);
        let id = event.id.to_hex();
        assert!(!server.store.first_seen(&id, Seen::Pushed, now).await.unwrap(), "it was noted");
    }

    // A clock a few minutes ahead, and a sender who dated a sealed message
    // as far back as senders do.
    let (soon, backdated) = (dated(now + 10 * 60), dated(now - 2 * 86_400));
    for event in [&soon, &backdated] {
        let dealt = pipeline.event(&r.url, event, false).await;
        assert!(matches!(dealt, Dealt::Pushed { .. }), "{dealt:?}");
    }

    let pushes = server.pushes_are(2).await;
    let mut ids: Vec<_> = pushes
        .iter()
        .map(|p| carried(&p["data"])["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let mut expected = vec![soon.id.to_hex(), backdated.id.to_hex()];
    expected.sort();
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn a_relay_that_is_not_on_the_list_is_not_connected_to() {
    let (listed, other) = (relay().await, relay().await);
    let server = server(&[&listed], SHORT).await;
    let alice = Keys::generate();
    // The registration API would have left `other` out; the store is
    // written to directly here, as if it had not.
    server.register(&alice, "phone", &[&listed, &other]).await;
    server.watching(&alice, &[&listed]).await;

    let urls: Vec<_> = server.watch().health().into_iter().map(|h| h.url).collect();
    assert_eq!(urls, std::slice::from_ref(&listed.url));

    other.publish(&wrap(&alice, &[])).await;
    server.pushes_are(0).await;
}

#[tokio::test]
async fn who_registers_while_a_relay_is_away_is_watched_when_it_is_back() {
    let r = relay_behind_a_door().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    // The relay is away when somebody new registers: the request for their
    // key waits for it.
    r.goes_away();
    eventually("the server sees the relay away", || async {
        server.watch().health().iter().any(|h| h.url == r.url && h.state == "disconnected")
    })
    .await;
    let bob = Keys::generate();
    server.register(&bob, "bob-phone", &[&r]).await;
    // The door stays closed until the relay comes back, so the line takes
    // the new key while the relay is away.
    eventually("the line takes the new key", || async {
        server.watch().health().iter().any(|h| h.url == r.url && h.dm == 2)
    })
    .await;

    // Back on the line, it is asked, not within the hour but at once. The
    // wait is the library's own before it tries the relay again, which
    // grows with every try that failed: a minute is given to it. (A relay
    // that failed for so long that the library refuses requests for it is
    // not played here: that takes minutes. `Line::unsent` is for it.)
    let r = r.comes_back().await;
    server.relays.lock().unwrap()[0].1 = r.relay.clone();
    let bob_key = bob.public_key().to_hex();
    eventually_within(2 * WAIT, "the relay is asked about the new key", || async {
        server.knows(&r, &bob_key).await
    })
    .await;

    r.publish(&wrap(&bob, &[])).await;
    server.pushes_are(1).await;
}

/// Every change of what is watched makes the relay send two days of direct
/// messages again, and whoever registers a device can cause one.
#[tokio::test]
async fn a_burst_of_registrations_makes_a_relay_be_asked_twice_and_no_more() {
    let r = relay().await;
    // Longer than the three readings of the plan below take, so that all
    // of them come within one wait of the line.
    let tuning = Tuning {
        ask_every: Duration::from_secs(8),
        ..Tuning::default()
    };
    let server = server_with(&[&r], waking(SHORT), tuning).await;
    let first = Keys::generate();
    server.register(&first, "phone-0", &[&r]).await;
    server.watching(&first, &[&r]).await;
    assert_eq!(r.asked_for_direct_messages(), 1);

    // Registrations in three rounds, each read by the watcher before the
    // next: what is watched on the relay is another thing three times.
    let mut owners = Vec::new();
    for round in [1..=3, 4..=6, 7..=8] {
        for n in round {
            let owner = Keys::generate();
            server.register(&owner, &format!("phone-{n}"), &[&r]).await;
            owners.push(owner);
        }
        server.plan_read().await;
    }
    for owner in &owners {
        server.watching(owner, &[&r]).await;
    }
    assert_eq!(
        r.asked_for_direct_messages(),
        2,
        "once for the first, at once, and once for all who came after"
    );

    // Whoever came last is watched like the rest.
    let message = wrap(owners.last().unwrap(), &[]);
    r.publish(&message).await;
    let pushes = server.pushes_are(1).await;
    assert_eq!(pushes[0]["token"], "token-of-phone-8");
    assert_eq!(r.asked_for_direct_messages(), 2);
}

/// A phone of forty chats, and a message in every one of them at once,
/// straight to the pipeline: thirty are pushed, ten come over the cap.
async fn forty_chats_at_once(device_window: Duration) -> Server {
    let r = relay().await;
    let cap = Waking {
        chat_every: SHORT,
        device_cap: 30,
        device_window,
        ..waking(SHORT)
    };
    let server = server_with(&[&r], cap, at_once()).await;
    let alice = Keys::generate();
    let groups: Vec<String> = (1..=40).map(|n| format!("{n:064x}")).collect();
    let mut input = phone(&alice, "phone", &[&r]);
    input.groups = groups.iter().map(|group| member_of(group)).collect();
    server.put(input).await;

    let pipeline = server.pipeline();
    let mut pushed = Vec::new();
    for group in &groups {
        let event = event_of(group, &Keys::generate(), &[]);
        pushed.push(pipeline.event(&r.url, &event, false).await);
    }
    let at_once = Dealt::Pushed { now: 1, later: 0, own: 0, over: 0 };
    let counted = Dealt::Pushed { now: 0, later: 0, own: 0, over: 1 };
    assert!(pushed[..30].iter().all(|dealt| *dealt == at_once), "{pushed:?}");
    assert!(pushed[30..].iter().all(|dealt| *dealt == counted), "{pushed:?}");
    server
}

#[tokio::test]
async fn a_device_is_pushed_to_thirty_times_and_told_of_the_rest_in_one_push() {
    // Within the window of the device, an hour here, the thirty are pushed
    // and the ten wait.
    let server = forty_chats_at_once(HOUR).await;
    let pushes = server.pushes_are(30).await;
    assert!(pushes.iter().all(|p| p["data"]["type"] == "group"), "{pushes:#?}");
    assert_eq!(server.counters.sync_pushes.get(), 0, "the rest is told when the time is up");

    // The ten that came over the cap: one push when the window of two
    // seconds is over, which says how many and nothing of any one of them.
    let server = forty_chats_at_once(Duration::from_secs(2)).await;
    let pushes = server.pushes_are(31).await;
    assert!(pushes[..30].iter().all(|p| p["data"]["type"] == "group"), "{pushes:#?}");
    let sync = &pushes[30];
    assert_eq!(sync["token"], "token-of-phone");
    let data = sync["data"].as_object().unwrap();
    assert_eq!(data["type"], "sync");
    assert_eq!(data["count"], "10");
    assert_eq!(data["v"], "2");
    let mut keys: Vec<_> = data.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["count", "trace", "type", "v"], "no event, no id of one, no group");
    assert_eq!(sync["android"]["priority"], "HIGH");
    assert_eq!(server.counters.sync_pushes.get(), 1);

    // Every one of the forty was told: thirty by itself, ten by the count.
    assert_eq!(server.told().await, 40);
}

/// Sixty messages for a line that has room for eight.
#[tokio::test]
async fn a_line_with_more_to_deal_with_than_it_has_room_for_starts_over_and_loses_nothing() {
    let r = relay().await;
    let tuning = Tuning {
        queue: 8,
        ask_every: Duration::from_millis(100),
        ..Tuning::default()
    };
    let mut server = server_with(&[&r], waking(SHORT), tuning).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    // While the server is away. When it is back the relay sends them all
    // at once.
    server.stop().await;
    for _ in 0..60 {
        r.publish(&wrap(&alice, &[])).await;
    }
    server.start();

    eventually("every message is told", || async { server.told().await >= 60 }).await;
    server.settled().await;
    assert_eq!(server.told().await, 60, "every one of them, and none of them twice");
    assert!(
        server.counters.lines_started_over.get() >= 1,
        "sixty do not fit where there is room for eight"
    );

    // The line that started over watches as before.
    let after = wrap(&alice, &[]);
    r.publish(&after).await;
    eventually("what comes after is told", || async { server.told().await >= 61 }).await;
    assert_eq!(server.watch().status(&r.url), vpush_proto::RelayStatus::Ok);
}

/// nostr-sdk hands on what a relay says through a channel that drops the
/// oldest when it is full, and says nothing. Here it has eight places, and
/// the relay sends sixty messages at once.
#[tokio::test]
async fn a_burst_bigger_than_the_channel_of_nostr_sdk_loses_nothing() {
    let r = relay().await;
    let tuning = Tuning {
        notifications: 8,
        ..at_once()
    };
    let mut server = server_with(&[&r], waking(SHORT), tuning).await;
    let alice = Keys::generate();
    server.register(&alice, "phone", &[&r]).await;
    server.watching(&alice, &[&r]).await;

    server.stop().await;
    for _ in 0..60 {
        r.publish(&wrap(&alice, &[])).await;
    }
    server.start();

    eventually("every message is told", || async { server.told().await >= 60 }).await;
    server.settled().await;
    assert_eq!(server.told().await, 60);
    assert_eq!(
        server.counters.lines_started_over.get(),
        0,
        "the queue had room for all of them: nothing was lost, and nothing was asked for again"
    );
}

/// What APNs was handed: the push type, the priority and the body.
async fn apple_got(server: &Server, count: usize) -> Vec<(String, String, Value)> {
    eventually(&format!("{count} pushes to APNs"), || async {
        server.apple.received_requests().await.unwrap().len() >= count
    })
    .await;
    server.settled().await;
    let got: Vec<_> = server
        .apple
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let header = |name: &str| r.headers[name].to_str().unwrap().to_string();
            (
                header("apns-push-type"),
                header("apns-priority"),
                serde_json::from_slice::<Value>(&r.body).unwrap(),
            )
        })
        .collect();
    assert_eq!(got.len(), count, "{got:#?}");
    got
}

/// A note to the keeper of a channel (`["channel", "1"]` on the wrap) wakes
/// an iPhone in the background, with nothing to show; a message is an alert
/// the extension of the app writes. Both carry the event, as through FCM.
#[tokio::test]
async fn a_note_to_a_keeper_wakes_an_iphone_in_the_background() {
    let r = relay().await;
    let server = server(&[&r], SHORT).await;
    let alice = Keys::generate();
    let mut iphone = phone(&alice, "iphone", &[&r]);
    iphone.app_id = IOS.to_string();
    iphone.provider = "apns".to_string();
    iphone.token = "ab".repeat(32);
    iphone.channel_json = Some(format!(
        r#"{{"provider":"apns","token":"{}","environment":"sandbox"}}"#,
        "ab".repeat(32)
    ));
    server.put(iphone).await;
    server.watching(&alice, &[&r]).await;

    let note = wrap(&alice, &[&["channel", "1"]]);
    r.publish(&note).await;
    let got = apple_got(&server, 1).await;
    let (kind, priority, body) = &got[0];
    assert_eq!((kind.as_str(), priority.as_str()), ("background", "5"));
    assert_eq!(body["aps"], json!({ "content-available": 1 }));
    assert_eq!(carried(body)["id"], note.id.to_hex());

    // The window of the chat is short, and over: `apple_got` waited it out.
    // The message after it goes at once.
    let message = wrap(&alice, &[]);
    r.publish(&message).await;
    let got = apple_got(&server, 2).await;
    let (kind, priority, body) = &got[1];
    assert_eq!((kind.as_str(), priority.as_str()), ("alert", "10"));
    assert_eq!(body["aps"]["mutable-content"], 1);
    assert_eq!(body["aps"]["alert"]["loc-key"], "push.new");
    assert!(body["aps"]["thread-id"].is_string(), "the chat, which groups its notifications");
    assert_eq!(carried(body)["id"], message.id.to_hex());
    assert!(server.pushes().await.is_empty(), "nothing went to FCM");
}

/// An iPhone of `owner` with its device token, and its PushKit token when
/// it gave one.
fn iphone(owner: &Keys, device: &str, relays: &[&Relay], token: &str, voip: Option<&str>) -> DeviceInput {
    let mut iphone = phone(owner, device, relays);
    iphone.app_id = IOS.to_string();
    iphone.provider = "apns".to_string();
    iphone.token = token.to_string();
    let voip = voip.map(|v| format!(r#","voip_token":"{v}""#)).unwrap_or_default();
    iphone.channel_json = Some(format!(
        r#"{{"provider":"apns","token":"{token}","environment":"sandbox"{voip}}}"#
    ));
    iphone
}

/// A request APNs was handed, as far as these tests look at it.
#[derive(Debug)]
struct ToApple {
    token: String,
    topic: String,
    kind: String,
    priority: String,
    expiration: u64,
    body: Value,
}

async fn to_apple(server: &Server, count: usize) -> Vec<ToApple> {
    apple_got(server, count).await;
    server
        .apple
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let header = |name: &str| r.headers[name].to_str().unwrap().to_string();
            ToApple {
                token: r.url.path().trim_start_matches("/3/device/").to_string(),
                topic: header("apns-topic"),
                kind: header("apns-push-type"),
                priority: header("apns-priority"),
                expiration: header("apns-expiration").parse().unwrap(),
                body: serde_json::from_slice(&r.body).unwrap(),
            }
        })
        .collect()
}

/// The invitation to a call goes to an iPhone that gave a PushKit token as
/// a VoIP push: to that token, the topic `.voip`, at once, alive as long as
/// the push of a call, the keys of the push and no `aps`. An iPhone without
/// one is rung by an alert, as before. The end of a ringing is never VoIP:
/// iOS wants every VoIP push shown as an incoming call.
#[tokio::test]
async fn a_ring_goes_as_voip_to_an_iphone_with_a_voip_token_and_nothing_else_does() {
    let r = relay().await;
    let server = server(&[&r], HOUR).await;
    let alice = Keys::generate();
    let (with, voip, without) = ("ab".repeat(32), "cd".repeat(32), "ef".repeat(32));
    server.put(iphone(&alice, "iphone-1", &[&r], &with, Some(&voip))).await;
    server.put(iphone(&alice, "iphone-2", &[&r], &without, None)).await;
    server.watching(&alice, &[&r]).await;

    let ring = wrap(&alice, &[&["call", "1"]]);
    r.publish(&ring).await;
    let got = to_apple(&server, 2).await;
    let pushkit = got.iter().find(|p| p.token == voip).expect("a VoIP push to the PushKit token");
    assert!(!got.iter().any(|p| p.token == with), "and nothing to the device token beside it");
    assert_eq!(pushkit.topic, format!("{IOS}.voip"));
    assert_eq!((pushkit.kind.as_str(), pushkit.priority.as_str()), ("voip", "10"));
    let now = Timestamp::now().as_secs();
    assert!(pushkit.expiration.abs_diff(now + 60) <= 5, "the minute of a call: {}", pushkit.expiration);
    assert!(pushkit.body.get("aps").is_none(), "{:#}", pushkit.body);
    assert_eq!(pushkit.body["type"], "dm");
    assert_eq!(pushkit.body["call"], "1");
    assert_eq!(carried(&pushkit.body)["id"], ring.id.to_hex());

    let alert = got.iter().find(|p| p.token == without).expect("an alert to the iPhone without one");
    assert_eq!(alert.topic, IOS);
    assert_eq!((alert.kind.as_str(), alert.priority.as_str()), ("alert", "10"));
    assert_eq!(alert.body["aps"]["alert"]["loc-key"], "push.new");
    assert_eq!(alert.body["call"], "1");
    assert_eq!(carried(&alert.body)["id"], ring.id.to_hex());
    let mut voip_body = pushkit.body.clone();
    voip_body.as_object_mut().unwrap().insert("aps".into(), alert.body["aps"].clone());
    assert_eq!(voip_body, alert.body, "the same keys, `aps` aside");

    // The end of the ringing: an alert to the device token of each.
    let end = wrap(&alice, &[&["call", "0"]]);
    r.publish(&end).await;
    let got = to_apple(&server, 4).await;
    let ends: Vec<&ToApple> = got[2..].iter().collect();
    let mut tokens: Vec<&str> = ends.iter().map(|p| p.token.as_str()).collect();
    tokens.sort();
    assert_eq!(tokens, [with.as_str(), without.as_str()]);
    for push in ends {
        assert_eq!((push.topic.as_str(), push.kind.as_str()), (IOS, "alert"));
        assert_eq!(carried(&push.body)["id"], end.id.to_hex());
    }
    let device = server.store.device(&alice.public_key().to_hex(), "iphone-1").await.unwrap().unwrap();
    assert_eq!(device.voip_token, Some(voip));
    assert_eq!(device.last_outcome.as_deref(), Some("delivered"));
}

/// A PushKit token APNs says is dead is forgotten, and the device is not:
/// the ring goes on to its device token as an alert, and so does the next
/// one, until the phone registers a VoIP token again.
#[tokio::test]
async fn a_dead_voip_token_is_forgotten_and_the_ring_goes_as_an_alert() {
    let r = relay().await;
    let server = server(&[&r], HOUR).await;
    let (token, voip) = ("ab".repeat(32), "cd".repeat(32));
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{voip}")))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({ "reason": "Unregistered" })))
        .with_priority(1)
        .mount(&server.apple)
        .await;
    let alice = Keys::generate();
    server.put(iphone(&alice, "iphone", &[&r], &token, Some(&voip))).await;
    server.watching(&alice, &[&r]).await;

    let ring = wrap(&alice, &[&["call", "1"]]);
    r.publish(&ring).await;
    let got = to_apple(&server, 2).await;
    assert_eq!((got[0].token.as_str(), got[0].kind.as_str()), (voip.as_str(), "voip"));
    assert_eq!((got[1].token.as_str(), got[1].kind.as_str()), (token.as_str(), "alert"));
    assert_eq!(got[1].topic, IOS);
    assert_eq!(got[1].body["call"], "1");
    assert_eq!(carried(&got[1].body)["id"], ring.id.to_hex());

    let device = server.store.device(&alice.public_key().to_hex(), "iphone").await.unwrap().unwrap();
    assert_eq!(device.voip_token, None, "the VoIP token is forgotten");
    assert_eq!((device.state.as_str(), device.token.as_str()), ("active", token.as_str()), "the device is not");
    assert_eq!(device.last_outcome.as_deref(), Some("delivered"));

    let again = wrap(&alice, &[&["call", "1"]]);
    r.publish(&again).await;
    let got = to_apple(&server, 3).await;
    assert_eq!((got[2].token.as_str(), got[2].kind.as_str()), (token.as_str(), "alert"));
    assert_eq!(carried(&got[2].body)["id"], again.id.to_hex());
}
