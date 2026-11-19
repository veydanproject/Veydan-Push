//! Apple Push Notification service: HTTP/2, with a provider token.
//!
//! A push with something to show is an alert, marked `mutable-content`: the
//! Notification Service Extension of the app opens the event and writes the
//! text, as the app does on Android. The push says only `push.new`, a key
//! of the app's own strings, which is what shows when the extension cannot.
//! A push with nothing to show goes as a background push, which iOS shows
//! nothing for.
//!
//! There is no `apns-collapse-id`. On iOS a push with one takes the place
//! of the notification on the screen, and every direct message is one chat
//! to this server: each would wipe out the one before it. The chat goes
//! into `thread-id` instead, which only groups the notifications.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use vpush_proto::Payload;

use super::{
    http, Attempt, CheckFuture, Message, Outcome, ProviderKind, PushProvider, SendFuture, Target,
    TokenCheck,
};
use crate::config::ApnsConfig;

pub const PRODUCTION: &str = "https://api.push.apple.com";
pub const SANDBOX: &str = "https://api.sandbox.push.apple.com";

/// The most a push may weigh: APNs counts the whole body, every byte of the
/// JSON, and refuses one above this. (A VoIP push may have 5120; none is
/// sent.)
pub const BODY_LIMIT: usize = 4096;

/// What an alert push says by itself: a key of the app's own strings, shown
/// when the extension of the app could not write the text.
pub const LOC_KEY: &str = "push.new";

/// A provider token is made anew this long after the one before it. Apple
/// refuses a token older than an hour, and answers
/// `TooManyProviderTokenUpdates` to one made more often than every 20
/// minutes.
const TOKEN_RENEW: Duration = Duration::from_secs(50 * 60);

/// A token APNs refused as expired or invalid is made anew, and the push
/// tried once more; not again within this time, whatever APNs says, so a
/// server whose clock is off does not make a token for every push.
const TOKEN_FORCED_GAP: Duration = Duration::from_secs(20 * 60);

/// The `.p8` key of a developer account: ES256, a P-256 key in PKCS#8 PEM.
pub struct SigningKey {
    key: EcdsaKeyPair,
}

impl SigningKey {
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(pem: &str) -> Result<Self, String> {
        const EXPECTED: &str = "the .p8 key of APNs is expected: ES256, a P-256 key in PKCS#8 PEM";
        // PKCS#8 says `PRIVATE KEY` alone; SEC1 says `EC PRIVATE KEY`.
        if !pem.contains("BEGIN PRIVATE KEY") {
            return Err(format!("not a PKCS#8 PEM private key; {EXPECTED}"));
        }
        let body: String = pem
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = STANDARD
            .decode(body)
            .map_err(|e| format!("the PEM is not base64 ({e}); {EXPECTED}"))?;
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &SystemRandom::new())
                .map_err(|e| format!("not a P-256 key ({e}); {EXPECTED}"))?;
        Ok(Self { key })
    }

    /// The signature as a JWT carries it: `r` and `s`, 32 bytes each.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        self.key
            .sign(&SystemRandom::new(), message)
            .map(|s| s.as_ref().to_vec())
            .map_err(|e| format!("cannot sign: {e}"))
    }

    #[cfg(test)]
    fn public_key(&self) -> Vec<u8> {
        use ring::signature::KeyPair;
        self.key.public_key().as_ref().to_vec()
    }
}

/// The provider token: a JWT of ES256 that names the key and the team, made
/// at `iat` (Unix seconds).
pub fn provider_token(
    key: &SigningKey,
    key_id: &str,
    team_id: &str,
    iat: u64,
) -> Result<String, String> {
    let header = json!({ "alg": "ES256", "kid": key_id });
    let claims = json!({ "iss": team_id, "iat": iat });
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature = key.sign(signed.as_bytes())?;
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

struct Made {
    value: String,
    at: Instant,
}

/// The provider token in use, and when one was last made for a refusal.
#[derive(Default)]
struct Tokens {
    current: Option<Made>,
    forced_at: Option<Instant>,
}

impl Tokens {
    /// The token to push with at `now`, while it is young enough.
    fn usable(&self, now: Instant) -> Option<&str> {
        self.current
            .as_ref()
            .filter(|t| now.saturating_duration_since(t.at) < TOKEN_RENEW)
            .map(|t| t.value.as_str())
    }

    /// Whether a token may be made now in place of the refused one.
    fn may_force(&self, now: Instant) -> bool {
        self.forced_at
            .is_none_or(|at| now.saturating_duration_since(at) >= TOKEN_FORCED_GAP)
    }
}

/// `apns-push-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushKind {
    /// Shown, in the words the extension of the app writes.
    Alert,
    /// Wakes the app, shows nothing.
    Background,
}

impl PushKind {
    /// What the push is: a background push when there is nothing to show
    /// for it, an alert for everything else.
    pub fn of(message: &Message, payload: &Payload) -> Self {
        if message.background || payload.is_silent() || payload.check {
            Self::Background
        } else {
            Self::Alert
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::Background => "background",
        }
    }

    /// `apns-priority`: 10 sends now, 5 when convenient for the phone. A
    /// background push must be 5: APNs refuses it at 10.
    pub fn priority(self, urgent: bool) -> u8 {
        match self {
            Self::Alert if urgent => 10,
            _ => 5,
        }
    }
}

/// The body of a push: the keys of the push as FCM carries them, every
/// value a string, and `aps` beside them.
pub fn body(message: &Message, payload: &Payload) -> Value {
    let mut root: serde_json::Map<String, Value> = payload
        .to_data()
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect();
    let aps = match PushKind::of(message, payload) {
        PushKind::Alert => {
            // The sound of the device: without the key iOS shows the
            // banner in silence, whatever the user allowed.
            let mut aps = json!({
                "mutable-content": 1,
                "alert": { "loc-key": LOC_KEY },
                "sound": "default",
            });
            if let Some(chat) = &message.collapse_key {
                aps["thread-id"] = json!(chat);
            }
            aps
        }
        PushKind::Background => json!({ "content-available": 1 }),
    };
    root.insert("aps".to_string(), aps);
    Value::Object(root)
}

/// The push as it is sent, within [`BODY_LIMIT`]: with the event when that
/// fits, else in its other form, `event_id` and `relay` (`Message::fallback`).
/// Measured here, before sending: APNs is not asked to refuse it first.
pub fn encode(message: &Message) -> Result<(Vec<u8>, PushKind, bool), Attempt> {
    let first = serde_json::to_vec(&body(message, &message.payload)).unwrap_or_default();
    if first.len() <= BODY_LIMIT {
        return Ok((first, PushKind::of(message, &message.payload), false));
    }
    if let Some(fallback) = &message.fallback {
        let other = serde_json::to_vec(&body(message, fallback)).unwrap_or_default();
        if other.len() <= BODY_LIMIT {
            return Ok((other, PushKind::of(message, fallback), true));
        }
    }
    Err(Attempt::new(Outcome::Rejected)
        .code("TOO_BIG")
        .detail(format!("{} bytes, APNs takes {BODY_LIMIT}", first.len())))
}

/// `apns-expiration`: when APNs stops trying, in Unix seconds. 0 for a push
/// with no time to live: delivered now or never.
pub fn expiration(ttl: Duration, now: u64) -> u64 {
    if ttl.is_zero() {
        0
    } else {
        now + ttl.as_secs()
    }
}

/// What APNs says of a provider token it refuses: made anew, the push is
/// tried once more.
const STALE_TOKEN: [&str; 2] = ["ExpiredProviderToken", "InvalidProviderToken"];

/// What an error answer of APNs means for us.
pub fn classify(status: u16, body: &str) -> Attempt {
    #[derive(Deserialize, Default)]
    struct Answer {
        #[serde(default)]
        reason: String,
    }
    let reason = serde_json::from_str::<Answer>(body)
        .unwrap_or_default()
        .reason;
    let outcome = match (status, reason.as_str()) {
        // The token is gone for good (410 is Apple's word for it), or is
        // none at all. A sandbox token pushed to production is told
        // `BadDeviceToken` too: the device said which it is.
        (410, _) | (_, "Unregistered" | "BadDeviceToken") => Outcome::DeadToken,
        (429 | 500..=599, _) => Outcome::Retry,
        // A key, a topic or a body of ours that APNs refuses: no device is
        // to blame, and none is forgotten. `DeviceTokenNotForTopic` is among
        // them: a wrong topic in the config would say it of every device.
        _ => Outcome::Rejected,
    };
    let attempt = Attempt::new(outcome).status(status);
    let attempt = if body.is_empty() {
        attempt
    } else {
        attempt.detail(body)
    };
    if reason.is_empty() {
        attempt
    } else {
        attempt.code(reason)
    }
}

pub struct ApnsClient {
    http: reqwest::Client,
    production: String,
    sandbox: String,
    topic: String,
    key_id: String,
    team_id: String,
    key: SigningKey,
    /// Held while a token is made, so many pushes make one token.
    tokens: Mutex<Tokens>,
}

impl ApnsClient {
    pub fn from_config(config: &ApnsConfig) -> Result<Self, String> {
        let key = SigningKey::read(&config.key_file)?;
        Self::new(
            key,
            &config.key_id,
            &config.team_id,
            &config.topic,
            config.endpoint.as_deref(),
        )
    }

    /// `endpoint` in place of both hosts of APNs, in tests.
    pub fn new(
        key: SigningKey,
        key_id: &str,
        team_id: &str,
        topic: &str,
        endpoint: Option<&str>,
    ) -> Result<Self, String> {
        let host = |default: &str| {
            endpoint
                .unwrap_or(default)
                .trim_end_matches('/')
                .to_string()
        };
        Ok(Self {
            http: http::h2_client(Duration::from_secs(5), Duration::from_secs(15))?,
            production: host(PRODUCTION),
            sandbox: host(SANDBOX),
            topic: topic.to_string(),
            key_id: key_id.to_string(),
            team_id: team_id.to_string(),
            key,
            tokens: Mutex::new(Tokens::default()),
        })
    }

    /// The host of the device's environment.
    pub fn host(&self, target: &Target) -> &str {
        if target.sandbox {
            &self.sandbox
        } else {
            &self.production
        }
    }

    fn make_token(&self) -> Result<Made, Attempt> {
        let iat = unix_now();
        let value = provider_token(&self.key, &self.key_id, &self.team_id, iat).map_err(|e| {
            Attempt::new(Outcome::Rejected)
                .code("PROVIDER_AUTH")
                .detail(e)
        })?;
        tracing::debug!(team = %self.team_id, key = %self.key_id, iat, "apns provider token made");
        Ok(Made {
            value,
            at: Instant::now(),
        })
    }

    async fn token(&self) -> Result<String, Attempt> {
        let mut tokens = self.tokens.lock().await;
        if let Some(value) = tokens.usable(Instant::now()) {
            return Ok(value.to_string());
        }
        let made = self.make_token()?;
        let value = made.value.clone();
        tokens.current = Some(made);
        Ok(value)
    }

    /// A token in place of the refused one: the one another push has made
    /// meanwhile, or a new one. `None` when one was made for a refusal a
    /// moment ago: making more will not help.
    async fn renew(&self, refused: &str) -> Option<String> {
        let mut tokens = self.tokens.lock().await;
        let now = Instant::now();
        if let Some(current) = &tokens.current {
            if current.value != refused {
                return Some(current.value.clone());
            }
        }
        if !tokens.may_force(now) {
            return None;
        }
        let made = self.make_token().ok()?;
        let value = made.value.clone();
        tokens.current = Some(made);
        tokens.forced_at = Some(now);
        Some(value)
    }

    /// One request to APNs, and what it made of it.
    async fn post(
        &self,
        target: &Target,
        message: &Message,
        body: &[u8],
        kind: PushKind,
        token: &str,
    ) -> Attempt {
        let url = format!("{}/3/device/{}", self.host(target), target.token);
        let response = self
            .http
            .post(url)
            .header(reqwest::header::AUTHORIZATION, format!("bearer {token}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("apns-topic", &self.topic)
            .header("apns-push-type", kind.as_str())
            .header("apns-priority", kind.priority(message.urgent).to_string())
            .header(
                "apns-expiration",
                expiration(message.ttl, unix_now()).to_string(),
            )
            .body(body.to_vec())
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return Attempt::new(Outcome::Retry)
                    .code("UNREACHABLE")
                    .detail(e.to_string())
            }
        };
        let status = response.status().as_u16();
        let asked = http::retry_after(response.headers());
        let text = response.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            return Attempt::new(Outcome::Delivered).status(status);
        }
        classify(status, &text).retry_after(asked)
    }

    async fn attempt(&self, target: &Target, message: &Message) -> Attempt {
        // The token goes into the path of the request: hex, as APNs gives
        // it, and nothing else.
        if target.token.is_empty() || !target.token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Attempt::new(Outcome::Rejected)
                .code("NOT_A_TOKEN")
                .detail("a token of APNs is hex characters");
        }
        let (body, kind, by_id) = match encode(message) {
            Ok(encoded) => encoded,
            Err(attempt) => return attempt,
        };
        if by_id {
            tracing::info!(
                trace = message.payload.trace.as_deref().unwrap_or(""),
                token = %target.masked(),
                bytes = body.len(),
                "the push with the event is over 4096 bytes for APNs; sent without it"
            );
        }
        let token = match self.token().await {
            Ok(token) => token,
            Err(attempt) => return attempt,
        };
        let attempt = self.post(target, message, &body, kind, &token).await;
        let stale = attempt.http_status == Some(403)
            && attempt
                .code
                .as_deref()
                .is_some_and(|c| STALE_TOKEN.contains(&c));
        if !stale {
            return attempt;
        }
        match self.renew(&token).await {
            Some(fresh) => {
                tracing::info!(
                    code = attempt.code.as_deref(),
                    "apns refused the provider token; sent again with a new one"
                );
                self.post(target, message, &body, kind, &fresh).await
            }
            None => attempt,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PushProvider for ApnsClient {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Apns
    }

    fn send<'a>(&'a self, target: &'a Target, message: &'a Message) -> SendFuture<'a> {
        Box::pin(async move {
            let started = Instant::now();
            let mut attempt = self.attempt(target, message).await;
            attempt.latency_ms = started.elapsed().as_millis() as u64;
            attempt
        })
    }

    /// APNs has no question that sends nothing: whether a token is one is
    /// told by the first push to it.
    fn check<'a>(&'a self, _: &'a Target) -> CheckFuture<'a> {
        Box::pin(std::future::ready(TokenCheck::Unknown))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};
    use vpush_proto::PushType;

    /// A P-256 key made for the test, as a `.p8` file holds it.
    fn p8() -> String {
        let der =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .unwrap();
        pem(der.as_ref())
    }

    /// Text in PEM armour. The label is put in by `format!`, so that no
    /// line of this file looks like the head of a private key to a scanner.
    fn armoured(label: &str, body: &str) -> String {
        format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
    }

    fn pem(der: &[u8]) -> String {
        let b64 = STANDARD.encode(der);
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        armoured("PRIVATE KEY", &lines.join("\n"))
    }

    fn message(payload: Payload) -> Message {
        Message {
            payload,
            fallback: None,
            collapse_key: Some("3fa1c2d4".into()),
            ttl: Duration::from_secs(3600),
            urgent: true,
            background: false,
        }
    }

    fn dm(event_len: usize) -> Message {
        let mut payload = Payload::new(PushType::Dm);
        payload.trace = Some("abcd1234".into());
        let mut by_id = payload.clone();
        by_id.event_id = Some("e1".repeat(32));
        by_id.relay = Some("wss://node-1.veydan.net".into());
        // Quotes, which JSON escapes: the body weighs more than the data.
        payload.event = Some(format!(r#"{{"content":"{}"}}"#, "\"".repeat(event_len)));
        let mut m = message(payload);
        m.fallback = Some(by_id);
        m
    }

    #[test]
    fn the_provider_token_names_the_key_and_the_team_and_verifies() {
        let key = SigningKey::parse(&p8()).unwrap();
        let jwt = provider_token(&key, "ABC123DEFG", "TEAM123456", 1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(header, json!({ "alg": "ES256", "kid": "ABC123DEFG" }));
        assert_eq!(
            claims,
            json!({ "iss": "TEAM123456", "iat": 1_700_000_000u64 })
        );
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(signature.len(), 64, "r and s, as a JWT carries them");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_key())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn a_key_that_is_not_es256_is_refused_with_a_reason() {
        let ed = ring::signature::Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let e = SigningKey::parse(&pem(ed.as_ref())).err().unwrap();
        assert!(e.contains("not a P-256 key") && e.contains(".p8"), "{e}");

        // SEC1, as `openssl ecparam -genkey` writes it: not what Apple gives.
        let e = SigningKey::parse(&armoured("EC PRIVATE KEY", "AAAA"))
            .err()
            .unwrap();
        assert!(e.contains("not a PKCS#8 PEM"), "{e}");

        let e = SigningKey::parse(&armoured("PRIVATE KEY", "!!!"))
            .err()
            .unwrap();
        assert!(e.contains("not base64"), "{e}");
    }

    #[test]
    fn a_token_is_used_for_50_minutes_and_forced_anew_once_in_20() {
        let start = Instant::now();
        let mut tokens = Tokens::default();
        assert_eq!(tokens.usable(start), None);
        tokens.current = Some(Made {
            value: "t1".into(),
            at: start,
        });
        let minutes = |m: u64| start + Duration::from_secs(m * 60);
        assert_eq!(tokens.usable(minutes(49)), Some("t1"));
        assert_eq!(
            tokens.usable(minutes(50)),
            None,
            "renewed before Apple's hour"
        );
        assert!(
            TOKEN_RENEW >= Duration::from_secs(20 * 60) && TOKEN_RENEW < Duration::from_secs(3600)
        );

        assert!(tokens.may_force(start));
        tokens.forced_at = Some(start);
        assert!(!tokens.may_force(minutes(19)));
        assert!(tokens.may_force(minutes(20)));
    }

    #[test]
    fn a_message_is_an_alert_with_the_chat_as_its_thread_and_no_collapse_id() {
        let m = dm(10);
        let body = body(&m, &m.payload);
        assert_eq!(body["aps"]["mutable-content"], 1);
        assert_eq!(body["aps"]["alert"]["loc-key"], LOC_KEY);
        assert_eq!(body["aps"]["sound"], "default");
        assert_eq!(body["aps"]["thread-id"], "3fa1c2d4");
        assert!(body["aps"].get("content-available").is_none());
        assert!(body.get("collapse_key").is_none() && body["aps"].get("collapse-id").is_none());
        // The keys of the push as FCM carries them, every value a string.
        assert_eq!(body["v"], "2");
        assert_eq!(body["type"], "dm");
        assert_eq!(body["trace"], "abcd1234");
        assert!(body["event"].is_string());
        assert_eq!(PushKind::of(&m, &m.payload).priority(m.urgent), 10);
    }

    #[test]
    fn what_shows_nothing_is_a_background_push_at_priority_5() {
        let manifest = message(Payload::new(PushType::ManifestUpdate));
        let mut check = Payload::new(PushType::Test);
        check.check = true;
        let check = message(check);
        let mut keeper = dm(10);
        keeper.background = true;
        for m in [&manifest, &check, &keeper] {
            let kind = PushKind::of(m, &m.payload);
            assert_eq!(kind, PushKind::Background, "{:?}", m.payload.kind);
            assert_eq!(
                kind.priority(true),
                5,
                "APNs refuses a background push at 10"
            );
            let body = body(m, &m.payload);
            assert_eq!(body["aps"], json!({ "content-available": 1 }));
        }
        for kind in [
            PushType::Dm,
            PushType::Group,
            PushType::Sync,
            PushType::Test,
            PushType::Sealed,
        ] {
            let m = message(Payload::new(kind));
            assert_eq!(PushKind::of(&m, &m.payload), PushKind::Alert, "{kind:?}");
        }
        let mut later = message(Payload::new(PushType::Dm));
        later.urgent = false;
        assert_eq!(
            PushKind::of(&later, &later.payload).priority(later.urgent),
            5
        );
    }

    #[test]
    fn a_push_over_4096_goes_without_the_event_and_is_measured_as_sent() {
        // Fits: sent with the event.
        let (bytes, _, by_id) = encode(&dm(100)).unwrap();
        assert!(!by_id);
        assert!(String::from_utf8(bytes).unwrap().contains("content"));

        // Within the data budget of FCM, over the body of APNs once the
        // quotes are escaped: sent by its id.
        let m = dm(2500);
        assert!(m.payload.fits(), "FCM would take it");
        assert!(serde_json::to_vec(&body(&m, &m.payload)).unwrap().len() > BODY_LIMIT);
        let (bytes, kind, by_id) = encode(&m).unwrap();
        assert!(by_id);
        assert!(bytes.len() <= BODY_LIMIT);
        assert_eq!(kind, PushKind::Alert);
        let sent: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(sent.get("event").is_none());
        assert_eq!(sent["relay"], "wss://node-1.veydan.net");

        // Exactly at the limit is taken.
        let mut edge = message(Payload::new(PushType::Test));
        let base = serde_json::to_vec(&body(&edge, &edge.payload))
            .unwrap()
            .len();
        edge.payload.trace = Some("x".repeat(BODY_LIMIT - base - r#","trace":"""#.len()));
        let (bytes, _, _) = encode(&edge).unwrap();
        assert_eq!(bytes.len(), BODY_LIMIT);

        // Nothing to fall back to: refused here, never sent.
        let mut big = message(Payload::new(PushType::Sealed));
        big.payload.blob = Some("a".repeat(BODY_LIMIT));
        let a = encode(&big).err().unwrap();
        assert_eq!(
            (a.outcome, a.code.as_deref()),
            (Outcome::Rejected, Some("TOO_BIG"))
        );
    }

    #[test]
    fn a_push_with_no_time_to_live_expires_at_once() {
        assert_eq!(expiration(Duration::ZERO, 1_700_000_000), 0);
        assert_eq!(
            expiration(Duration::from_secs(60), 1_700_000_000),
            1_700_000_060
        );
    }

    fn apple(reason: &str) -> String {
        json!({ "reason": reason }).to_string()
    }

    #[test]
    fn a_gone_or_bad_token_is_dead() {
        let a = classify(
            410,
            &json!({ "reason": "Unregistered", "timestamp": 1 }).to_string(),
        );
        assert_eq!(
            (a.outcome, a.code.as_deref()),
            (Outcome::DeadToken, Some("Unregistered"))
        );
        assert_eq!(
            classify(410, &apple("ExpiredToken")).outcome,
            Outcome::DeadToken
        );
        assert_eq!(classify(410, "").outcome, Outcome::DeadToken);
        assert_eq!(
            classify(400, &apple("BadDeviceToken")).outcome,
            Outcome::DeadToken
        );
    }

    #[test]
    fn what_is_wrong_on_our_side_kills_no_token() {
        for (status, reason) in [
            (400, "DeviceTokenNotForTopic"),
            (400, "BadTopic"),
            (400, "PayloadEmpty"),
            (400, "BadExpirationDate"),
            (403, "InvalidProviderToken"),
            (403, "ExpiredProviderToken"),
            (403, "BadCertificateEnvironment"),
            (404, "BadPath"),
            (413, "PayloadTooLarge"),
        ] {
            let a = classify(status, &apple(reason));
            assert_eq!(a.outcome, Outcome::Rejected, "{status} {reason}");
            assert_eq!(a.code.as_deref(), Some(reason));
        }
        assert_eq!(classify(404, "<html>no</html>").outcome, Outcome::Rejected);
    }

    #[test]
    fn a_busy_service_is_tried_again() {
        assert_eq!(
            classify(429, &apple("TooManyRequests")).outcome,
            Outcome::Retry
        );
        assert_eq!(
            classify(429, &apple("TooManyProviderTokenUpdates")).outcome,
            Outcome::Retry
        );
        assert_eq!(
            classify(500, &apple("InternalServerError")).outcome,
            Outcome::Retry
        );
        assert_eq!(
            classify(503, &apple("ServiceUnavailable")).outcome,
            Outcome::Retry
        );
        assert_eq!(classify(502, "").outcome, Outcome::Retry);
    }
}
