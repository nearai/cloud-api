//! Organization-scoped prompt-cache namespace (`cache_salt`).
//!
//! Engines that support `cache_salt` only share cached prefixes between
//! requests with the same salt. A client-chosen salt is not a tenant boundary:
//! two organizations can send the same value, and a request without one shares
//! the engine's default namespace with every other unsalted request. The
//! completion service therefore derives the upstream salt from the
//! authenticated organization and the optional client salt, and providers
//! forward only that derived value (`ChatCompletionParams::apply_upstream_cache_salt`).
//!
//! Privacy: the client salt and the derived value are never logged.

use inference_providers::{ChatCompletionParams, UpstreamCacheSalt, CACHE_SALT_FIELD};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Largest client `cache_salt` accepted, in bytes.
pub const MAX_CLIENT_CACHE_SALT_BYTES: usize = 256;

/// Domain tag for the derivation; a new derivation gets a new version.
const DERIVATION_DOMAIN: &[u8] = b"nearai-cloud-api/cache-salt/v1\0";

/// Why a client `cache_salt` was rejected. Messages never echo the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSaltError {
    NotAString,
    TooLong,
}

impl CacheSaltError {
    pub fn message(self) -> String {
        match self {
            Self::NotAString => format!("{CACHE_SALT_FIELD} must be a string"),
            Self::TooLong => {
                format!("{CACHE_SALT_FIELD} must be at most {MAX_CLIENT_CACHE_SALT_BYTES} bytes")
            }
        }
    }
}

/// `hex(SHA-256(domain ‖ org_id[16] ‖ tag ‖ [len_be64 ‖ client_salt]))`.
///
/// The organization id is a fixed 16 bytes and the client salt is tagged
/// (absent vs present) and length-prefixed, so no client input can produce the
/// preimage of another organization's namespace. Only the organization and
/// the client salt are inputs, so every workspace, key and request of one
/// organization that sends the same salt shares one namespace.
pub fn derive(organization_id: Uuid, client_salt: Option<&str>) -> UpstreamCacheSalt {
    let mut hasher = Sha256::new();
    hasher.update(DERIVATION_DOMAIN);
    hasher.update(organization_id.as_bytes());
    match client_salt {
        None => hasher.update([0u8]),
        Some(salt) => {
            hasher.update([1u8]);
            hasher.update((salt.len() as u64).to_be_bytes());
            hasher.update(salt.as_bytes());
        }
    }
    UpstreamCacheSalt::new(hex::encode(hasher.finalize()))
}

/// Scope `params` to the namespace of `organization_id`:
///
/// - `upstream_cache_salt` always gets the derived value; the NEAR fleet
///   writes it into every request body.
/// - A client `cache_salt` is removed from the original request sidecar and
///   replaced in the body with the derived value, so other providers receive
///   the field exactly when the client sent one, never the client's value.
///
/// A JSON `null` counts as absent; a non-string or a value over
/// [`MAX_CLIENT_CACHE_SALT_BYTES`] is rejected.
pub fn scope_to_organization(
    params: &mut ChatCompletionParams,
    organization_id: Uuid,
) -> Result<(), CacheSaltError> {
    let client = params.take_client_cache_salt();
    let client_salt = match client.as_ref() {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(salt)) => {
            if salt.len() > MAX_CLIENT_CACHE_SALT_BYTES {
                return Err(CacheSaltError::TooLong);
            }
            Some(salt.as_str())
        }
        Some(_) => return Err(CacheSaltError::NotAString),
    };
    let salt = derive(organization_id, client_salt);
    if client_salt.is_some() {
        params.extra.insert(
            CACHE_SALT_FIELD.to_string(),
            serde_json::Value::String(salt.as_str().to_string()),
        );
    }
    params.upstream_cache_salt = Some(salt);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ORG_A: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_000a);
    const ORG_B: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_000b);

    fn params(extra: serde_json::Value) -> ChatCompletionParams {
        let mut body = json!({
            "model": "synthetic-model",
            "messages": [{"role": "user", "content": "synthetic input"}],
        });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                body.insert(key.clone(), value.clone());
            }
        }
        serde_json::from_value(body).unwrap()
    }

    fn scoped(org: Uuid, extra: serde_json::Value) -> UpstreamCacheSalt {
        let mut params = params(extra);
        scope_to_organization(&mut params, org).unwrap();
        params.upstream_cache_salt.unwrap()
    }

    #[test]
    fn same_organization_gets_a_stable_namespace() {
        assert_eq!(scoped(ORG_A, json!({})), scoped(ORG_A, json!({})));
        assert_eq!(
            scoped(ORG_A, json!({"cache_salt": "team-1"})),
            scoped(ORG_A, json!({"cache_salt": "team-1"}))
        );
        assert_eq!(
            scoped(ORG_A, json!({"cache_salt": null})),
            scoped(ORG_A, json!({}))
        );
        // The client salt still partitions within the organization.
        assert_ne!(
            scoped(ORG_A, json!({"cache_salt": "team-1"})),
            scoped(ORG_A, json!({"cache_salt": "team-2"}))
        );
        assert_ne!(
            scoped(ORG_A, json!({"cache_salt": ""})),
            scoped(ORG_A, json!({}))
        );
    }

    #[test]
    fn different_organizations_are_separated_even_with_the_same_client_salt() {
        assert_ne!(scoped(ORG_A, json!({})), scoped(ORG_B, json!({})));
        assert_ne!(
            scoped(ORG_A, json!({"cache_salt": "shared"})),
            scoped(ORG_B, json!({"cache_salt": "shared"}))
        );
    }

    #[test]
    fn client_input_cannot_select_another_organizations_namespace() {
        let victim_default = derive(ORG_B, None);
        let victim_salted = derive(ORG_B, Some("shared"));
        for forged in [
            victim_default.as_str().to_string(),
            victim_salted.as_str().to_string(),
            "shared".to_string(),
        ] {
            let mut request = params(json!({"cache_salt": forged}));
            request.original_request = Some(json!({"cache_salt": forged, "temperature": 0.5}));
            scope_to_organization(&mut request, ORG_A).unwrap();

            let salt = request.upstream_cache_salt.clone().unwrap();
            assert_ne!(salt, victim_default);
            assert_ne!(salt, victim_salted);
            assert_eq!(salt, derive(ORG_A, Some(forged.as_str())));
            // The client value is replaced everywhere it could be forwarded:
            // the body carries the derived value, the sidecar none.
            assert_eq!(request.extra[CACHE_SALT_FIELD], salt.as_str());
            assert_ne!(request.extra[CACHE_SALT_FIELD], forged.as_str());
            let original = request.original_request.as_ref().unwrap();
            assert!(original.get(CACHE_SALT_FIELD).is_none());
            assert_eq!(original["temperature"], 0.5);

            // On the wire the body carries the server namespace only.
            request.apply_upstream_cache_salt();
            let body = serde_json::to_value(&request).unwrap();
            assert_eq!(body[CACHE_SALT_FIELD], salt.as_str());
            assert_ne!(body[CACHE_SALT_FIELD], forged.as_str());
        }
    }

    #[test]
    fn body_salt_is_present_only_when_the_client_sent_one() {
        for extra in [json!({}), json!({"cache_salt": null})] {
            let mut request = params(extra);
            scope_to_organization(&mut request, ORG_A).unwrap();
            assert!(!request.extra.contains_key(CACHE_SALT_FIELD));
            assert_eq!(request.upstream_cache_salt, Some(derive(ORG_A, None)));
        }
    }

    #[test]
    fn client_salt_is_bounded_and_must_be_a_string() {
        let max = "s".repeat(MAX_CLIENT_CACHE_SALT_BYTES);
        let _ = scoped(ORG_A, json!({"cache_salt": max}));

        // Bytes, not chars: a multi-byte character over the limit is rejected.
        let over = "é".repeat(MAX_CLIENT_CACHE_SALT_BYTES / 2 + 1);
        for (value, expected) in [
            (json!(over), CacheSaltError::TooLong),
            (
                json!("s".repeat(MAX_CLIENT_CACHE_SALT_BYTES + 1)),
                CacheSaltError::TooLong,
            ),
            (json!(42), CacheSaltError::NotAString),
            (json!(["a"]), CacheSaltError::NotAString),
            (json!({"salt": "a"}), CacheSaltError::NotAString),
        ] {
            let mut request = params(json!({"cache_salt": value}));
            assert_eq!(scope_to_organization(&mut request, ORG_A), Err(expected));
            assert!(request.upstream_cache_salt.is_none());
            assert!(!request.extra.contains_key(CACHE_SALT_FIELD));
        }
    }

    #[test]
    fn namespace_debug_is_redacted() {
        let salt = derive(ORG_A, Some("synthetic-client-salt"));
        assert!(!format!("{salt:?}").contains(salt.as_str()));
    }
}
