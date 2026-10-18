//! vguard between a client and a relay, over real sockets: a fake strfry
//! that records what reaches it, and nostr-sdk's own relay with the
//! client the app is built on.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr_sdk::prelude::*;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use vguard::config::Config;
use vguard::proxy::{self, Shared};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A strfry of a test: gives a challenge as the connection opens (when
/// `challenge`), answers REQ with EOSE, AUTH with a refusal of its own (as
/// strfry does for an alias), NEG-OPEN with a NEG-MSG; keeps every frame
/// and the X-Real-IP of each connection. A REQ is answered with the events
/// of `served` first, whatever it asked: what a filter would match is
/// strfry's business, what reaches the client is vguard's.
#[derive(Clone, Default)]
struct Fake {
    frames: Arc<Mutex<Vec<Value>>>,
    real_ips: Arc<Mutex<Vec<String>>>,
    served: Arc<Mutex<Vec<Value>>>,
}

impl Fake {
    async fn start(challenge: bool) -> (Self, SocketAddr) {
        let fake = Fake::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let f = fake.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let f = f.clone();
                tokio::spawn(async move {
                    let mut ip = None;
                    #[allow(clippy::result_large_err)]
                    let cb = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                        ip = req
                            .headers()
                            .get("x-real-ip")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        Ok(resp)
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else {
                        return;
                    };
                    f.real_ips.lock().unwrap().push(ip.unwrap_or_default());
                    if challenge {
                        ws.send(Message::text(
                            json!(["AUTH", "strfry-challenge"]).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    while let Some(Ok(m)) = ws.next().await {
                        let Message::Text(t) = m else { continue };
                        let v: Value = serde_json::from_str(t.as_str()).unwrap();
                        f.frames.lock().unwrap().push(v.clone());
                        if v[0] == "REQ" {
                            let served = f.served.lock().unwrap().clone();
                            for e in served {
                                if ws.send(Message::text(json!(["EVENT", v[1], e]).to_string())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        let answer = match v[0].as_str() {
                            Some("REQ") => Some(json!(["EOSE", v[1]])),
                            Some("AUTH") => Some(json!([
                                "OK",
                                v[1]["id"],
                                false,
                                "auth failed: incorrect or missing relay tag"
                            ])),
                            Some("NEG-OPEN") => Some(json!(["NEG-MSG", v[1], "61"])),
                            Some("EVENT") => Some(json!(["OK", v[1]["id"], true, ""])),
                            _ => None,
                        };
                        if let Some(a) = answer {
                            if ws.send(Message::text(a.to_string())).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });
        (fake, addr)
    }

    fn verbs_with(&self, sub: &str) -> Vec<String> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v[1] == sub)
            .map(|v| v[0].as_str().unwrap().to_string())
            .collect()
    }

    /// Every frame strfry got, in order: the verb and, but for an AUTH, the
    /// subscription id (of an EVENT, its id).
    fn order(&self) -> Vec<String> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .map(|v| match (v[0].as_str().unwrap(), &v[1]) {
                ("AUTH", _) => "AUTH".to_string(),
                (verb, Value::String(s)) => format!("{verb} {s}"),
                (verb, e) => format!("{verb} {}", e["id"].as_str().unwrap_or_default()),
            })
            .collect()
    }

    fn saw(&self, verb: &str) -> usize {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v[0] == verb)
            .count()
    }
}

async fn guard(upstream: SocketAddr, extra: &str) -> (SocketAddr, Arc<Shared>) {
    let config = Config::parse(&format!(
        "upstream = \"{upstream}\"\nrelay_hosts = [\"relay.example.org\", \"127.0.0.1\"]\n{extra}"
    ))
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shared = Shared::new(config);
    tokio::spawn(proxy::run(listener, shared.clone()));
    (addr, shared)
}

async fn connect(addr: SocketAddr, headers: &[(&'static str, &str)]) -> Ws {
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    for (k, v) in headers {
        req.headers_mut()
            .insert(*k, HeaderValue::from_str(v).unwrap());
    }
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

/// The next text frame, within two seconds.
async fn recv(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("an answer in time");
        match m.expect("the connection is open").expect("a frame") {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Close(_) => panic!("closed"),
            _ => continue,
        }
    }
}

fn auth_event(keys: &Keys, challenge: &str, relay: &str) -> Value {
    let ev = EventBuilder::new(Kind::Authentication, "")
        .tags([
            Tag::parse(["challenge", challenge]).unwrap(),
            Tag::parse(["relay", relay]).unwrap(),
        ])
        .finalize(keys)
        .unwrap();
    serde_json::to_value(&ev).unwrap()
}

#[tokio::test]
async fn a_client_signs_in_and_reads_its_own_wraps_only() {
    let (fake, up) = Fake::start(true).await;
    // No wait for the AUTH: a question before it is refused at once.
    let (addr, shared) = guard(up, "[limits]\nauth_wait_ms = 0\n").await;
    let keys = Keys::generate();
    let me = keys.public_key().to_hex();
    let other = Keys::generate().public_key().to_hex();
    let mut ws = connect(addr, &[("x-real-ip", "198.51.100.9")]).await;

    // strfry's challenge comes through.
    assert_eq!(recv(&mut ws).await, json!(["AUTH", "strfry-challenge"]));

    // Before signing in: auth-required, and strfry never hears of it.
    send(&mut ws, json!(["REQ", "dm", {"kinds": [1059], "#p": [me]}])).await;
    let a = recv(&mut ws).await;
    assert_eq!(a[0], "CLOSED");
    assert!(a[2].as_str().unwrap().starts_with("auth-required:"), "{a}");

    // Signing in: vguard says OK, strfry gets the AUTH as it is, and its own
    // refusal does not reach the client.
    let ev = auth_event(
        &keys,
        "strfry-challenge",
        "wss://relay.example.org/?key=appkey",
    );
    send(&mut ws, json!(["AUTH", ev])).await;
    let ok = recv(&mut ws).await;
    assert_eq!(ok, json!(["OK", ev["id"], true, ""]));

    send(&mut ws, json!(["REQ", "dm", {"kinds": [1059], "#p": [me]}])).await;
    assert_eq!(
        recv(&mut ws).await,
        json!(["EOSE", "dm"]),
        "no second OK came before it"
    );
    assert_eq!(fake.saw("AUTH"), 1);

    for (sub, filter) in [
        ("theirs", json!({"kinds": [1059], "#p": [other]})),
        ("all-wraps", json!({"kinds": [1059]})),
        ("groups", json!({"kinds": [9]})),
        ("everything", json!({})),
    ] {
        send(&mut ws, json!(["REQ", sub, filter])).await;
        let a = recv(&mut ws).await;
        assert_eq!(a[0], "CLOSED", "{sub}: {a}");
        assert!(
            a[2].as_str().unwrap().starts_with("restricted:"),
            "{sub}: {a}"
        );
        assert!(fake.verbs_with(sub).is_empty(), "{sub} reached strfry");
    }
    send(&mut ws, json!(["COUNT", "count", {"kinds": [1059]}])).await;
    assert_eq!(recv(&mut ws).await[0], "CLOSED");
    send(&mut ws, json!(["NEG-OPEN", "neg", {"kinds": [1059]}, "61"])).await;
    let a = recv(&mut ws).await;
    assert_eq!(a[0], "NEG-ERR");
    assert!(fake.verbs_with("neg").is_empty());

    // What is allowed goes through.
    send(
        &mut ws,
        json!(["REQ", "group", {"kinds": [9], "#h": ["g1"]}]),
    )
    .await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "group"]));
    send(
        &mut ws,
        json!(["REQ", "profiles", {"kinds": [0], "authors": [other]}]),
    )
    .await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "profiles"]));
    send(
        &mut ws,
        json!(["NEG-OPEN", "sync", {"kinds": [1059], "#p": [me]}, "61"]),
    )
    .await;
    assert_eq!(recv(&mut ws).await, json!(["NEG-MSG", "sync", "61"]));
    send(
        &mut ws,
        json!(["REQ", "pushed", {"ids": ["ab".repeat(32)]}]),
    )
    .await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "pushed"]));

    // The address nginx named reaches strfry.
    assert_eq!(fake.real_ips.lock().unwrap().as_slice(), ["198.51.100.9"]);
    let m = shared.metrics.line();
    assert!(m.contains("refused_restricted=6"), "{m}");
    assert!(m.contains("auth_ok=1"), "{m}");
}

#[tokio::test]
async fn a_peer_named_by_nginx_reads_everything() {
    let (fake, up) = Fake::start(true).await;
    let (addr, shared) = guard(up, "").await;
    let mut ws = connect(addr, &[("x-veydan-role", "peer")]).await;
    assert_eq!(recv(&mut ws).await[0], "AUTH");
    send(&mut ws, json!(["REQ", "mesh", {}])).await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "mesh"]));
    send(&mut ws, json!(["NEG-OPEN", "sync", {}, "61"])).await;
    assert_eq!(recv(&mut ws).await, json!(["NEG-MSG", "sync", "61"]));
    send(&mut ws, json!(["REQ", "wraps", {"kinds": [1059]}])).await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "wraps"]));
    // Frames pass as they are, even what vguard would not read.
    ws.send(Message::text(r#"["REQ","dup",{"kinds":[0],"kinds":[1]}]"#))
        .await
        .unwrap();
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "dup"]));
    assert!(shared.metrics.line().contains("peers=1"));
    assert_eq!(fake.saw("REQ"), 3);
}

#[tokio::test]
async fn without_a_challenge_of_strfry_vguard_gives_its_own_and_keeps_the_auth() {
    let (fake, up) = Fake::start(false).await;
    let (addr, _) = guard(up, "[limits]\nchallenge_wait_ms = 100\n").await;
    let keys = Keys::generate();
    let me = keys.public_key().to_hex();
    let mut ws = connect(addr, &[]).await;
    send(&mut ws, json!(["REQ", "dm", {"kinds": [1059], "#p": [me]}])).await;
    let c = recv(&mut ws).await;
    assert_eq!(c[0], "AUTH");
    // The question waits for the AUTH over vguard's challenge, and is
    // answered after it.
    let ev = auth_event(&keys, c[1].as_str().unwrap(), "wss://relay.example.org");
    send(&mut ws, json!(["AUTH", ev])).await;
    assert_eq!(recv(&mut ws).await[2], true);
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "dm"]));
    send(&mut ws, json!(["REQ", "dm2", {"kinds": [1059], "#p": [me]}])).await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "dm2"]));
    assert_eq!(
        fake.saw("AUTH"),
        0,
        "strfry does not know vguard's challenge"
    );
}

/// The app asks for its history as the connection opens, before it has
/// signed in: with strfry's challenge out, the question waits for the
/// AUTH and is answered after it, not refused (the refusal sent the app
/// to a plain REQ, which strfry caps at its newest 500 events).
#[tokio::test]
async fn a_question_before_the_auth_is_answered_after_it() {
    let (fake, up) = Fake::start(true).await;
    let (addr, shared) = guard(up, "").await;
    let keys = Keys::generate();
    let me = keys.public_key().to_hex();
    let mut ws = connect(addr, &[]).await;
    assert_eq!(recv(&mut ws).await, json!(["AUTH", "strfry-challenge"]));

    send(&mut ws, json!(["NEG-OPEN", "history", {"kinds": [1059], "#p": [me]}, "61"])).await;
    send(&mut ws, json!(["REQ", "dm", {"kinds": [1059], "#p": [me]}])).await;
    // Nothing yet: neither an answer nor strfry hearing of it.
    let early = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
    assert!(early.is_err(), "{early:?}");
    assert!(fake.verbs_with("history").is_empty() && fake.verbs_with("dm").is_empty());

    let ev = auth_event(&keys, "strfry-challenge", "wss://relay.example.org");
    send(&mut ws, json!(["AUTH", ev])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", ev["id"], true, ""]));
    assert_eq!(recv(&mut ws).await, json!(["NEG-MSG", "history", "61"]));
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "dm"]));
    // strfry got the AUTH first, then the questions as they were sent.
    assert_eq!(fake.order(), ["AUTH", "NEG-OPEN history", "REQ dm"]);
    let m = shared.metrics.line();
    assert!(m.contains("refused_auth_required=0") && m.contains("auth_waited=1"), "{m}");

    // The same AUTH again: its challenge has signed in once.
    send(&mut ws, json!(["AUTH", ev])).await;
    let again = recv(&mut ws).await;
    assert_eq!(again[2], false, "{again}");
    assert_eq!(fake.saw("AUTH"), 1);
}

/// Everything the client sends while a question waits keeps its place
/// behind it: a later REQ of the same id replaces the held one at strfry,
/// as it would have without the wait; only the AUTH goes ahead.
#[tokio::test]
async fn the_frames_behind_a_held_question_keep_their_order() {
    let (fake, up) = Fake::start(true).await;
    let (addr, _) = guard(up, "").await;
    let keys = Keys::generate();
    let me = keys.public_key().to_hex();
    let mut ws = connect(addr, &[]).await;
    recv(&mut ws).await;
    let note = serde_json::to_value(EventBuilder::new(Kind::Metadata, "{}").finalize(&keys).unwrap()).unwrap();
    send(&mut ws, json!(["REQ", "dm", {"kinds": [1059], "#p": [me]}])).await;
    send(&mut ws, json!(["EVENT", note])).await;
    send(&mut ws, json!(["REQ", "dm", {"kinds": [0], "authors": [me]}])).await;
    send(&mut ws, json!(["REQ", "pub", {"kinds": [0]}])).await;
    send(&mut ws, json!(["CLOSE", "pub"])).await;
    let ev = auth_event(&keys, "strfry-challenge", "wss://relay.example.org");
    send(&mut ws, json!(["AUTH", ev])).await;
    assert_eq!(recv(&mut ws).await[0], "OK");
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "dm"]));
    assert_eq!(recv(&mut ws).await, json!(["OK", note["id"], true, ""]));
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "dm"]));
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "pub"]));
    // The CLOSE has no answer: wait until strfry has it.
    for _ in 0..40 {
        if fake.order().len() == 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        fake.order(),
        ["AUTH", "REQ dm", &format!("EVENT {}", note["id"].as_str().unwrap()), "REQ dm", "REQ pub", "CLOSE pub"]
    );
}

/// A client that does not answer the challenge: its question is refused
/// after `auth_wait_ms`, as before, and the next one at once.
#[tokio::test]
async fn a_question_without_an_auth_is_refused_after_the_wait() {
    let (fake, up) = Fake::start(true).await;
    let (addr, shared) = guard(up, "[limits]\nauth_wait_ms = 400\n").await;
    let me = Keys::generate().public_key().to_hex();
    let mut ws = connect(addr, &[]).await;
    recv(&mut ws).await;
    let t = std::time::Instant::now();
    send(&mut ws, json!(["REQ", "dm", {"kinds": [1059], "#p": [me]}])).await;
    let a = recv(&mut ws).await;
    assert_eq!(a[0], "CLOSED");
    assert!(a[2].as_str().unwrap().starts_with("auth-required:"), "{a}");
    assert!(t.elapsed() >= Duration::from_millis(350), "{:?}", t.elapsed());
    // The connection waited once: the next question is answered at once.
    let t = std::time::Instant::now();
    send(&mut ws, json!(["NEG-OPEN", "n", {"kinds": [1059], "#p": [me]}, "61"])).await;
    let a = recv(&mut ws).await;
    assert_eq!(a[0], "NEG-ERR");
    assert!(a[2].as_str().unwrap().starts_with("auth-required:"), "{a}");
    assert!(t.elapsed() < Duration::from_millis(300), "{:?}", t.elapsed());
    assert!(fake.verbs_with("dm").is_empty() && fake.verbs_with("n").is_empty());
    assert!(shared.metrics.line().contains("auth_wait_expired=1"), "{}", shared.metrics.line());
}

/// At most MAX_HELD frames wait: one more ends the wait at once, and the
/// held question is refused as without it.
#[tokio::test]
async fn a_client_that_sends_too_much_ends_the_wait() {
    let (fake, up) = Fake::start(true).await;
    let (addr, shared) = guard(up, "").await;
    let me = Keys::generate().public_key().to_hex();
    let mut ws = connect(addr, &[]).await;
    recv(&mut ws).await;
    let t = std::time::Instant::now();
    send(&mut ws, json!(["REQ", "dm", {"kinds": [1059], "#p": [me]}])).await;
    // The question and 31 more fit; the 33rd frame does not.
    for i in 0..proxy::MAX_HELD {
        send(&mut ws, json!(["REQ", format!("p{i}"), {"kinds": [0]}])).await;
    }
    let a = recv(&mut ws).await;
    assert_eq!(a[0], "CLOSED");
    assert!(a[2].as_str().unwrap().starts_with("auth-required:"), "{a}");
    for i in 0..proxy::MAX_HELD {
        assert_eq!(recv(&mut ws).await, json!(["EOSE", format!("p{i}")]));
    }
    assert!(t.elapsed() < Duration::from_secs(3), "long before auth_wait_ms: {:?}", t.elapsed());
    assert_eq!(fake.saw("REQ"), proxy::MAX_HELD);
    assert!(shared.metrics.line().contains("auth_wait_overflow=1"), "{}", shared.metrics.line());
}

#[tokio::test]
async fn limits_hold() {
    let (_fake, up) = Fake::start(true).await;
    let (addr, shared) = guard(up, "[limits]\nmax_frame = 4096\nmax_subs = 2\n").await;
    let mut ws = connect(addr, &[]).await;
    recv(&mut ws).await;
    for id in ["a", "b"] {
        send(&mut ws, json!(["REQ", id, {"kinds": [0]}])).await;
        assert_eq!(recv(&mut ws).await[0], "EOSE");
    }
    send(&mut ws, json!(["REQ", "c", {"kinds": [0]}])).await;
    let a = recv(&mut ws).await;
    assert_eq!(a[0], "CLOSED");
    assert!(a[2].as_str().unwrap().starts_with("error:"), "{a}");
    // A binary frame is no nostr message.
    ws.send(Message::binary(b"[\"REQ\",\"x\",{}]".to_vec()))
        .await
        .unwrap();
    assert_eq!(recv(&mut ws).await[0], "NOTICE");
    // A frame over the limit ends the connection.
    let big = json!(["EVENT", {"content": "x".repeat(8192)}]).to_string();
    ws.send(Message::text(big)).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                _ => continue,
            }
        }
    })
    .await;
    assert!(end.is_ok(), "the connection ends");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        shared.metrics.line().contains("too_big=1"),
        "{}",
        shared.metrics.line()
    );
}

#[tokio::test]
async fn no_strfry_no_connection() {
    // A port nobody listens on.
    let free = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (addr, shared) = guard(free, "").await;
    let mut ws = connect(addr, &[]).await;
    let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap();
    assert!(
        matches!(m, None | Some(Ok(Message::Close(_))) | Some(Err(_))),
        "{m:?}"
    );
    assert!(shared.metrics.line().contains("upstream_down=1"));
}

/// The app's way: nostr-sdk with the key as its authenticator, the app key
/// in the query, a subscription to its gift wraps before it signed in.
/// nostr-sdk's relay gives no challenge (as a strfry without auth), so
/// vguard gives its own; nostr-sdk signs in and asks again by itself.
#[tokio::test]
async fn the_client_of_the_app_gets_its_wraps_and_no_one_elses() {
    let relay = LocalRelay::builder().build();
    relay.run().await.unwrap();
    let url = relay.url().await;
    let up: SocketAddr = url
        .as_str_without_trailing_slash()
        .trim_start_matches("ws://")
        .parse()
        .unwrap();
    let (addr, shared) = guard(up, "[limits]\nchallenge_wait_ms = 200\n").await;

    let me = Keys::generate();
    let other = Keys::generate();
    let sender = Keys::generate();
    let wrap_to = |to: &Keys, text: &str| {
        EventBuilder::new(Kind::GiftWrap, text)
            .tag(Tag::public_key(to.public_key()))
            .finalize(&sender)
            .unwrap()
    };
    // One to me and one to someone else, already on the relay.
    relay.add_event(wrap_to(&me, "old to me")).await.unwrap();
    relay
        .add_event(wrap_to(&other, "old to other"))
        .await
        .unwrap();

    let client = Client::builder()
        .authenticator(SignerAuthenticator::new(me.clone()))
        .build();
    let guarded = format!("ws://{addr}/?key=appkey");
    client.add_relay(guarded.as_str()).await.unwrap();
    client.connect().await;
    let mut notes = client.notifications();
    client
        .subscribe(Filter::new().kind(Kind::GiftWrap).pubkey(me.public_key()))
        .await
        .unwrap();

    // A new one to me, and one to the other, sent past vguard.
    let direct = Client::default();
    direct.add_relay(url.as_str()).await.unwrap();
    direct.connect().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    direct.send_event(&wrap_to(&me, "new to me")).await.unwrap();
    direct
        .send_event(&wrap_to(&other, "new to other"))
        .await
        .unwrap();

    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(n) = notes.next().await {
            if let ClientNotification::Event { event, .. } = n {
                got.push(event.content.clone());
                if got.len() == 2 {
                    break;
                }
            }
        }
    })
    .await;
    got.sort();
    assert_eq!(got, ["new to me", "old to me"], "{}", shared.metrics.line());

    // The same client asking for the other's wraps is refused.
    let theirs = client
        .fetch_events(
            Filter::new()
                .kind(Kind::GiftWrap)
                .pubkey(other.public_key()),
        )
        .timeout(Duration::from_secs(3))
        .await;
    assert!(
        theirs.as_ref().map(|e| e.is_empty()).unwrap_or(true),
        "{theirs:?}"
    );
    let m = shared.metrics.line();
    assert!(m.contains("auth_ok=1"), "{m}");
    assert!(m.contains("challenges_own=1"), "{m}");
    client.disconnect().await;
    direct.disconnect().await;
}

#[tokio::test]
async fn a_denied_key_is_refused_at_the_relay_and_the_file_is_read_again() {
    let dir = std::env::temp_dir().join(format!("vguard-denied-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("denied-keys.txt");
    let bad = Keys::generate();
    let good = Keys::generate();
    std::fs::write(&file, format!("{}\n", bad.public_key().to_hex())).unwrap();
    let (fake, up) = Fake::start(true).await;
    let (addr, shared) = guard(up, &format!("denied_keys_file = \"{}\"", file.display())).await;
    assert_eq!(shared.denied.len(), 1, "read at the start");
    let mut ws = connect(addr, &[("x-real-ip", "198.51.100.9")]).await;
    assert_eq!(recv(&mut ws).await, json!(["AUTH", "strfry-challenge"]));

    // AUTH of the denied key: refused here, strfry never hears of it.
    let ev = auth_event(&bad, "strfry-challenge", "wss://relay.example.org");
    send(&mut ws, json!(["AUTH", ev])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", ev["id"], false, "restricted: key denied"]));
    assert_eq!(fake.saw("AUTH"), 0);
    // Its profile: refused; another's goes on to strfry.
    let profile = serde_json::to_value(EventBuilder::new(Kind::Metadata, "{}").finalize(&bad).unwrap()).unwrap();
    send(&mut ws, json!(["EVENT", profile])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", profile["id"], false, "blocked: key denied"]));
    let theirs = serde_json::to_value(EventBuilder::new(Kind::Metadata, "{}").finalize(&good).unwrap()).unwrap();
    send(&mut ws, json!(["EVENT", theirs])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", theirs["id"], true, ""]));
    assert_eq!(fake.saw("EVENT"), 1);
    // Its events asked by author: empty, without strfry.
    send(&mut ws, json!(["REQ", "p", {"kinds": [0], "authors": [bad.public_key().to_hex()]}])).await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "p"]));
    assert!(fake.verbs_with("p").is_empty());

    // Lifted: the file without it, looked at again; its events pass.
    std::fs::write(&file, "").unwrap();
    assert_eq!(shared.denied.refresh(), vpush_proto::denied::Refreshed::Loaded(0));
    let again = serde_json::to_value(EventBuilder::new(Kind::Metadata, "{\"name\":\"x\"}").finalize(&bad).unwrap()).unwrap();
    send(&mut ws, json!(["EVENT", again])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", again["id"], true, ""]));
    let _ = std::fs::remove_dir_all(&dir);
}

/// An event of a group, as the app signs it: kind 9 with the `h` of the group.
fn group_event(keys: &Keys, group: &str) -> Value {
    let ev = EventBuilder::new(Kind::from(9u16), "sealed").tags([Tag::parse(["h", group]).unwrap()]).finalize(keys).unwrap();
    serde_json::to_value(&ev).unwrap()
}

#[tokio::test]
async fn a_closed_group_is_neither_given_nor_taken_and_the_file_is_read_again() {
    let dir = std::env::temp_dir().join(format!("vguard-groups-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("denied-groups.txt");
    let (bad, good) = ("9f".repeat(32), "0a".repeat(32));
    std::fs::write(&file, format!("# closed by vcloud\n{bad}\n")).unwrap();
    let (fake, up) = Fake::start(true).await;
    let member = Keys::generate();
    let (theirs, ours) = (group_event(&member, &bad), group_event(&member, &good));
    *fake.served.lock().unwrap() = vec![theirs.clone(), ours.clone()];
    let (addr, shared) = guard(up, &format!("denied_groups_file = \"{}\"", file.display())).await;
    assert_eq!(shared.denied_groups.len(), 1, "read at the start");
    let mut ws = connect(addr, &[("x-real-ip", "198.51.100.9")]).await;
    assert_eq!(recv(&mut ws).await, json!(["AUTH", "strfry-challenge"]));

    // Of the closed group alone: empty, without strfry.
    send(&mut ws, json!(["REQ", "g", {"kinds": [9], "#h": [bad]}])).await;
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "g"]));
    assert!(fake.verbs_with("g").is_empty());
    // With the open one: strfry is asked for the open one alone, and what it
    // sends of the closed one does not come through.
    send(&mut ws, json!(["REQ", "both", {"kinds": [9], "#h": [bad, good]}])).await;
    assert_eq!(recv(&mut ws).await, json!(["EVENT", "both", ours]));
    assert_eq!(recv(&mut ws).await, json!(["EOSE", "both"]));
    let asked = fake.frames.lock().unwrap().iter().find(|v| v[1] == "both").cloned().unwrap();
    assert_eq!(asked[2]["#h"], json!([good]));
    // An event of it, by a member: refused here; of the open one: stored.
    send(&mut ws, json!(["EVENT", theirs])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", theirs["id"], false, "blocked: group denied"]));
    send(&mut ws, json!(["EVENT", ours])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", ours["id"], true, ""]));
    assert_eq!(fake.saw("EVENT"), 1);

    // Opened again: the file without it, looked at again; its events pass.
    std::fs::write(&file, "").unwrap();
    assert_eq!(shared.denied_groups.refresh(), vpush_proto::denied::Refreshed::Loaded(0));
    send(&mut ws, json!(["EVENT", theirs])).await;
    assert_eq!(recv(&mut ws).await, json!(["OK", theirs["id"], true, ""]));
    send(&mut ws, json!(["REQ", "again", {"kinds": [9], "#h": [bad]}])).await;
    assert_eq!(recv(&mut ws).await, json!(["EVENT", "again", theirs]));
    let _ = std::fs::remove_dir_all(&dir);
}
