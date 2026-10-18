//! Through the gateway: the delivery of a private push server
//! (`spec/protocol.md`, "The gateway").
//!
//! A private push server holds no key of FCM. Its devices register with a
//! ticket the gateway issued for their token, and with a push key of their
//! own; every push is sealed to that key (NIP-44 v2, from the key of this
//! server) and handed to the gateway, which wakes the ticket. The gateway
//! sees the ticket, this server's address and key, the size, the time and
//! whether the push is a call's (`urgent`), and nothing else of what is
//! inside.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use nostr::key::{Keys, PublicKey, SecretKey};
use nostr::nips::nip44;
use nostr::nips::nip44::v2::ConversationKey;
use ring::rand::{SecureRandom, SystemRandom};
use vpush_proto::{ErrorBody, GatewayInfo, Payload, PushType, Wake, SEALED_BUDGET};

use super::{
    http, Attempt, CheckFuture, Message, Outcome, ProviderKind, PushProvider, SendFuture, Target,
    TokenCheck,
};

/// The key a private push server seals its pushes with. Its public half is
/// in `/v1/info` as `gateway.sender`, so a device can tell who sealed a push.
pub struct SealKey {
    keys: Keys,
}

impl SealKey {
    /// Reads the key: 64 hex characters, and nothing else but blanks.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        // What is wrong is said without the text: it may be most of a key.
        let secret = SecretKey::from_hex(text.trim())
            .map_err(|_| format!("{}: not a key (64 hex characters)", path.display()))?;
        Ok(Self {
            keys: Keys::new(secret),
        })
    }

    /// Reads the key, or makes one when there is none yet: the first start
    /// of a private server. The file is the server's alone (0600), and is
    /// written whole or not at all.
    pub fn read_or_make(path: &Path) -> Result<Self, String> {
        if path.exists() {
            return Self::read(path);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let keys = Keys::generate();
        let fresh = path.with_extension("key.new");
        let written = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&fresh)?;
            writeln!(file, "{}", keys.secret_key().to_secret_hex())?;
            file.sync_all()?;
            std::fs::rename(&fresh, path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&fresh);
            return Err(format!("cannot write {}: {e}", path.display()));
        }
        tracing::info!(file = %path.display(), sender = %keys.public_key().to_hex(), "a key to seal pushes with was made");
        Ok(Self { keys })
    }

    pub fn from_keys(keys: Keys) -> Self {
        Self { keys }
    }

    /// The public half, 64 hex characters: `sender` of every sealed push.
    pub fn sender(&self) -> String {
        self.keys.public_key().to_hex()
    }

    /// The push, sealed to the push key of a device: its `data` map, the one
    /// FCM would have got, as JSON, encrypted with NIP-44 v2, base64url
    /// without padding.
    pub fn seal(&self, to: &PublicKey, data: &BTreeMap<String, String>) -> Result<String, String> {
        let json = serde_json::to_string(data).map_err(|e| e.to_string())?;
        let mut nonce = [0u8; 32];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "no randomness for a nonce".to_string())?;
        let bytes = nip44::encrypt_to_bytes_with_nonce(
            self.keys.secret_key(),
            to,
            json,
            nip44::Nonce::V2(nonce),
        )
        .map_err(|e| e.to_string())?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }
}

/// What a device does with a sealed push: opens `blob` with its push key and
/// the `sender`, and finds the JSON of the push inside. Here for the tests,
/// and as the description of it in code.
pub fn open(push_key: &SecretKey, sender: &PublicKey, blob: &str) -> Result<String, String> {
    let bytes = URL_SAFE_NO_PAD.decode(blob).map_err(|e| e.to_string())?;
    let key = ConversationKey::derive(push_key, sender).map_err(|e| e.to_string())?;
    let plain = nip44::v2::decrypt_to_bytes(&key, &bytes).map_err(|e| e.to_string())?;
    String::from_utf8(plain).map_err(|e| e.to_string())
}

/// The gateway, as a push service of this server.
pub struct GatewayClient {
    http: reqwest::Client,
    /// The gateway, without a trailing slash.
    url: String,
    key: Arc<SealKey>,
}

impl GatewayClient {
    pub fn new(url: &str, key: Arc<SealKey>) -> Result<Self, String> {
        Ok(Self {
            http: http::client(Duration::from_secs(5), Duration::from_secs(15))?,
            url: url.trim_end_matches('/').to_string(),
            key,
        })
    }

    /// What `/v1/info` says of the gateway of this server.
    pub fn info(&self) -> GatewayInfo {
        GatewayInfo {
            url: self.url.clone(),
            sender: self.key.sender(),
        }
    }

    /// The push sealed for the device, in the form that fits: with the
    /// event when it does, by its id and relay when it does not.
    fn sealed(&self, target: &Target, message: &Message) -> Result<String, Attempt> {
        let Some(to) = target.seal_to.as_deref() else {
            return Err(Attempt::new(Outcome::Rejected)
                .code("NO_PUSH_KEY")
                .detail("the device registered no push key"));
        };
        let to = PublicKey::from_hex(to).map_err(|_| {
            Attempt::new(Outcome::Rejected)
                .code("NO_PUSH_KEY")
                .detail("the push key of the device is not a key")
        })?;
        let seal = |payload: &Payload| {
            self.key
                .seal(&to, &payload.to_data())
                .map_err(|e| Attempt::new(Outcome::Rejected).code("SEAL").detail(e))
        };
        let mut blob = seal(&message.payload)?;
        if blob.len() > SEALED_BUDGET {
            if let Some(fallback) = &message.fallback {
                tracing::debug!(
                    trace = message.payload.trace.as_deref().unwrap_or(""),
                    bytes = blob.len(),
                    "a sealed push with the event does not fit; sealed without it"
                );
                blob = seal(fallback)?;
            }
        }
        if blob.len() > SEALED_BUDGET {
            return Err(Attempt::new(Outcome::Rejected)
                .code("TOO_BIG")
                .detail(format!("sealed, the push is {} bytes", blob.len())));
        }
        Ok(blob)
    }

    /// One `POST /v1/gateway/wake`, and what the gateway made of it.
    async fn wake(&self, ticket: &str, blob: String, urgent: bool) -> Attempt {
        let body = Wake {
            ticket: ticket.to_string(),
            blob,
            sender: self.key.sender(),
            urgent,
        };
        let response = match self
            .http
            .post(format!("{}/v1/gateway/wake", self.url))
            .json(&body)
            .send()
            .await
        {
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
        classify(status, &text).retry_after(asked)
    }
}

/// Whether the gateway is asked to wake the phone at high priority: for the
/// push of a call alone (a ring or its end, `call` inside), which rings now
/// or never. Every other push goes at normal priority: the gateway learns
/// that a wake is urgent, and nothing more of what is inside.
fn urgent(message: &Message) -> bool {
    message.urgent && message.payload.call
}

/// What an answer of the gateway means for us.
pub fn classify(status: u16, body: &str) -> Attempt {
    if (200..300).contains(&status) {
        return Attempt::new(Outcome::Delivered).status(status);
    }
    // The gateway's own word, as its errors say it: `ticket_unknown`.
    let said = serde_json::from_value::<ErrorBody>(
        serde_json::from_str::<serde_json::Value>(body).unwrap_or_default(),
    )
    .ok();
    let code = said
        .as_ref()
        .and_then(|b| serde_json::to_value(b.error.code).ok())
        .and_then(|v| v.as_str().map(str::to_string));
    let detail = said
        .map(|b| b.error.message)
        .unwrap_or_else(|| body.to_string());
    let outcome = match (status, code.as_deref()) {
        // Expired, deleted, or never issued: no wake will ever reach it.
        // A bare 404 is not enough: a wrong address of the gateway answers
        // 404 too, and would cost every device its registration.
        (404, Some("ticket_unknown")) => Outcome::DeadToken,
        (408 | 429, _) | (500..=599, _) => Outcome::Retry,
        _ => Outcome::Rejected,
    };
    let attempt = Attempt::new(outcome).status(status).detail(detail);
    match code {
        Some(code) => attempt.code(code),
        None => attempt,
    }
}

impl PushProvider for GatewayClient {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gateway
    }

    fn send<'a>(&'a self, target: &'a Target, message: &'a Message) -> SendFuture<'a> {
        Box::pin(async move {
            let started = Instant::now();
            let mut attempt = match self.sealed(target, message) {
                Ok(blob) => self.wake(&target.token, blob, urgent(message)).await,
                Err(attempt) => attempt,
            };
            attempt.latency_ms = started.elapsed().as_millis() as u64;
            attempt
        })
    }

    /// The gateway is asked about a ticket the only way it can be: a `test`
    /// push, sealed and woken. A ticket it does not know is refused.
    fn check<'a>(&'a self, target: &'a Target) -> CheckFuture<'a> {
        Box::pin(async move {
            let mut payload = Payload::new(PushType::Test);
            payload.trace = Some(crate::api::next_request_id());
            // The device shows nothing for it: it only asks whether the ticket is one.
            payload.check = true;
            let message = Message {
                payload,
                fallback: None,
                collapse_key: None,
                ttl: super::TEST_TTL,
                urgent: true,
            };
            let attempt = self.send(target, &message).await;
            match attempt.outcome {
                Outcome::Delivered => TokenCheck::Valid,
                Outcome::DeadToken => TokenCheck::Invalid,
                _ => TokenCheck::Unknown,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(code: &str) -> String {
        format!(r#"{{"error":{{"code":"{code}","message":"why"}},"request_id":"5f3a9c1e"}}"#)
    }

    #[test]
    fn a_ticket_the_gateway_does_not_know_is_a_dead_token_and_a_bare_404_is_not() {
        let a = classify(404, &error("ticket_unknown"));
        assert_eq!(
            (a.outcome, a.code.as_deref()),
            (Outcome::DeadToken, Some("ticket_unknown"))
        );
        assert_eq!(
            classify(404, &error("not_found")).outcome,
            Outcome::Rejected
        );
        assert_eq!(
            classify(404, "<html>no such site</html>").outcome,
            Outcome::Rejected
        );
    }

    #[test]
    fn a_busy_gateway_is_tried_again_and_a_refusal_is_not() {
        assert_eq!(
            classify(202, r#"{"trace":"5f3a9c1e"}"#).outcome,
            Outcome::Delivered
        );
        for status in [429, 503, 500, 502, 408] {
            assert_eq!(
                classify(status, &error("rate_limited")).outcome,
                Outcome::Retry,
                "{status}"
            );
        }
        for status in [400, 413, 422] {
            assert_eq!(
                classify(status, &error("payload_too_large")).outcome,
                Outcome::Rejected,
                "{status}"
            );
        }
    }

    #[test]
    fn only_the_push_of_a_call_is_urgent_at_the_gateway() {
        let message = |kind: PushType, call: bool| {
            let mut payload = Payload::new(kind);
            payload.call = call;
            Message {
                payload,
                fallback: None,
                collapse_key: None,
                ttl: Duration::from_secs(60),
                urgent: true,
            }
        };
        assert!(urgent(&message(PushType::Dm, true)));
        for (kind, call) in [
            (PushType::Dm, false),
            (PushType::Group, false),
            (PushType::Sync, false),
            (PushType::Test, false),
        ] {
            assert!(!urgent(&message(kind, call)), "{kind:?}");
        }
        let mut quiet = message(PushType::Dm, true);
        quiet.urgent = false;
        assert!(!urgent(&quiet), "what is not urgent at FCM is not at the gateway");
    }

    #[test]
    fn a_sealed_push_opens_with_the_push_key_and_the_sender() {
        let server = SealKey::from_keys(Keys::generate());
        let device = Keys::generate();
        let mut payload = Payload::new(PushType::Test);
        payload.trace = Some("abcd1234".into());
        let blob = server
            .seal(&device.public_key(), &payload.to_data())
            .unwrap();
        assert!(
            !blob.contains('=') && !blob.contains('+') && !blob.contains('/'),
            "base64url without padding"
        );
        let sender = PublicKey::from_hex(&server.sender()).unwrap();
        let inside = open(device.secret_key(), &sender, &blob).unwrap();
        assert_eq!(inside, r#"{"trace":"abcd1234","type":"test","v":"2"}"#);
        // Another key opens nothing.
        assert!(open(Keys::generate().secret_key(), &sender, &blob).is_err());
    }

    #[test]
    fn a_key_is_made_once_and_read_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("vpush-seal-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("data/gateway.key");
        let made = SealKey::read_or_make(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            SealKey::read_or_make(&path).unwrap().sender(),
            made.sender()
        );
        assert_eq!(SealKey::read(&path).unwrap().sender(), made.sender());

        std::fs::write(&path, "not a key, but secret-looking text\n").unwrap();
        let e = SealKey::read(&path).err().unwrap();
        assert!(
            e.contains("not a key") && !e.contains("secret-looking"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
