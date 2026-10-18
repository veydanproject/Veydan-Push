//! The gateway (`spec/protocol.md`, "The gateway"): tickets for the tokens
//! of the devices registered here, `/v1/tickets`, and the wake of a ticket
//! by a private push server, `/v1/gateway/wake`.
//!
//! A ticket is asked for, listed and removed by the owner of the device,
//! signed as every request of `/v1/devices` is. A wake is signed by nobody:
//! the ticket is the right to wake. What the gateway learns of a wake is the
//! ticket, and so the device and its owner, the address and the seal key of
//! the private server that sends it, the size, the time, and whether it is
//! urgent; who wrote, in which chat, and what the sealed push says, it never
//! learns.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine;
use vpush_proto::{
    ErrorCode, Payload, PushType, TicketAsk, TicketIssued, TicketList, TicketView, Wake,
    WakeAnswer, SEALED_BUDGET,
};

use super::devices::{client, device_id_ok, is_hex64, signed, Refusal, Signed};
use super::{
    now, Api, RequestId, DEVICE_URGENT_WAKES_PER_HOUR, DEVICE_URGENT_WAKES_PER_MINUTE,
    DEVICE_WAKES_PER_HOUR, DEVICE_WAKES_PER_MINUTE, PUTS_PER_MINUTE,
    WAKES_PER_MINUTE, WAKE_REQUESTS_PER_MINUTE,
};
use crate::delivery::{Message, Outcome, ProviderKind, Target};
use crate::limit::Over;
use crate::store::{StoreError, TicketGone, TicketInput, TicketRef};

/// How long the push service keeps a sealed push for a phone that is off.
/// What is inside is not known here: a day, as for a message.
const TTL: Duration = Duration::from_secs(24 * 3600);

/// What the database keeps of a ticket: the SHA-256 of its hex, hex.
pub fn ticket_hash(ticket: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, ticket.as_bytes());
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 random bytes, hex.
fn new_ticket() -> Result<String, ring::error::Unspecified> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn ticket_unknown() -> Refusal {
    Refusal::new(
        StatusCode::NOT_FOUND,
        ErrorCode::TicketUnknown,
        "no such ticket: expired, deleted, or never issued",
    )
}

/// The event is used up once the request has shown it is worth a place in
/// the memory shared by all.
fn remembered(api: &Api, signed: &Signed) -> Result<(), Refusal> {
    api.auth.remember(signed.event).map_err(Refusal::from)
}

/// `POST /v1/tickets`: a ticket for the token of a device of the signer.
pub async fn issue(
    State(api): State<Api>,
    Extension(RequestId(rid)): Extension<RequestId>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let done = async {
        let signed = signed(&api, &method, &uri, &headers, None, body)?;
        // A ticket costs a row, as a registration does, and is counted with
        // the registrations of the owner.
        api.limits.registration(&signed.pubkey).map_err(|wait| {
            api.counters.refused_rate_owner.add();
            Refusal::too_often(
                format!("{PUTS_PER_MINUTE} registrations and tickets of one owner a minute; come back later"),
                wait,
            )
        })?;
        let ask: TicketAsk = serde_json::from_slice(&signed.body)
            .map_err(|e| Refusal::bad(format!("the body is not a request for a ticket: {e}")))?;
        if !device_id_ok(&ask.device_id) {
            return Err(Refusal::bad(
                "the device id is 8 to 64 letters, digits, `_` or `-`",
            ));
        }
        let ticket = new_ticket().map_err(|_| Refusal::internal("no randomness for a ticket", &rid))?;
        let issued_at = now();
        let tickets = api.config.tickets;
        let input = TicketInput {
            hash: ticket_hash(&ticket),
            prefix: ticket[..8].to_string(),
            issued_at,
            expires_at: issued_at + u64::from(tickets.days) * 86_400,
        };
        match api
            .store
            .issue_ticket(&signed.pubkey, &ask.device_id, &input, tickets.per_device)
            .await
        {
            Ok(()) => {}
            Err(StoreError::NoDevice) => {
                return Err(Refusal::new(
                    StatusCode::CONFLICT,
                    ErrorCode::NoDevice,
                    "no such device of this owner with a live token here; register it first",
                ))
            }
            Err(e) => return Err(Refusal::internal(e, &rid)),
        }
        remembered(&api, &signed)?;
        // The ticket's first 8 hex, as the device names it; nothing of the
        // owner or the device: the gateway does not tie the two in its log.
        tracing::info!(request_id = %rid, ticket = %input.prefix, "ticket issued");
        Ok::<_, Refusal>(TicketIssued {
            ticket,
            expires_at: input.expires_at,
        })
    }
    .await;
    match done {
        Ok(issued) => (StatusCode::CREATED, Json(issued)).into_response(),
        Err(refusal) => refusal.answer(&rid),
    }
}

/// `GET /v1/tickets`: the live tickets of the signer's devices.
pub async fn list(
    State(api): State<Api>,
    Extension(RequestId(rid)): Extension<RequestId>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let done = async {
        let signed = signed(&api, &method, &uri, &headers, None, body)?;
        let tickets = api
            .store
            .tickets_of(&signed.pubkey, now())
            .await
            .map_err(|e| Refusal::internal(e, &rid))?;
        // A list of nothing tells nothing of anybody, and takes no place.
        if !tickets.is_empty() {
            remembered(&api, &signed)?;
        }
        Ok::<_, Refusal>(TicketList {
            tickets: tickets
                .into_iter()
                .map(|t| TicketView {
                    ticket: format!("{}…", t.prefix),
                    device_id: t.device_id,
                    issued_at: t.issued_at,
                    expires_at: t.expires_at,
                    last_wake_at: t.last_wake_at,
                })
                .collect(),
        })
    }
    .await;
    match done {
        Ok(list) => Json(list).into_response(),
        Err(refusal) => refusal.answer(&rid),
    }
}

/// `DELETE /v1/tickets/{ticket}`: the whole ticket, or its first 8 hex.
pub async fn delete(
    State(api): State<Api>,
    Extension(RequestId(rid)): Extension<RequestId>,
    Path(ticket): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let done = async {
        let which = if is_hex64(&ticket) {
            TicketRef::Hash(ticket_hash(&ticket))
        } else if ticket.len() == 8
            && ticket
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            TicketRef::Prefix(ticket.clone())
        } else {
            return Err(Refusal::bad(
                "a ticket is named by its 64 hex characters, or by its first 8",
            ));
        };
        let signed = signed(&api, &method, &uri, &headers, None, body)?;
        match api
            .store
            .delete_ticket(&signed.pubkey, &which)
            .await
            .map_err(|e| Refusal::internal(e, &rid))?
        {
            TicketGone::Removed => {}
            // A ticket of another key is one this key does not have.
            TicketGone::NotFound => return Err(ticket_unknown()),
            TicketGone::Ambiguous => {
                return Err(Refusal::bad(
                    "more than one ticket begins so; name the whole ticket",
                ))
            }
        }
        remembered(&api, &signed)?;
        tracing::info!(request_id = %rid, ticket = %&ticket[..8], "ticket deleted");
        Ok::<_, Refusal>(())
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => refusal.answer(&rid),
    }
}

/// `POST /v1/gateway/wake`: a private push server hands a sealed push to
/// the device of a ticket.
///
/// The push service is tried once, while the private server waits: what it
/// said is the answer, and the private server tries again by its own policy
/// when it is worth it. A dead token takes the tickets of its device with
/// it, and is told as a ticket that is not known.
pub async fn wake(
    State(api): State<Api>,
    Extension(RequestId(rid)): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let done = async {
        // Before the ticket costs a hash and a question to the database:
        // whoever tries tickets at random is told to come back later.
        if let Some(from) = client(&api, &headers) {
            api.limits.wake_request(from).map_err(|wait| {
                api.counters.refused_rate_ip.add();
                Refusal::too_often(
                    format!("{WAKE_REQUESTS_PER_MINUTE} wakes from one address a minute; come back later"),
                    wait,
                )
            })?;
        }
        let body = body.map_err(|e| {
            let too_large = e.status() == StatusCode::PAYLOAD_TOO_LARGE;
            Refusal::new(
                e.status(),
                if too_large {
                    ErrorCode::PayloadTooLarge
                } else {
                    ErrorCode::BadRequest
                },
                "the body cannot be read",
            )
        })?;
        let wake: Wake = serde_json::from_slice(&body)
            .map_err(|_| Refusal::bad("the body is not a wake: { ticket, blob, sender, urgent? }"))?;
        if !is_hex64(&wake.ticket) {
            return Err(Refusal::bad("a ticket is 64 hex characters"));
        }
        if !is_hex64(&wake.sender) {
            return Err(Refusal::bad("the sender is 64 hex characters"));
        }
        if wake.blob.is_empty()
            || base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&wake.blob)
                .is_err()
        {
            return Err(Refusal::bad("the blob is base64url without padding"));
        }

        // The ticket, the size, the pace: in this order, as the protocol
        // says them.
        let hash = ticket_hash(&wake.ticket);
        let prefix = &wake.ticket[..8];
        let at = now();
        let Some(device) = api
            .store
            .ticket_device(&hash, at)
            .await
            .map_err(|e| Refusal::internal(e, &rid))?
        else {
            return Err(ticket_unknown());
        };
        if wake.blob.len() > SEALED_BUDGET {
            return Err(Refusal::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::PayloadTooLarge,
                format!("the blob is above {SEALED_BUDGET} characters"),
            ));
        }
        api.limits.wake(&hash).map_err(|wait| {
            Refusal::too_often(
                format!("{WAKES_PER_MINUTE} wakes of one ticket a minute; come back later"),
                wait,
            )
        })?;
        // A device holds several tickets, each perhaps handed to a server
        // that means it harm: together they wake it no more often than this.
        // An urgent wake (a call) has a budget of its own, so a flood of
        // ordinary ones never silences a call.
        api.limits.device_wake(&device.token, wake.urgent).map_err(|over| {
            let (what, per_minute, per_hour) = if wake.urgent {
                ("urgent wakes", DEVICE_URGENT_WAKES_PER_MINUTE, DEVICE_URGENT_WAKES_PER_HOUR)
            } else {
                ("wakes", DEVICE_WAKES_PER_MINUTE, DEVICE_WAKES_PER_HOUR)
            };
            let message = match over {
                Over::Short(_) => format!(
                    "{per_minute} {what} of one device a minute, by all its tickets; come back later"
                ),
                Over::Long(_) => format!(
                    "{per_hour} {what} of one device an hour, by all its tickets; come back later"
                ),
            };
            Refusal::too_often(message, over.wait())
        })?;
        api.store
            .ticket_woken(&hash, at)
            .await
            .map_err(|e| Refusal::internal(e, &rid))?;

        let provider = device
            .provider
            .parse::<ProviderKind>()
            .and_then(|kind| api.providers.get(&device.app_id, kind))
            .map_err(|e| Refusal::internal(e, &rid))?;
        let mut payload = Payload::new(PushType::Sealed);
        payload.blob = Some(wake.blob);
        payload.sender = Some(wake.sender);
        payload.trace = Some(rid.clone());
        let size = payload.blob.as_ref().map_or(0, String::len);
        let message = Message {
            payload,
            fallback: None,
            // A key to replace a waiting push by would say which pushes are
            // of one chat; the gateway does not know, and does not guess.
            collapse_key: None,
            ttl: TTL,
            // High priority only when the private server asks for it, which
            // it does for a call: a push service that is handed urgent
            // pushes with nothing to show soon takes them all for noise.
            urgent: wake.urgent,
        };
        let target = Target::of(device.token.clone());
        let attempt = provider.send(&target, &message).await;
        let outcome = super::devices::outcome_name(attempt.outcome);
        // The trace, the ticket's first 8 hex and the size; of the device,
        // nothing: the log does not tie a ticket to its owner.
        tracing::info!(
            request_id = %rid,
            trace = %rid,
            ticket = %prefix,
            bytes = size,
            outcome,
            http_status = attempt.http_status,
            code = attempt.code.as_deref(),
            "wake"
        );
        api.store
            .record_outcome(
                &device.pubkey,
                &device.device_id,
                &device.token,
                outcome,
                now(),
            )
            .await
            .map_err(|e| Refusal::internal(e, &rid))?;
        match attempt.outcome {
            Outcome::Delivered => Ok(WakeAnswer { trace: rid.clone() }),
            // The tickets of the device went with its token.
            Outcome::DeadToken => Err(ticket_unknown()),
            Outcome::Retry => {
                let refusal = Refusal::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::ProviderUnavailable,
                    "the push service is busy or out of reach; the wake may be sent again",
                );
                Err(Refusal {
                    retry_after: attempt.retry_after_ms.map(Duration::from_millis),
                    ..refusal
                })
            }
            Outcome::Rejected => Err(Refusal::new(
                StatusCode::BAD_GATEWAY,
                ErrorCode::ProviderRejected,
                "the push service refused the wake",
            )),
        }
    }
    .await;
    match done {
        Ok(answer) => (StatusCode::ACCEPTED, Json(answer)).into_response(),
        Err(refusal) => refusal.answer(&rid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_is_32_random_bytes_and_is_kept_as_its_hash() {
        let (a, b) = (new_ticket().unwrap(), new_ticket().unwrap());
        assert!(is_hex64(&a) && is_hex64(&b));
        assert_ne!(a, b);
        let hash = ticket_hash(&a);
        assert!(is_hex64(&hash));
        assert_ne!(hash, a);
        assert_eq!(hash, ticket_hash(&a));
    }
}
