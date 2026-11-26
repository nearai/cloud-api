//! Intel model reports using the `intel_quote` and `event_log` fields.
use cc_eventlog::{EventLogVersion, RuntimeEvent, DSTACK_RUNTIME_EVENT_TYPE};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{AttestationVerificationError as Error, AttestationVerifier, Rtmr3Event, VerifiedCpu};

pub(super) async fn verify(
    verifier: &AttestationVerifier,
    report: &Map<String, Value>,
) -> Result<VerifiedCpu, Error> {
    let quote = report
        .get("intel_quote")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::MissingField("intel_quote".into()))?;
    let verified = verifier.verify_tdx_quote(quote).await?;
    AttestationVerifier::tdx_claims(verified, |td| {
        let log = report
            .get("event_log")
            .ok_or_else(|| Error::MissingField("event_log".into()))?;
        AttestationVerifier::verify_rtmr3(&td.rt_mr3, &runtime_events(log)?)
    })
}

#[derive(Deserialize)]
struct EventLogEntry {
    imr: u32,
    digest: String,
    #[serde(default)]
    event_type: u32,
    #[serde(default)]
    event: String,
    #[serde(default)]
    event_payload: String,
}

fn runtime_events(log: &Value) -> Result<Vec<Rtmr3Event>, Error> {
    let entries: Vec<EventLogEntry> = match log {
        Value::String(json) => serde_json::from_str(json),
        value => Vec::deserialize(value),
    }
    .map_err(|e| Error::InvalidFormat(format!("invalid event_log: {e}")))?;

    let mut events = Vec::new();
    for entry in entries.into_iter().filter(|entry| entry.imr == 3) {
        if entry.event_type != DSTACK_RUNTIME_EVENT_TYPE {
            return Err(Error::InvalidFormat("non-runtime event in RTMR3".into()));
        }
        let payload = hex::decode(&entry.event_payload)
            .map_err(|e| Error::InvalidFormat(format!("invalid runtime event payload: {e}")))?;
        let event = RuntimeEvent::new(entry.event, payload, EventLogVersion::V1);
        let digest = event.sha384_digest();
        if !entry.digest.is_empty() {
            let stored = hex::decode(entry.digest)
                .map_err(|e| Error::InvalidFormat(format!("invalid runtime event digest: {e}")))?;
            if stored.as_slice() != digest {
                return Err(Error::RtmrMismatch(
                    "runtime event digest does not match its payload".into(),
                ));
            }
        }
        events.push(Rtmr3Event {
            digest,
            name: event.event,
            payload_hex: entry.event_payload,
        });
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_both_log_forms_and_only_includes_runtime_events_in_rtmr3() {
        let mut entry = json!({"imr":3, "event_type":DSTACK_RUNTIME_EVENT_TYPE,
            "event":"system-ready", "event_payload":"", "digest":""});
        let other_register = json!({"imr":2, "event_type":DSTACK_RUNTIME_EVENT_TYPE,
            "event":placement::consts::KEY_EVENT, "event_payload":"01", "digest":""});
        let log = json!([other_register, entry]);
        for value in [log.clone(), Value::String(log.to_string())] {
            let events = runtime_events(&value).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].name, "system-ready");
        }

        // SHA-384 of the V1 system-ready event with an empty payload.
        entry["digest"] = json!("1a76b2a80a0be71eae59f80945d876351a7a3fb8e9fd1ff1cede5734aa84ea11fd72b4edfbb6f04e5a85edd114c751bd");
        assert!(runtime_events(&json!([entry])).is_ok());
        entry["event_payload"] = json!("ff");
        assert!(matches!(
            runtime_events(&json!([entry])),
            Err(Error::RtmrMismatch(_))
        ));
    }

    #[test]
    fn rejects_non_runtime_rtmr3_entries_even_alongside_valid_events() {
        let boot = json!({"imr":3,"event_type":DSTACK_RUNTIME_EVENT_TYPE,"event":"os-image-hash","event_payload":"ab","digest":""});
        let mut non_runtime = boot.clone();
        non_runtime["digest"] = json!(hex::encode(
            runtime_events(&json!([boot])).unwrap()[0].digest
        ));
        non_runtime["event_type"] = json!(0);
        assert!(matches!(
            runtime_events(&json!([non_runtime, boot])),
            Err(Error::InvalidFormat(_))
        ));
    }
}
