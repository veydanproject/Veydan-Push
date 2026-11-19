//! The APNs client against a server that plays Apple, over HTTP/2 (h2c).

#[path = "support/p8.rs"]
mod p8;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{
    EcdsaKeyPair, KeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_FIXED,
    ECDSA_P256_SHA256_FIXED_SIGNING,
};
use serde_json::{json, Value};
use vpush_proto::{Payload, PushType};
use vpush_server::delivery::apns::{self, ApnsClient, SigningKey};
use vpush_server::delivery::retry::{deliver, RetryPolicy};
use vpush_server::delivery::{Message, Outcome, PushProvider, Target, TokenCheck};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOPIC: &str = "net.veydan.mobile";
const KEY_ID: &str = "ABC123DEFG";
const TEAM_ID: &str = "TEAM123456";

fn token() -> String {
    "0f".repeat(32)
}

fn device() -> String {
    format!("/3/device/{}", token())
}

/// The client, and the public key its provider tokens verify with.
fn client(server: &MockServer) -> (ApnsClient, Vec<u8>) {
    let (pem, der) = p8::p8();
    let public =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &SystemRandom::new())
            .unwrap()
            .public_key()
            .as_ref()
            .to_vec();
    let key = SigningKey::parse(&pem).unwrap();
    let client = ApnsClient::new(key, KEY_ID, TEAM_ID, TOPIC, Some(&server.uri())).unwrap();
    (client, public)
}

fn dm() -> Message {
    let mut payload = Payload::new(PushType::Dm);
    payload.event = Some(r#"{"id":"e1","kind":1059,"content":"sealed"}"#.into());
    payload.trace = Some("abcd1234".into());
    let mut fallback = Payload::new(PushType::Dm);
    fallback.event_id = Some("e1".into());
    fallback.relay = Some("wss://node-1.veydan.net".into());
    fallback.trace = Some("abcd1234".into());
    Message {
        payload,
        fallback: Some(fallback),
        collapse_key: Some("3fa1c2d4".into()),
        ttl: Duration::from_secs(3600),
        urgent: true,
        background: false,
    }
}

fn apple(status: u16, reason: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "reason": reason }))
}

async fn answers(server: &MockServer, answer: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(device()))
        .respond_with(answer)
        .mount(server)
        .await;
}

async fn sent(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).map(|v| v.to_str().unwrap())
}

fn bearer(request: &Request) -> String {
    header(request, "authorization")
        .unwrap()
        .strip_prefix("bearer ")
        .unwrap()
        .to_string()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn fast() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base: Duration::from_millis(20),
        max_wait: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn a_message_goes_as_an_alert_the_extension_writes_with_a_provider_token() {
    let server = MockServer::start().await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, public) = client(&server);

    let attempt = client.send(&Target::of(token()), &dm()).await;
    assert_eq!(attempt.outcome, Outcome::Delivered, "{attempt:?}");
    assert_eq!(attempt.http_status, Some(200));

    let sent = sent(&server).await;
    assert_eq!(sent.len(), 1);
    let request = &sent[0];
    assert_eq!(request.url.path(), device());
    assert_eq!(header(request, "apns-topic"), Some(TOPIC));
    assert_eq!(header(request, "apns-push-type"), Some("alert"));
    assert_eq!(header(request, "apns-priority"), Some("10"));
    let expires: u64 = header(request, "apns-expiration").unwrap().parse().unwrap();
    assert!(expires.abs_diff(unix_now() + 3600) <= 2, "{expires}");
    assert_eq!(
        header(request, "apns-collapse-id"),
        None,
        "it would replace the notification on the screen"
    );

    // The provider token: ES256, the key and the team, made just now.
    let jwt = bearer(request);
    let parts: Vec<&str> = jwt.split('.').collect();
    let header_of_jwt: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(header_of_jwt, json!({ "alg": "ES256", "kid": KEY_ID }));
    assert_eq!(claims["iss"], TEAM_ID);
    assert!(claims["iat"].as_u64().unwrap().abs_diff(unix_now()) <= 2);
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &public)
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
        )
        .unwrap();

    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        body["aps"],
        json!({ "mutable-content": 1, "alert": { "loc-key": "push.new" }, "sound": "default", "thread-id": "3fa1c2d4" })
    );
    assert_eq!(body["v"], "2");
    assert_eq!(body["type"], "dm");
    assert_eq!(
        body["event"],
        r#"{"id":"e1","kind":1059,"content":"sealed"}"#
    );
    assert!(body.get("event_id").is_none());

    // The next push takes the same token.
    client.send(&Target::of(token()), &dm()).await;
    let sent = self::sent(&server).await;
    assert_eq!(bearer(&sent[1]), jwt);
}

#[tokio::test]
async fn a_push_with_nothing_to_show_wakes_the_app_in_the_background() {
    let server = MockServer::start().await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);
    let mut keeper = dm();
    keeper.background = true;

    let attempt = client.send(&Target::of(token()), &keeper).await;
    assert_eq!(attempt.outcome, Outcome::Delivered, "{attempt:?}");
    let sent = sent(&server).await;
    assert_eq!(header(&sent[0], "apns-push-type"), Some("background"));
    assert_eq!(
        header(&sent[0], "apns-priority"),
        Some("5"),
        "APNs refuses a background push at 10"
    );
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["aps"], json!({ "content-available": 1 }));
}

#[tokio::test]
async fn a_push_with_no_time_to_live_is_now_or_never() {
    let server = MockServer::start().await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);
    let mut once = dm();
    once.ttl = Duration::ZERO;
    once.urgent = false;
    client.send(&Target::of(token()), &once).await;
    let sent = sent(&server).await;
    assert_eq!(header(&sent[0], "apns-expiration"), Some("0"));
    assert_eq!(header(&sent[0], "apns-priority"), Some("5"));
}

#[tokio::test]
async fn a_push_too_big_for_apns_goes_by_its_id_in_one_request() {
    let server = MockServer::start().await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);
    let mut big = dm();
    big.payload.event = Some(format!(r#"{{"content":"{}"}}"#, "\"".repeat(2500)));
    assert!(big.payload.fits(), "FCM would take it as it is");

    let attempt = client.send(&Target::of(token()), &big).await;
    assert_eq!(attempt.outcome, Outcome::Delivered, "{attempt:?}");
    let sent = sent(&server).await;
    assert_eq!(sent.len(), 1, "measured here, not refused there first");
    assert!(sent[0].body.len() <= apns::BODY_LIMIT);
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert!(body.get("event").is_none());
    assert_eq!(body["event_id"], "e1");
    assert_eq!(body["relay"], "wss://node-1.veydan.net");
}

#[tokio::test]
async fn a_push_too_big_with_no_other_form_is_not_sent() {
    let server = MockServer::start().await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);
    let mut sealed = Payload::new(PushType::Sealed);
    sealed.blob = Some("a".repeat(apns::BODY_LIMIT));
    let mut message = dm();
    message.payload = sealed;
    message.fallback = None;

    let attempt = client.send(&Target::of(token()), &message).await;
    assert_eq!(
        (attempt.outcome, attempt.code.as_deref()),
        (Outcome::Rejected, Some("TOO_BIG"))
    );
    assert!(sent(&server).await.is_empty());
}

#[tokio::test]
async fn an_unregistered_token_is_dead_and_tried_once() {
    let server = MockServer::start().await;
    answers(
        &server,
        ResponseTemplate::new(410)
            .set_body_json(json!({ "reason": "Unregistered", "timestamp": 1_700_000_000_000u64 })),
    )
    .await;
    let (client, _) = client(&server);

    let delivery = deliver(&client, &Target::of(token()), &dm(), fast()).await;
    assert_eq!(delivery.outcome, Outcome::DeadToken);
    assert_eq!(delivery.attempts.len(), 1);
    assert_eq!(delivery.attempts[0].code.as_deref(), Some("Unregistered"));
}

#[tokio::test]
async fn a_bad_token_is_dead_and_a_bad_topic_is_not() {
    let server = MockServer::start().await;
    let (client, _) = client(&server);
    answers(&server, apple(400, "BadDeviceToken")).await;
    assert_eq!(
        client.send(&Target::of(token()), &dm()).await.outcome,
        Outcome::DeadToken
    );

    server.reset().await;
    answers(&server, apple(400, "DeviceTokenNotForTopic")).await;
    let a = client.send(&Target::of(token()), &dm()).await;
    assert_eq!(
        (a.outcome, a.code.as_deref()),
        (Outcome::Rejected, Some("DeviceTokenNotForTopic"))
    );
}

#[tokio::test]
async fn an_expired_provider_token_is_made_anew_once_and_the_push_sent_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(device()))
        .respond_with(apple(403, "ExpiredProviderToken"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);

    let attempt = client.send(&Target::of(token()), &dm()).await;
    assert_eq!(attempt.outcome, Outcome::Delivered, "{attempt:?}");
    let sent = sent(&server).await;
    assert_eq!(sent.len(), 2);
    assert_ne!(bearer(&sent[0]), bearer(&sent[1]), "a new token");
}

#[tokio::test]
async fn a_provider_token_refused_again_is_not_made_anew_for_every_push() {
    let server = MockServer::start().await;
    answers(&server, apple(403, "InvalidProviderToken")).await;
    let (client, _) = client(&server);

    let first = client.send(&Target::of(token()), &dm()).await;
    assert_eq!(
        (first.outcome, first.code.as_deref()),
        (Outcome::Rejected, Some("InvalidProviderToken"))
    );
    assert_eq!(
        sent(&server).await.len(),
        2,
        "made anew once, sent again once"
    );

    // Made anew a moment ago: another one will not help, and Apple would
    // refuse it as too many.
    let second = client.send(&Target::of(token()), &dm()).await;
    assert_eq!(second.outcome, Outcome::Rejected);
    let sent = sent(&server).await;
    assert_eq!(sent.len(), 3);
    assert_eq!(bearer(&sent[1]), bearer(&sent[2]));
}

#[tokio::test]
async fn another_refusal_of_apple_is_not_retried() {
    let server = MockServer::start().await;
    answers(&server, apple(403, "MissingTopic")).await;
    let (client, _) = client(&server);
    let delivery = deliver(&client, &Target::of(token()), &dm(), fast()).await;
    assert_eq!(delivery.outcome, Outcome::Rejected);
    assert_eq!(sent(&server).await.len(), 1);
}

#[tokio::test]
async fn a_busy_apple_is_tried_again_when_it_says() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(device()))
        .respond_with(apple(429, "TooManyRequests").insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(device()))
        .respond_with(apple(503, "ServiceUnavailable"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);

    let delivery = deliver(&client, &Target::of(token()), &dm(), fast()).await;
    assert_eq!(delivery.outcome, Outcome::Delivered, "{delivery:?}");
    assert_eq!(delivery.attempts.len(), 3);
    assert_eq!(delivery.attempts[0].retry_after_ms, Some(1000));
    assert_eq!(delivery.attempts[1].outcome, Outcome::Retry);
}

#[tokio::test]
async fn a_token_that_is_not_hex_never_reaches_apple() {
    let server = MockServer::start().await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);
    let a = client.send(&Target::of("../../other"), &dm()).await;
    assert_eq!(
        (a.outcome, a.code.as_deref()),
        (Outcome::Rejected, Some("NOT_A_TOKEN"))
    );
    assert!(sent(&server).await.is_empty());
}

#[tokio::test]
async fn apple_is_asked_nothing_about_a_token() {
    let server = MockServer::start().await;
    let (client, _) = client(&server);
    assert_eq!(
        client.check(&Target::of(token())).await,
        TokenCheck::Unknown
    );
    assert!(sent(&server).await.is_empty());
}

#[test]
fn the_environment_of_the_device_chooses_the_host() {
    let key = SigningKey::parse(&p8::p8().0).unwrap();
    let client = ApnsClient::new(key, KEY_ID, TEAM_ID, TOPIC, None).unwrap();
    let mut target = Target::of(token());
    assert_eq!(client.host(&target), "https://api.push.apple.com");
    target.sandbox = true;
    assert_eq!(client.host(&target), "https://api.sandbox.push.apple.com");
}

fn voip_token() -> String {
    "1e".repeat(32)
}

/// The invitation to a call, as the pipeline hands it over: a `dm` push
/// marked `call`, alive for a minute.
fn ring() -> Message {
    let mut ring = dm();
    ring.payload.call = true;
    if let Some(fallback) = ring.fallback.as_mut() {
        fallback.call = true;
    }
    ring.collapse_key = Some("c411".into());
    ring.ttl = Duration::from_secs(60);
    ring
}

fn pushkit() -> Target {
    Target {
        voip: true,
        ..Target::of(voip_token())
    }
}

#[tokio::test]
async fn a_push_to_a_voip_token_goes_as_voip_to_the_voip_topic() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{}", voip_token())))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (client, _) = client(&server);

    let attempt = client.send(&pushkit(), &ring()).await;
    assert_eq!(attempt.outcome, Outcome::Delivered, "{attempt:?}");
    let sent = sent(&server).await;
    assert_eq!(sent.len(), 1);
    let request = &sent[0];
    assert_eq!(header(request, "apns-topic"), Some("net.veydan.mobile.voip"));
    assert_eq!(header(request, "apns-push-type"), Some("voip"));
    assert_eq!(header(request, "apns-priority"), Some("10"));
    let expires: u64 = header(request, "apns-expiration").unwrap().parse().unwrap();
    assert!(expires.abs_diff(unix_now() + 60) <= 2, "{expires}");
    assert_eq!(header(request, "apns-collapse-id"), None);

    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert!(body.get("aps").is_none(), "no alert, no badge: the app shows the call");
    assert_eq!(body["v"], "2");
    assert_eq!(body["type"], "dm");
    assert_eq!(body["call"], "1");
    assert_eq!(body["trace"], "abcd1234");
    assert_eq!(body["event"], r#"{"id":"e1","kind":1059,"content":"sealed"}"#);

    // Not urgent by itself, the push of a VoIP token is still at once.
    let mut later = ring();
    later.urgent = false;
    client.send(&pushkit(), &later).await;
    assert_eq!(header(&sent_last(&server).await, "apns-priority"), Some("10"));

    // The same ring to the device token is the alert it always was.
    answers(&server, ResponseTemplate::new(200)).await;
    client.send(&Target::of(token()), &ring()).await;
    let alert = sent_last(&server).await;
    assert_eq!(alert.url.path(), device());
    assert_eq!(header(&alert, "apns-topic"), Some(TOPIC));
    assert_eq!(header(&alert, "apns-push-type"), Some("alert"));
    let alert: Value = serde_json::from_slice(&alert.body).unwrap();
    assert_eq!(alert["aps"]["alert"]["loc-key"], "push.new");
    assert_eq!(alert["call"], "1");
}

async fn sent_last(server: &MockServer) -> Request {
    sent(server).await.pop().unwrap()
}

#[tokio::test]
async fn a_dead_voip_token_is_dead_and_sandbox_goes_to_the_sandbox() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{}", voip_token())))
        .respond_with(apple(410, "Unregistered"))
        .mount(&server)
        .await;
    let (client, _) = client(&server);
    let delivery = deliver(&client, &pushkit(), &ring(), fast()).await;
    assert_eq!(delivery.outcome, Outcome::DeadToken);
    assert_eq!(delivery.attempts.len(), 1);

    let key = SigningKey::parse(&p8::p8().0).unwrap();
    let real = ApnsClient::new(key, KEY_ID, TEAM_ID, TOPIC, None).unwrap();
    let mut sandbox = pushkit();
    sandbox.sandbox = true;
    assert_eq!(real.host(&sandbox), "https://api.sandbox.push.apple.com");
    assert_eq!(real.topic(&sandbox), "net.veydan.mobile.voip");
    assert_eq!(real.topic(&Target::of(token())), TOPIC);
}

#[tokio::test]
async fn a_voip_token_that_is_not_hex_never_reaches_apple() {
    let server = MockServer::start().await;
    answers(&server, ResponseTemplate::new(200)).await;
    let (client, _) = client(&server);
    let target = Target {
        voip: true,
        ..Target::of("../../other")
    };
    let a = client.send(&target, &ring()).await;
    assert_eq!((a.outcome, a.code.as_deref()), (Outcome::Rejected, Some("NOT_A_TOKEN")));
    assert!(sent(&server).await.is_empty());
}
