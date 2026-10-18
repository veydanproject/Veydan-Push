//! The gateway (`spec/protocol.md`, "The gateway"), both ways: a server of
//! the network that issues tickets and wakes them through a push service
//! played by a mock, and a private push server that seals its pushes and
//! hands them to a gateway, played by a mock or by the first one.

#[path = "support/key.rs"]
mod key;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use nostr::prelude::*;
use serde_json::{json, Value};
use vpush_proto::{Payload, PushType};
use vpush_server::api::{self, Api};
use vpush_server::auth::sha256_hex;
use vpush_server::config::Config;
use vpush_server::counters::Counters;
use vpush_server::delivery::fcm::{FcmClient, ServiceAccount};
use vpush_server::delivery::gateway::{open, GatewayClient, SealKey};
use vpush_server::delivery::retry::{self, RetryPolicy};
use vpush_server::delivery::{Message, Outcome, Providers, PushProvider, Target};
use vpush_server::relays::RelayPolicy;
use vpush_server::store::{SqliteStore, Store};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP: &str = "net.veydan.mobile";
const SEND: &str = "/v1/projects/veydan-test/messages:send";
const WAKE: &str = "/v1/gateway/wake";
const DEVICE: &str = "/v1/devices/phone-0001";

struct Server {
    base: String,
    public: String,
    http: reqwest::Client,
    store: Arc<SqliteStore>,
    google: MockServer,
}

/// A server with FCM played by a mock, and `more` in its config.
async fn gateway(more: &str) -> Server {
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1", "expires_in": 3600
        })))
        .mount(&google)
        .await;
    // Every token is one FCM would push to.
    Mock::given(method("POST"))
        .and(path(SEND))
        .and(body_string_contains(r#""validate_only":true"#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "name": "m/0" })))
        .mount(&google)
        .await;

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
    let mut providers = Providers::default();
    providers.insert(
        APP,
        Arc::new(FcmClient::new(account, &google.uri()).unwrap()),
    );
    serve(
        &format!("[apps.\"{APP}\".fcm]\nservice_account = \"/nowhere/fcm.json\"\n{more}"),
        providers,
        google,
    )
    .await
}

/// A private server: its devices are woken through the gateway at `url`,
/// with the key `seal`.
async fn private(url: &str, seal: Keys) -> Server {
    let mut providers = Providers::default();
    let client = GatewayClient::new(url, Arc::new(SealKey::from_keys(seal))).unwrap();
    providers.insert_gateway(APP, Arc::new(client));
    let google = MockServer::start().await;
    serve(
        &format!("[apps.\"{APP}\"]\n[gateway]\nurl = \"{url}\"\n"),
        providers,
        google,
    )
    .await
}

async fn serve(config: &str, providers: Providers, google: MockServer) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let public = format!("http://localhost:{port}");
    let config = Config::parse(&format!(
        "public_url = \"{public}\"\n[[relays.allow]]\nurl = \"wss://node-1.veydan.net\"\n{config}"
    ))
    .unwrap();
    let store = Arc::new(SqliteStore::in_memory().await.unwrap());
    let relays = RelayPolicy::from_config(&config).unwrap();
    let api = Api::new(
        Arc::new(config),
        store.clone() as Arc<dyn Store>,
        Arc::new(providers),
        Arc::new(relays),
        vpush_server::relay::Watch::new(),
        Arc::new(Counters::default()),
    );
    tokio::spawn(async move {
        axum::serve(listener, api::router(api)).await.unwrap();
    });
    Server {
        base: format!("http://127.0.0.1:{port}"),
        public,
        http: reqwest::Client::new(),
        store,
        google,
    }
}

fn now() -> u64 {
    Timestamp::now().as_secs()
}

fn signed(keys: &Keys, method: &str, url: &str, body: &[u8]) -> String {
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tags = vec![
        Tag::parse(["u", url]).unwrap(),
        Tag::parse(["method", method]).unwrap(),
        Tag::parse(["nonce", nonce.to_string().as_str()]).unwrap(),
    ];
    if !body.is_empty() {
        tags.push(Tag::parse(["payload", sha256_hex(body).as_str()]).unwrap());
    }
    let event = EventBuilder::new(Kind::HttpAuth, "")
        .tags(tags)
        .finalize(keys)
        .unwrap();
    format!(
        "Nostr {}",
        STANDARD.encode(serde_json::to_vec(&event).unwrap())
    )
}

struct Answer {
    status: u16,
    body: Value,
    retry_after: Option<String>,
}

impl Server {
    async fn send(
        &self,
        method: &str,
        path: &str,
        header: Option<String>,
        body: Vec<u8>,
    ) -> Answer {
        self.send_with(method, path, header, body, &[]).await
    }

    /// `more`: headers a proxy in front adds.
    async fn send_with(
        &self,
        method: &str,
        path: &str,
        header: Option<String>,
        body: Vec<u8>,
        more: &[(&str, &str)],
    ) -> Answer {
        let mut request = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.base));
        if let Some(h) = header {
            request = request.header("authorization", h);
        }
        for (name, value) in more {
            request = request.header(*name, *value);
        }
        if !body.is_empty() {
            request = request
                .header("content-type", "application/json")
                .body(body);
        }
        let response = request.send().await.unwrap();
        let retry_after = response
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_string());
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        Answer {
            status,
            body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
            retry_after,
        }
    }

    async fn ask(&self, keys: &Keys, method: &str, path: &str, body: Option<Value>) -> Answer {
        let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let header = signed(keys, method, &format!("{}{path}", self.public), &body);
        self.send(method, path, Some(header), body).await
    }

    async fn wake(&self, body: Value) -> Answer {
        self.send("POST", WAKE, None, body.to_string().into_bytes())
            .await
    }

    /// A wake that came past the proxy in front from `from`.
    async fn wake_from(&self, body: Value, from: &str) -> Answer {
        self.send_with("POST", WAKE, None, body.to_string().into_bytes(), &[("x-real-ip", from)])
            .await
    }

    async fn fcm_answers(&self, answer: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path(SEND))
            .respond_with(answer)
            .mount(&self.google)
            .await;
    }

    /// The pushes FCM was handed, as JSON: not the questions about tokens.
    async fn pushes(&self) -> Vec<Value> {
        self.google
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == SEND)
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
            .filter(|body| body.get("validate_only").is_none())
            .collect()
    }

    /// A device of `owner` registered here with its token, and a ticket for it.
    async fn ticket_for(&self, owner: &Keys) -> String {
        let put = self
            .ask(
                owner,
                "PUT",
                DEVICE,
                Some(registration("token-of-the-phone")),
            )
            .await;
        assert_eq!(put.status, 200, "{}", put.body);
        let issued = self
            .ask(
                owner,
                "POST",
                "/v1/tickets",
                Some(json!({ "device_id": "phone-0001" })),
            )
            .await;
        assert_eq!(issued.status, 201, "{}", issued.body);
        issued.body["ticket"].as_str().unwrap().to_string()
    }
}

fn registration(token: &str) -> Value {
    json!({
        "app_id": APP,
        "channel": { "provider": "fcm", "token": token },
        "relays": [{ "url": "wss://node-1.veydan.net" }],
    })
}

fn code(answer: &Answer) -> &str {
    answer.body["error"]["code"].as_str().unwrap_or("")
}

/// A blob as a private server sends it, `len` characters of base64url
/// (never one more than a multiple of four: no bytes make that).
fn blob(len: usize) -> String {
    let bytes: Vec<u8> = (0..len * 3 / 4).map(|i| (i * 37 % 256) as u8).collect();
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    assert_eq!(blob.len(), len);
    blob
}

#[tokio::test]
async fn a_ticket_is_issued_listed_and_deleted_by_the_owner_alone() {
    let server = gateway("[gateway]\naccept = true\n").await;
    let (alice, bob) = (Keys::generate(), Keys::generate());

    // No device yet: no ticket.
    let refused = server
        .ask(
            &alice,
            "POST",
            "/v1/tickets",
            Some(json!({ "device_id": "phone-0001" })),
        )
        .await;
    assert_eq!(
        (refused.status, code(&refused)),
        (409, "no_device"),
        "{}",
        refused.body
    );

    let ticket = server.ticket_for(&alice).await;
    assert_eq!(ticket.len(), 64);
    assert!(ticket
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    // Another ticket for the same device is another ticket.
    assert_ne!(server.ticket_for(&alice).await, ticket);
    // Bob registers no device of alice: her device is none of his.
    let refused = server
        .ask(
            &bob,
            "POST",
            "/v1/tickets",
            Some(json!({ "device_id": "phone-0001" })),
        )
        .await;
    assert_eq!((refused.status, code(&refused)), (409, "no_device"));

    let list = server.ask(&alice, "GET", "/v1/tickets", None).await;
    assert_eq!(list.status, 200, "{}", list.body);
    let tickets = list.body["tickets"].as_array().unwrap();
    assert_eq!(tickets.len(), 2);
    let shown: Vec<_> = tickets
        .iter()
        .map(|t| t["ticket"].as_str().unwrap().to_string())
        .collect();
    assert!(shown.contains(&format!("{}…", &ticket[..8])), "{shown:?}");
    for t in tickets {
        assert_eq!(t["device_id"], "phone-0001");
        assert_eq!(t["last_wake_at"], Value::Null);
        let days = (t["expires_at"].as_u64().unwrap() - t["issued_at"].as_u64().unwrap()) / 86_400;
        assert_eq!(days, 30);
    }
    assert!(
        !list.body.to_string().contains(&ticket),
        "the ticket is not said again"
    );
    assert_eq!(
        server.ask(&bob, "GET", "/v1/tickets", None).await.body,
        json!({ "tickets": [] })
    );

    // A ticket of another key does not exist to bob, whole or by its prefix.
    for name in [ticket.clone(), ticket[..8].to_string()] {
        let gone = server
            .ask(&bob, "DELETE", &format!("/v1/tickets/{name}"), None)
            .await;
        assert_eq!((gone.status, code(&gone)), (404, "ticket_unknown"));
    }
    let bad = server.ask(&alice, "DELETE", "/v1/tickets/xyz", None).await;
    assert_eq!((bad.status, code(&bad)), (400, "bad_request"));

    let gone = server
        .ask(
            &alice,
            "DELETE",
            &format!("/v1/tickets/{}", &ticket[..8]),
            None,
        )
        .await;
    assert_eq!(gone.status, 204, "{}", gone.body);
    let wake = server
        .wake(json!({ "ticket": ticket, "blob": blob(100), "sender": "ab".repeat(32) }))
        .await;
    assert_eq!(
        (wake.status, code(&wake)),
        (404, "ticket_unknown"),
        "a deleted ticket wakes nothing"
    );
    assert_eq!(
        server.ask(&alice, "GET", "/v1/tickets", None).await.body["tickets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // The device removed, its tickets go with it.
    assert_eq!(server.ask(&alice, "DELETE", DEVICE, None).await.status, 204);
    assert_eq!(
        server.ask(&alice, "GET", "/v1/tickets", None).await.body,
        json!({ "tickets": [] })
    );
    assert_eq!(server.store.counts().await.unwrap().tickets, 0);
}

#[tokio::test]
async fn a_device_holds_so_many_tickets_and_one_that_ran_out_wakes_nothing() {
    let server = gateway("[gateway]\naccept = true\n[tickets]\ndays = 3\nper_device = 2\n").await;
    let alice = Keys::generate();
    let first = server.ticket_for(&alice).await;
    let second = server.ticket_for(&alice).await;
    let third = server.ticket_for(&alice).await;
    let list = server.ask(&alice, "GET", "/v1/tickets", None).await;
    let shown: Vec<_> = list.body["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["ticket"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert!(
        !shown.contains(&format!("{}…", &first[..8])),
        "the oldest went: {shown:?}"
    );
    let days = list.body["tickets"][0]["expires_at"].as_u64().unwrap() - now();
    assert!((2 * 86_400..=3 * 86_400).contains(&days), "{days}");

    server
        .fcm_answers(ResponseTemplate::new(200).set_body_json(json!({ "name": "m/1" })))
        .await;
    let wake =
        |ticket: &str| json!({ "ticket": ticket, "blob": blob(64), "sender": "ab".repeat(32) });
    assert_eq!(server.wake(wake(&first)).await.status, 404);
    assert_eq!(server.wake(wake(&second)).await.status, 202);
    // Three days later the sweep has forgotten what ran out.
    assert_eq!(
        server
            .store
            .purge_tickets(now() + 3 * 86_400 + 1)
            .await
            .unwrap(),
        2
    );
    let late = server.wake(wake(&third)).await;
    assert_eq!((late.status, code(&late)), (404, "ticket_unknown"));
}

#[tokio::test]
async fn a_wake_reaches_fcm_as_a_sealed_push_and_the_blob_is_passed_untouched() {
    let server = gateway("[gateway]\naccept = true\n").await;
    server
        .fcm_answers(ResponseTemplate::new(200).set_body_json(json!({ "name": "m/1" })))
        .await;
    let alice = Keys::generate();
    let ticket = server.ticket_for(&alice).await;
    let (sealed, sender) = (blob(3600), "ab".repeat(32));

    let wake = server
        .wake(json!({ "ticket": ticket, "blob": sealed, "sender": sender }))
        .await;
    assert_eq!(wake.status, 202, "{}", wake.body);
    let trace = wake.body["trace"].as_str().unwrap().to_string();
    assert_eq!(trace.len(), 8);

    let pushes = server.pushes().await;
    assert_eq!(pushes.len(), 1);
    let message = &pushes[0]["message"];
    assert_eq!(message["token"], "token-of-the-phone");
    assert_eq!(
        message["data"],
        json!({ "v": "2", "type": "sealed", "blob": sealed, "sender": sender, "trace": trace })
    );
    assert!(message.get("notification").is_none());
    assert_eq!(
        message["android"]["priority"], "NORMAL",
        "a wake that does not say it is urgent is not"
    );
    assert!(
        message["android"].get("collapse_key").is_none(),
        "nothing says which pushes are of one chat"
    );

    // The wake of a call says so, and goes at high priority.
    let urgent = server
        .wake(json!({ "ticket": ticket, "blob": sealed, "sender": sender, "urgent": true }))
        .await;
    assert_eq!(urgent.status, 202, "{}", urgent.body);
    let pushes = server.pushes().await;
    assert_eq!(pushes.len(), 2);
    assert_eq!(pushes[1]["message"]["android"]["priority"], "HIGH");
    assert_eq!(
        pushes[1]["message"]["data"]["blob"], sealed,
        "nothing else of the push changes"
    );
    let not = server
        .wake(json!({ "ticket": ticket, "blob": sealed, "sender": sender, "urgent": false }))
        .await;
    assert_eq!(not.status, 202, "{}", not.body);
    assert_eq!(server.pushes().await[2]["message"]["android"]["priority"], "NORMAL");

    let list = server.ask(&alice, "GET", "/v1/tickets", None).await;
    let woken = list.body["tickets"][0]["last_wake_at"]
        .as_u64()
        .expect("the wake is noted");
    assert!(woken + 5 >= now());

    // A wake is signed by nobody, and asks for nothing the protocol does not name.
    let unknown = server
        .wake(json!({ "ticket": "ee".repeat(32), "blob": blob(10), "sender": sender }))
        .await;
    assert_eq!((unknown.status, code(&unknown)), (404, "ticket_unknown"));
    let big = server
        .wake(json!({ "ticket": ticket, "blob": blob(3602), "sender": sender }))
        .await;
    assert_eq!((big.status, code(&big)), (413, "payload_too_large"));
    for wrong in [
        json!({ "ticket": "nope", "blob": blob(10), "sender": sender }),
        json!({ "ticket": ticket, "blob": "not base64url!", "sender": sender }),
        json!({ "ticket": ticket, "blob": "", "sender": sender }),
        json!({ "ticket": ticket, "blob": blob(10), "sender": "AB".repeat(32) }),
        json!({ "ticket": ticket, "blob": blob(10) }),
        json!({ "ticket": ticket, "blob": blob(10), "sender": sender, "urgent": "yes" }),
    ] {
        let refused = server.wake(wrong.clone()).await;
        assert_eq!(
            (refused.status, code(&refused)),
            (400, "bad_request"),
            "{wrong}"
        );
    }
    assert_eq!(server.pushes().await.len(), 3, "nothing more reached FCM");
}

#[tokio::test]
async fn a_ticket_is_woken_thirty_times_a_minute() {
    let server = gateway("[gateway]\naccept = true\n").await;
    server
        .fcm_answers(ResponseTemplate::new(200).set_body_json(json!({ "name": "m/1" })))
        .await;
    let alice = Keys::generate();
    let ticket = server.ticket_for(&alice).await;
    let other = {
        let bob = Keys::generate();
        let put = server
            .ask(
                &bob,
                "PUT",
                DEVICE,
                Some(registration("token-of-the-tablet")),
            )
            .await;
        assert_eq!(put.status, 200);
        let issued = server
            .ask(
                &bob,
                "POST",
                "/v1/tickets",
                Some(json!({ "device_id": "phone-0001" })),
            )
            .await;
        issued.body["ticket"].as_str().unwrap().to_string()
    };
    let wake =
        |ticket: &str| json!({ "ticket": ticket, "blob": blob(64), "sender": "ab".repeat(32) });
    for i in 0..api::WAKES_PER_MINUTE {
        assert_eq!(server.wake(wake(&ticket)).await.status, 202, "wake {i}");
    }
    let refused = server.wake(wake(&ticket)).await;
    assert_eq!((refused.status, code(&refused)), (429, "rate_limited"));
    let after: u64 = refused.retry_after.expect("Retry-After").parse().unwrap();
    assert!((50..=60).contains(&after), "{after}");
    // Another ticket has its own thirty.
    assert_eq!(server.wake(wake(&other)).await.status, 202);
    assert_eq!(server.pushes().await.len(), api::WAKES_PER_MINUTE + 1);
}

/// A device holds several tickets, each perhaps handed to a server that
/// means it harm: all of them together wake it as often as one may.
#[tokio::test]
async fn a_device_is_woken_thirty_times_a_minute_by_all_its_tickets() {
    let server = gateway("[gateway]\naccept = true\n").await;
    server
        .fcm_answers(ResponseTemplate::new(200).set_body_json(json!({ "name": "m/1" })))
        .await;
    let (alice, bob) = (Keys::generate(), Keys::generate());
    let tickets = [
        server.ticket_for(&alice).await,
        server.ticket_for(&alice).await,
        server.ticket_for(&alice).await,
    ];
    let wake =
        |ticket: &str| json!({ "ticket": ticket, "blob": blob(64), "sender": "ab".repeat(32) });
    for i in 0..api::DEVICE_WAKES_PER_MINUTE {
        let answer = server.wake(wake(&tickets[i % 2])).await;
        assert_eq!(answer.status, 202, "wake {i}: {}", answer.body);
    }
    // The third ticket was never woken, and still finds no room.
    let refused = server.wake(wake(&tickets[2])).await;
    assert_eq!(
        (refused.status, code(&refused)),
        (429, "rate_limited"),
        "{}",
        refused.body
    );
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("one device"),
        "{}",
        refused.body
    );
    let after: u64 = refused.retry_after.expect("Retry-After").parse().unwrap();
    assert!((50..=60).contains(&after), "{after}");
    assert_eq!(server.pushes().await.len(), api::DEVICE_WAKES_PER_MINUTE);
    // The refused wake was not noted as one.
    let list = server.ask(&alice, "GET", "/v1/tickets", None).await;
    let never = list.body["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["last_wake_at"].is_null())
        .count();
    assert_eq!(never, 1, "{}", list.body);

    // A call from another server still rings: urgent wakes have a budget
    // of their own, which the flood did not touch.
    let mut call = wake(&tickets[2]);
    call["urgent"] = json!(true);
    let rung = server.wake(call).await;
    assert_eq!(rung.status, 202, "{}", rung.body);
    let pushes = server.pushes().await;
    assert_eq!(pushes.len(), api::DEVICE_WAKES_PER_MINUTE + 1);
    assert_eq!(pushes.last().unwrap()["message"]["android"]["priority"], "HIGH");

    // Another phone has its own.
    let put = server
        .ask(&bob, "PUT", DEVICE, Some(registration("token-of-the-tablet")))
        .await;
    assert_eq!(put.status, 200);
    let issued = server
        .ask(&bob, "POST", "/v1/tickets", Some(json!({ "device_id": "phone-0001" })))
        .await;
    let other = issued.body["ticket"].as_str().unwrap().to_string();
    assert_eq!(server.wake(wake(&other)).await.status, 202);
}

/// Behind a proxy, one address asks for so many wakes a minute, answered or
/// not: whoever tries tickets at random is stopped before the database.
#[tokio::test]
async fn an_address_asks_for_six_hundred_wakes_a_minute() {
    let server = gateway("[server]\ntrusted_proxy = true\n[gateway]\naccept = true\n").await;
    let random =
        |i: usize| json!({ "ticket": format!("{i:064x}"), "blob": blob(64), "sender": "ab".repeat(32) });
    for i in 0..api::WAKE_REQUESTS_PER_MINUTE {
        let answer = server.wake_from(random(i), "203.0.113.7").await;
        assert_eq!((answer.status, code(&answer)), (404, "ticket_unknown"), "wake {i}");
    }
    let refused = server.wake_from(random(0), "203.0.113.7").await;
    assert_eq!(
        (refused.status, code(&refused)),
        (429, "rate_limited"),
        "{}",
        refused.body
    );
    let after: u64 = refused.retry_after.expect("Retry-After").parse().unwrap();
    assert!((1..=60).contains(&after), "{after}");
    // Not even a body that is no wake is read.
    let junk = server
        .send_with("POST", WAKE, None, b"{}".to_vec(), &[("x-real-ip", "203.0.113.7")])
        .await;
    assert_eq!(junk.status, 429);
    // Another address has its own room.
    let other = server.wake_from(random(1), "203.0.113.8").await;
    assert_eq!((other.status, code(&other)), (404, "ticket_unknown"));
}

#[tokio::test]
async fn a_dead_token_takes_its_tickets_and_is_told_as_a_ticket_unknown() {
    let server = gateway("[gateway]\naccept = true\n").await;
    server
        .fcm_answers(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": 404, "message": "gone", "status": "NOT_FOUND",
                       "details": [{ "@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                                     "errorCode": "UNREGISTERED" }] }
        })))
        .await;
    let alice = Keys::generate();
    let ticket = server.ticket_for(&alice).await;
    server.ticket_for(&alice).await;
    let wake = server
        .wake(json!({ "ticket": ticket, "blob": blob(64), "sender": "ab".repeat(32) }))
        .await;
    assert_eq!(
        (wake.status, code(&wake)),
        (404, "ticket_unknown"),
        "{}",
        wake.body
    );
    assert_eq!(
        server.ask(&alice, "GET", "/v1/tickets", None).await.body,
        json!({ "tickets": [] })
    );
    assert_eq!(server.store.counts().await.unwrap().tickets, 0);
    let device = server.ask(&alice, "GET", DEVICE, None).await;
    assert_eq!(device.body["state"], "dead_token");
}

#[tokio::test]
async fn a_busy_push_service_is_said_so_and_a_refusing_one_too() {
    let server = gateway("[gateway]\naccept = true\n").await;
    let alice = Keys::generate();
    let ticket = server.ticket_for(&alice).await;
    let wake = json!({ "ticket": ticket, "blob": blob(64), "sender": "ab".repeat(32) });
    Mock::given(method("POST"))
        .and(path(SEND))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "7"))
        .up_to_n_times(1)
        .mount(&server.google)
        .await;
    server
        .fcm_answers(ResponseTemplate::new(403).set_body_json(json!({
            "error": { "code": 403, "message": "no", "status": "PERMISSION_DENIED" }
        })))
        .await;
    let busy = server.wake(wake.clone()).await;
    assert_eq!(
        (busy.status, code(&busy)),
        (503, "provider_unavailable"),
        "{}",
        busy.body
    );
    assert_eq!(busy.retry_after.as_deref(), Some("7"));
    let refused = server.wake(wake).await;
    assert_eq!(
        (refused.status, code(&refused)),
        (502, "provider_rejected"),
        "{}",
        refused.body
    );
    // Neither is the token's fault: the ticket stays.
    assert_eq!(server.store.counts().await.unwrap().tickets, 1);
}

#[tokio::test]
async fn a_server_that_is_no_gateway_has_no_tickets_and_says_so() {
    let server = gateway("").await;
    let alice = Keys::generate();
    let put = server
        .ask(
            &alice,
            "PUT",
            DEVICE,
            Some(registration("token-of-the-phone")),
        )
        .await;
    assert_eq!(put.status, 200);
    for (method, path) in [
        ("POST", "/v1/tickets"),
        ("GET", "/v1/tickets"),
        ("POST", WAKE),
    ] {
        let answer = server
            .ask(
                &alice,
                method,
                path,
                Some(json!({ "device_id": "phone-0001" })),
            )
            .await;
        assert_eq!(
            (answer.status, code(&answer)),
            (404, "not_found"),
            "{method} {path}"
        );
    }
    let info = server.send("GET", "/v1/info", None, vec![]).await.body;
    assert!(
        info.get("tickets").is_none() && info.get("gateway").is_none(),
        "{info}"
    );

    let gateway = self::gateway("[gateway]\naccept = true\n").await;
    let info = gateway.send("GET", "/v1/info", None, vec![]).await.body;
    assert_eq!(info["tickets"], json!({ "days": 30, "per_device": 8 }));
    assert!(info.get("gateway").is_none());
    assert_eq!(info["apps"], json!([{ "id": APP, "providers": ["fcm"] }]));
}

/// A push with the event inside, and its form by id, as the pipeline makes
/// them.
fn dm_message(event_len: usize) -> Message {
    let mut payload = Payload::new(PushType::Dm);
    payload.trace = Some("abcd1234".into());
    let mut by_id = payload.clone();
    by_id.event_id = Some("e0".repeat(32));
    by_id.relay = Some("wss://relay.my-server.example".into());
    payload.event = Some(format!(
        r#"{{"kind":1059,"content":"{}"}}"#,
        "x".repeat(event_len)
    ));
    Message {
        payload,
        fallback: Some(by_id),
        collapse_key: Some("k".into()),
        ttl: Duration::from_secs(60),
        urgent: true,
    }
}

#[tokio::test]
async fn a_private_server_seals_what_fcm_would_have_got_and_hands_it_to_the_gateway() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(WAKE))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "trace": "5f3a9c1e" })))
        .mount(&gateway)
        .await;
    let (seal, device) = (Keys::generate(), Keys::generate());
    let client =
        GatewayClient::new(&gateway.uri(), Arc::new(SealKey::from_keys(seal.clone()))).unwrap();
    let ticket = "7a".repeat(32);
    let target = Target {
        token: ticket.clone(),
        seal_to: Some(device.public_key().to_hex()),
    };
    let message = dm_message(200);
    let attempt = client.send(&target, &message).await;
    assert_eq!(attempt.outcome, Outcome::Delivered, "{attempt:?}");

    // What FCM would have got for the same push.
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "a", "expires_in": 3600 })),
        )
        .mount(&google)
        .await;
    Mock::given(method("POST"))
        .and(path(SEND))
        .respond_with(ResponseTemplate::new(200))
        .mount(&google)
        .await;
    let account = ServiceAccount::parse(
        &json!({
            "project_id": "veydan-test", "private_key": key::TEST_RSA_KEY,
            "client_email": "push@veydan-test.iam.gserviceaccount.com",
            "token_uri": format!("{}/token", google.uri()),
        })
        .to_string(),
    )
    .unwrap();
    let fcm = FcmClient::new(account, &google.uri()).unwrap();
    assert_eq!(
        fcm.send(&Target::of("token"), &message).await.outcome,
        Outcome::Delivered
    );
    let fcm_body: Value = serde_json::from_slice(
        &google
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path() == SEND)
            .unwrap()
            .body,
    )
    .unwrap();
    let fcm_data = serde_json::to_string(&fcm_body["message"]["data"]).unwrap();

    let woken = gateway.received_requests().await.unwrap();
    assert_eq!(woken.len(), 1);
    let wake: Value = serde_json::from_slice(&woken[0].body).unwrap();
    let keys: Vec<_> = wake.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["blob", "sender", "ticket"]);
    assert_eq!(wake["ticket"], ticket);
    assert_eq!(wake["sender"], seal.public_key().to_hex());
    let sealed = wake["blob"].as_str().unwrap();
    assert!(sealed.len() <= vpush_proto::SEALED_BUDGET);
    assert!(
        !sealed.contains(&ticket) && !sealed.contains("1059"),
        "nothing of the push shows"
    );

    // The device opens it with its push key and the sender, and finds the
    // very bytes FCM would have carried.
    let inside = open(device.secret_key(), &seal.public_key(), sealed).unwrap();
    assert_eq!(inside, fcm_data);
    let data: BTreeMap<String, String> = serde_json::from_str(&inside).unwrap();
    assert_eq!(data, message.payload.to_data());
    assert_eq!(data["v"], "2");
}

/// The gateway learns that a wake is urgent: so it is asked for a call
/// alone, and a message, a count or a test goes at normal priority.
#[tokio::test]
async fn a_private_server_asks_for_an_urgent_wake_for_a_call_alone() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(WAKE))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "trace": "5f3a9c1e" })))
        .mount(&gateway)
        .await;
    let (seal, device) = (Keys::generate(), Keys::generate());
    let client =
        GatewayClient::new(&gateway.uri(), Arc::new(SealKey::from_keys(seal.clone()))).unwrap();
    let target = Target {
        token: "7a".repeat(32),
        seal_to: Some(device.public_key().to_hex()),
    };
    let mut call = dm_message(10);
    call.payload.call = true;
    if let Some(fallback) = call.fallback.as_mut() {
        fallback.call = true;
    }
    for message in [dm_message(10), call] {
        assert_eq!(client.send(&target, &message).await.outcome, Outcome::Delivered);
    }
    let woken: Vec<Value> = gateway
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(woken.len(), 2);
    assert!(woken[0].get("urgent").is_none(), "{}", woken[0]);
    assert_eq!(woken[1]["urgent"], true);
    let inside = open(
        device.secret_key(),
        &seal.public_key(),
        woken[1]["blob"].as_str().unwrap(),
    )
    .unwrap();
    assert!(inside.contains(r#""call":"1""#), "{inside}");
}

#[tokio::test]
async fn a_push_that_does_not_fit_sealed_goes_by_its_id_and_a_lost_ticket_is_a_dead_token() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(WAKE))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "trace": "5f3a9c1e" })))
        .up_to_n_times(1)
        .mount(&gateway)
        .await;
    Mock::given(method("POST"))
        .and(path(WAKE))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": "ticket_unknown", "message": "no such ticket" }, "request_id": "1"
        })))
        .mount(&gateway)
        .await;
    let (seal, device) = (Keys::generate(), Keys::generate());
    let client =
        GatewayClient::new(&gateway.uri(), Arc::new(SealKey::from_keys(seal.clone()))).unwrap();
    let target = Target {
        token: "7a".repeat(32),
        seal_to: Some(device.public_key().to_hex()),
    };

    // An event that fits FCM (3900) and not a sealed push (3600).
    let message = dm_message(2800);
    assert!(message.payload.fits());
    assert_eq!(
        client.send(&target, &message).await.outcome,
        Outcome::Delivered
    );
    let wake: Value =
        serde_json::from_slice(&gateway.received_requests().await.unwrap()[0].body).unwrap();
    let inside = open(
        device.secret_key(),
        &seal.public_key(),
        wake["blob"].as_str().unwrap(),
    )
    .unwrap();
    let data: BTreeMap<String, String> = serde_json::from_str(&inside).unwrap();
    assert_eq!(data, message.fallback.as_ref().unwrap().to_data());
    assert!(!data.contains_key("event"));

    let lost = client.send(&target, &message).await;
    assert_eq!(
        (lost.outcome, lost.code.as_deref()),
        (Outcome::DeadToken, Some("ticket_unknown"))
    );

    // A device with no push key is pushed nothing.
    let none = client.send(&Target::of("7a".repeat(32)), &message).await;
    assert_eq!(
        (none.outcome, none.code.as_deref()),
        (Outcome::Rejected, Some("NO_PUSH_KEY"))
    );
    assert_eq!(gateway.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_gateway_that_says_wait_is_tried_again_by_the_policy_of_the_private_server() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(WAKE))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_json(json!({
                    "error": { "code": "rate_limited", "message": "later" }, "request_id": "1"
                })),
        )
        .up_to_n_times(1)
        .mount(&gateway)
        .await;
    Mock::given(method("POST"))
        .and(path(WAKE))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "trace": "5f3a9c1e" })))
        .mount(&gateway)
        .await;
    let device = Keys::generate();
    let client = GatewayClient::new(
        &gateway.uri(),
        Arc::new(SealKey::from_keys(Keys::generate())),
    )
    .unwrap();
    let target = Target {
        token: "7a".repeat(32),
        seal_to: Some(device.public_key().to_hex()),
    };
    let policy = RetryPolicy {
        max_attempts: 3,
        base: Duration::from_millis(10),
        max_wait: Duration::from_secs(2),
    };
    let delivery = retry::deliver(&client, &target, &dm_message(10), policy).await;
    assert_eq!(delivery.outcome, Outcome::Delivered);
    assert_eq!(delivery.attempts.len(), 2);
    assert_eq!(delivery.attempts[0].outcome, Outcome::Retry);
    assert_eq!(delivery.attempts[0].retry_after_ms, Some(1000));
}

/// The sealed push of `golden/` opens, with the keys its README names, to
/// the push of `push_group.json` as FCM would carry it: a client tests its
/// own opening against the same files.
#[test]
fn the_sealed_push_of_the_examples_opens_with_the_keys_they_name() {
    let golden = |name: &str| {
        let path = format!("{}/../../golden/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    };
    let sealed: Payload = serde_json::from_str(&golden("push_sealed.json")).unwrap();
    let group: Payload = serde_json::from_str(&golden("push_group.json")).unwrap();
    let push_key = SecretKey::from_hex(&"11".repeat(32)).unwrap();
    let server = Keys::new(SecretKey::from_hex(&"22".repeat(32)).unwrap());
    assert_eq!(
        sealed.sender.as_deref(),
        Some(server.public_key().to_hex().as_str())
    );
    let sender = PublicKey::from_hex(sealed.sender.as_deref().unwrap()).unwrap();
    let inside = open(&push_key, &sender, sealed.blob.as_deref().unwrap()).unwrap();
    assert_eq!(inside, serde_json::to_string(&group.to_data()).unwrap());

    // The urgent wake carries a call: the push of `push_dm.json`, marked.
    let urgent: Value = serde_json::from_str(&golden("wake_urgent.json")).unwrap();
    assert_eq!(urgent["urgent"], true);
    assert_eq!(urgent["sender"], server.public_key().to_hex());
    let mut call: Payload = serde_json::from_str(&golden("push_dm.json")).unwrap();
    call.call = true;
    let inside = open(&push_key, &sender, urgent["blob"].as_str().unwrap()).unwrap();
    assert_eq!(inside, serde_json::to_string(&call.to_data()).unwrap());
}

/// The whole way, with no mock in the middle: a device registers at the
/// gateway with its token and takes a ticket, registers at a private server
/// with the ticket and its push key, and the test push of the private server
/// reaches FCM sealed, through the gateway.
#[tokio::test]
async fn a_test_push_of_a_private_server_reaches_the_phone_through_the_gateway() {
    let network = gateway("[gateway]\naccept = true\n").await;
    network
        .fcm_answers(ResponseTemplate::new(200).set_body_json(json!({ "name": "m/1" })))
        .await;
    let alice = Keys::generate();
    let ticket = network.ticket_for(&alice).await;

    let (seal, push_key) = (Keys::generate(), Keys::generate());
    let own = private(&network.base, seal.clone()).await;
    let info = own.send("GET", "/v1/info", None, vec![]).await.body;
    assert_eq!(
        info["gateway"],
        json!({ "url": network.base, "sender": seal.public_key().to_hex() })
    );
    assert_eq!(
        info["apps"],
        json!([{ "id": APP, "providers": ["gateway"] }])
    );
    assert!(info.get("tickets").is_none());

    let channel = json!({ "provider": "gateway", "ticket": ticket, "pubkey": push_key.public_key().to_hex() });
    let put = own
        .ask(&alice, "PUT", DEVICE, Some(json!({ "app_id": APP, "channel": channel, "relays": [{ "url": "wss://node-1.veydan.net" }] })))
        .await;
    assert_eq!(put.status, 200, "{}", put.body);
    // The ticket was asked about the only way it can be: a sealed test push,
    // marked as a check, which the device shows nothing for.
    let pushes = network.pushes().await;
    assert_eq!(pushes.len(), 1);
    let data = &pushes[0]["message"]["data"];
    let inside = open(
        push_key.secret_key(),
        &seal.public_key(),
        data["blob"].as_str().unwrap(),
    )
    .unwrap();
    let inside: Value = serde_json::from_str(&inside).unwrap();
    assert_eq!(
        (inside["type"].as_str(), inside["check"].as_str()),
        (Some("test"), Some("1"))
    );
    let keys: Vec<_> = inside.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["check", "trace", "type", "v"]);
    assert!(
        data.get("check").is_none(),
        "nothing of it shows outside the seal"
    );
    let device = own.ask(&alice, "GET", DEVICE, None).await.body;
    assert_eq!(
        (device["provider"].as_str(), device["state"].as_str()),
        (Some("gateway"), Some("active"))
    );
    assert!(
        !device.to_string().contains(&ticket),
        "the ticket is not said back"
    );

    let test = own
        .ask(&alice, "POST", &format!("{DEVICE}/test"), None)
        .await;
    assert_eq!(test.status, 200, "{}", test.body);
    assert_eq!(test.body["outcome"], "delivered");
    let pushes = network.pushes().await;
    assert_eq!(pushes.len(), 2);
    let data = &pushes[1]["message"]["data"];
    assert_eq!(data["type"], "sealed");
    assert_eq!(data["sender"], seal.public_key().to_hex());
    let inside = open(
        push_key.secret_key(),
        &seal.public_key(),
        data["blob"].as_str().unwrap(),
    )
    .unwrap();
    let inside: Value = serde_json::from_str(&inside).unwrap();
    assert_eq!(
        inside,
        json!({ "v": "2", "type": "test", "trace": test.body["trace"] })
    );

    // The ticket deleted at the gateway, the private server forgets the
    // device as it would a dead token.
    assert_eq!(
        network
            .ask(&alice, "DELETE", &format!("/v1/tickets/{ticket}"), None)
            .await
            .status,
        204
    );
    let test = own
        .ask(&alice, "POST", &format!("{DEVICE}/test"), None)
        .await;
    assert_eq!(test.body["outcome"], "dead_token", "{}", test.body);
    assert_eq!(
        own.ask(&alice, "GET", DEVICE, None).await.body["state"],
        "dead_token"
    );

    // A ticket the gateway never issued is refused at registration.
    let channel = json!({ "provider": "gateway", "ticket": "ee".repeat(32), "pubkey": push_key.public_key().to_hex() });
    let put = own
        .ask(
            &alice,
            "PUT",
            "/v1/devices/phone-0002",
            Some(json!({ "app_id": APP, "channel": channel })),
        )
        .await;
    assert_eq!(
        (put.status, code(&put)),
        (422, "token_invalid"),
        "{}",
        put.body
    );
    // And a channel that is not one, before anything is asked.
    for channel in [
        json!({ "provider": "gateway", "ticket": "short", "pubkey": push_key.public_key().to_hex() }),
        json!({ "provider": "gateway", "ticket": "ee".repeat(32), "pubkey": "00".repeat(32) }),
        json!({ "provider": "fcm", "token": "t" }),
    ] {
        let put = own
            .ask(
                &alice,
                "PUT",
                "/v1/devices/phone-0003",
                Some(json!({ "app_id": APP, "channel": channel })),
            )
            .await;
        assert!(
            put.status == 400 || code(&put) == "provider_disabled",
            "{channel}: {}",
            put.body
        );
    }
}
