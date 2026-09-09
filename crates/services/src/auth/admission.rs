// INTEGRATION: register `pub mod admission;` in crates/services/src/auth/mod.rs.
//! Audience-bound Commons proofs. These credentials confer no Cloud management authority.
//! Verifiers must pin issuer, audience, algorithm and type, check time, compare the
//! signed device key and nonce with their challenge, and consume that challenge once.

use crate::auth::UserId;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use url::Url;
use uuid::Uuid;

pub const ADMISSION_MAX_AGE_SECONDS: i64 = 120;
pub const ADMISSION_TOKEN_TYPE: &str = "tc-admission+jwt";
const MAX_AUDIENCES: usize = 16;
const MAX_RETAINED_KEYS: usize = 8;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("Invalid admission configuration")]
    InvalidConfiguration,
    #[error("Unsupported admission audience")]
    InvalidAudience,
    #[error("Invalid admission nonce")]
    InvalidNonce,
    #[error("Invalid admission device public key")]
    InvalidDevicePublicKey,
    #[error("User is not eligible for admission")]
    IneligibleUser,
    #[error("Invalid admission issuance time")]
    InvalidTimestamp,
    #[error("Admission assertion could not be signed")]
    SigningFailure,
}

/// Provision both secrets independently. Preserve `subject_key` across signing-key
/// rotations to preserve contributor identity. This type deliberately has no Debug.
pub struct AdmissionIssuerConfig {
    issuer: String,
    audiences: BTreeSet<String>,
    subject_key: [u8; 32],
    signing_seed: [u8; 32],
    retained_public_keys: Vec<[u8; 32]>,
}

impl AdmissionIssuerConfig {
    pub fn new(
        issuer: String,
        audiences: Vec<String>,
        subject_key: [u8; 32],
        signing_seed: [u8; 32],
        retained_public_keys: Vec<[u8; 32]>,
    ) -> Result<Self, AdmissionError> {
        let parsed = Url::parse(&issuer).map_err(|_| AdmissionError::InvalidConfiguration)?;
        if issuer.len() > 2048
            || !issuer.is_ascii()
            || issuer
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            || parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || (parsed.as_str() != issuer && parsed.as_str() != format!("{issuer}/"))
            || audiences.is_empty()
            || audiences.len() > MAX_AUDIENCES
            || audiences.iter().any(|value| !valid_audience(value))
            || retained_public_keys.len() > MAX_RETAINED_KEYS
            || subject_key == [0; 32]
            || signing_seed == [0; 32]
            || subject_key == signing_seed
        {
            return Err(AdmissionError::InvalidConfiguration);
        }
        let mut seen = BTreeSet::from([*SigningKey::from_bytes(&signing_seed)
            .verifying_key()
            .as_bytes()]);
        for key in &retained_public_keys {
            validate_public_key(key).map_err(|_| AdmissionError::InvalidConfiguration)?;
            if !seen.insert(*key) {
                return Err(AdmissionError::InvalidConfiguration);
            }
        }
        Ok(Self {
            issuer,
            audiences: audiences.into_iter().collect(),
            subject_key,
            signing_seed,
            retained_public_keys,
        })
    }
}

/// Minimal identity projected from an authenticated, live database user.
#[derive(Clone)]
pub struct AdmissionIdentity<'a> {
    pub user_id: UserId,
    pub auth_provider: &'a str,
    pub is_active: bool,
}

#[derive(Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRequest {
    #[schema(min_length = 1, max_length = 256)]
    pub audience: String,
    /// Canonical unpadded base64url encoding of the verifier's 32-byte challenge.
    #[schema(min_length = 43, max_length = 43)]
    pub nonce: String,
    /// Canonical unpadded base64url encoding of the contributor's Ed25519 key.
    #[schema(min_length = 43, max_length = 43)]
    pub device_public_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
    pub auth_provider: String,
    pub nonce: String,
    pub device_public_key: String,
}

/// Do not log this response: the assertion is a short-lived enrollment credential.
#[derive(Serialize, utoipa::ToSchema)]
pub struct AdmissionAssertion {
    pub assertion: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct AdmissionJwk {
    pub kty: String,
    pub crv: String,
    pub alg: String,
    #[serde(rename = "use")]
    pub key_use: String,
    pub kid: String,
    pub x: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct AdmissionJwks {
    pub keys: Vec<AdmissionJwk>,
}

pub struct AdmissionIssuer {
    issuer: String,
    audiences: BTreeSet<String>,
    subject_key: [u8; 32],
    signing_key: SigningKey,
    kid: String,
    jwks: AdmissionJwks,
}

impl AdmissionIssuer {
    pub fn new(config: AdmissionIssuerConfig) -> Self {
        let signing_key = SigningKey::from_bytes(&config.signing_seed);
        let active_key = public_jwk(signing_key.verifying_key().as_bytes());
        let kid = active_key.kid.clone();
        let mut keys = vec![active_key];
        for public_key in config.retained_public_keys {
            keys.push(public_jwk(&public_key));
        }
        Self {
            issuer: config.issuer,
            audiences: config.audiences,
            subject_key: config.subject_key,
            signing_key,
            kid,
            jwks: AdmissionJwks { keys },
        }
    }

    pub fn jwks(&self) -> &AdmissionJwks {
        &self.jwks
    }

    /// `now` is trusted server time in Unix seconds, never a client request value.
    /// Caller must first authenticate the live session, including revocation checks.
    pub fn issue(
        &self,
        user: &AdmissionIdentity<'_>,
        request: &AdmissionRequest,
        now: i64,
    ) -> Result<AdmissionAssertion, AdmissionError> {
        if !user.is_active || !matches!(user.auth_provider, "near" | "github" | "google") {
            return Err(AdmissionError::IneligibleUser);
        }
        if !valid_audience(&request.audience) || !self.audiences.contains(&request.audience) {
            return Err(AdmissionError::InvalidAudience);
        }
        decode_32(&request.nonce).ok_or(AdmissionError::InvalidNonce)?;
        let device_key =
            decode_32(&request.device_public_key).ok_or(AdmissionError::InvalidDevicePublicKey)?;
        validate_public_key(&device_key)?;
        let expires_at = now
            .checked_add(ADMISSION_MAX_AGE_SECONDS)
            .filter(|_| now >= 0)
            .ok_or(AdmissionError::InvalidTimestamp)?;
        let claims = AdmissionClaims {
            iss: self.issuer.clone(),
            aud: request.audience.clone(),
            sub: self.subject(&user.user_id, &request.audience)?,
            iat: now,
            exp: expires_at,
            jti: Uuid::new_v4().to_string(),
            auth_provider: user.auth_provider.to_owned(),
            nonce: request.nonce.clone(),
            device_public_key: request.device_public_key.clone(),
        };
        let header =
            serde_json::json!({"alg": "EdDSA", "typ": ADMISSION_TOKEN_TYPE, "kid": self.kid});
        let input = format!("{}.{}", encode_json(&header)?, encode_json(&claims)?);
        let signature = self.signing_key.sign(input.as_bytes());
        Ok(AdmissionAssertion {
            assertion: format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())),
            expires_at,
        })
    }

    fn subject(&self, user_id: &UserId, audience: &str) -> Result<String, AdmissionError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.subject_key)
            .map_err(|_| AdmissionError::SigningFailure)?;
        mac.update(b"trace-commons-admission-subject-v1\0");
        mac.update(&(audience.len() as u64).to_be_bytes());
        mac.update(audience.as_bytes());
        mac.update(user_id.0.as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
    }
}

fn valid_audience(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn decode_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 43 {
        return None;
    }
    let decoded: [u8; 32] = URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()?;
    (URL_SAFE_NO_PAD.encode(decoded) == value).then_some(decoded)
}

fn validate_public_key(bytes: &[u8; 32]) -> Result<(), AdmissionError> {
    let key =
        VerifyingKey::from_bytes(bytes).map_err(|_| AdmissionError::InvalidDevicePublicKey)?;
    if key.is_weak() {
        return Err(AdmissionError::InvalidDevicePublicKey);
    }
    Ok(())
}

fn public_jwk(bytes: &[u8; 32]) -> AdmissionJwk {
    AdmissionJwk {
        kty: "OKP".into(),
        crv: "Ed25519".into(),
        alg: "EdDSA".into(),
        key_use: "sig".into(),
        kid: URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)),
        x: URL_SAFE_NO_PAD.encode(bytes),
    }
}

fn encode_json(value: &impl Serialize) -> Result<String, AdmissionError> {
    serde_json::to_vec(value)
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .map_err(|_| AdmissionError::SigningFailure)
}

#[cfg(test)]
mod tests {
    use crate::auth::admission::*;
    use crate::auth::UserId;
    use ed25519_dalek::Signature;

    fn issuer(seed: u8, retained: Vec<[u8; 32]>) -> AdmissionIssuer {
        AdmissionIssuer::new(
            AdmissionIssuerConfig::new(
                "https://cloud.example".into(),
                vec!["commons".into(), "other".into()],
                [3; 32],
                [seed; 32],
                retained,
            )
            .unwrap(),
        )
    }

    fn user() -> AdmissionIdentity<'static> {
        AdmissionIdentity {
            user_id: UserId(Uuid::new_v4()),
            auth_provider: "near",
            is_active: true,
        }
    }

    fn request() -> AdmissionRequest {
        AdmissionRequest {
            audience: "commons".into(),
            nonce: URL_SAFE_NO_PAD.encode([7; 32]),
            device_public_key: URL_SAFE_NO_PAD
                .encode(SigningKey::from_bytes(&[9; 32]).verifying_key().as_bytes()),
        }
    }

    fn claims(token: &str) -> AdmissionClaims {
        serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap()
    }

    fn verify(token: &str, jwk: &AdmissionJwk) -> bool {
        let (input, signature) = token.rsplit_once('.').unwrap();
        let key = VerifyingKey::from_bytes(&decode_32(&jwk.x).unwrap()).unwrap();
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
        key.verify_strict(input.as_bytes(), &signature).is_ok()
    }

    #[test]
    fn publishes_key_for_valid_bounded_assertion_without_private_identity() {
        let issuer = issuer(1, vec![]);
        let request = request();
        let identity = user();
        let assertion = issuer.issue(&identity, &request, 1000).unwrap();
        assert!(verify(&assertion.assertion, &issuer.jwks().keys[0]));
        let claims = claims(&assertion.assertion);
        assert_eq!(
            (claims.iat, claims.exp, assertion.expires_at),
            (1000, 1120, 1120)
        );
        assert_eq!(claims.iss, "https://cloud.example");
        assert_eq!(claims.aud, request.audience);
        assert_eq!(claims.nonce, request.nonce);
        assert_eq!(claims.device_public_key, request.device_public_key);
        assert_eq!(claims.auth_provider, "near");
        assert!(Uuid::parse_str(&claims.jti).is_ok());
        let parts: Vec<_> = assertion.assertion.split('.').collect();
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["typ"], ADMISSION_TOKEN_TYPE);
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["kid"], issuer.jwks().keys[0].kid);
        let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert!(!payload.contains(&identity.user_id.to_string()));
        let jwks = serde_json::to_value(issuer.jwks()).unwrap();
        assert!(jwks["keys"][0].get("d").is_none());
    }

    #[test]
    fn tampered_claims_or_wrong_key_do_not_verify() {
        let issuer = issuer(1, vec![]);
        let token = issuer.issue(&user(), &request(), 1000).unwrap().assertion;
        let mut parts: Vec<_> = token.split('.').map(str::to_owned).collect();
        let mut payload = claims(&token);
        payload.aud = "other".into();
        parts[1] = encode_json(&payload).unwrap();
        assert!(!verify(&parts.join("."), &issuer.jwks().keys[0]));
        assert!(!verify(
            &token,
            &public_jwk(SigningKey::from_bytes(&[2; 32]).verifying_key().as_bytes())
        ));
    }

    #[test]
    fn subject_survives_rotation_and_challenge_changes_but_separates_users_and_audiences() {
        let first = issuer(1, vec![]);
        let next = issuer(2, vec![*first.signing_key.verifying_key().as_bytes()]);
        let user = user();
        let mut req = request();
        let old = first.issue(&user, &req, 1000).unwrap().assertion;
        let original = claims(&old);
        req.nonce = URL_SAFE_NO_PAD.encode([8; 32]);
        req.device_public_key =
            URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[10; 32]).verifying_key().as_bytes());
        let rotated = next.issue(&user, &req, 1001).unwrap().assertion;
        assert_eq!(original.sub, claims(&rotated).sub);
        assert_ne!(original.jti, claims(&rotated).jti);
        assert!(verify(&old, &next.jwks().keys[1]));
        assert!(verify(&rotated, &next.jwks().keys[0]));
        req.audience = "other".into();
        assert_ne!(
            original.sub,
            claims(&next.issue(&user, &req, 1000).unwrap().assertion).sub
        );
        req.audience = "commons".into();
        let mut other = user.clone();
        other.user_id = UserId(Uuid::new_v4());
        assert_ne!(
            original.sub,
            claims(&next.issue(&other, &req, 1000).unwrap().assertion).sub
        );
    }

    #[test]
    fn rejects_invalid_requests_inactive_users_and_unsupported_providers() {
        let issuer = issuer(1, vec![]);
        let user = user();
        for bad in ["", "unknown", "commons ", &"x".repeat(257)] {
            let mut req = request();
            req.audience = bad.into();
            assert!(matches!(
                issuer.issue(&user, &req, 1000),
                Err(AdmissionError::InvalidAudience)
            ));
        }
        for bad in [
            "",
            "bad",
            &"x".repeat(4096),
            &format!("{}=", request().nonce),
            &"/".repeat(43),
        ] {
            let mut req = request();
            req.nonce = bad.into();
            assert!(matches!(
                issuer.issue(&user, &req, 1000),
                Err(AdmissionError::InvalidNonce)
            ));
            req = request();
            req.device_public_key = bad.into();
            assert!(matches!(
                issuer.issue(&user, &req, 1000),
                Err(AdmissionError::InvalidDevicePublicKey)
            ));
        }
        let mut req = request();
        req.device_public_key = URL_SAFE_NO_PAD.encode([0; 32]);
        assert!(matches!(
            issuer.issue(&user, &req, 1000),
            Err(AdmissionError::InvalidDevicePublicKey)
        ));
        let mut inactive = user.clone();
        inactive.is_active = false;
        assert!(matches!(
            issuer.issue(&inactive, &request(), 1000),
            Err(AdmissionError::IneligibleUser)
        ));
        inactive = user.clone();
        inactive.auth_provider = "mock";
        assert!(matches!(
            issuer.issue(&inactive, &request(), 1000),
            Err(AdmissionError::IneligibleUser)
        ));
        for now in [-1, i64::MAX] {
            assert!(matches!(
                issuer.issue(&user, &request(), now),
                Err(AdmissionError::InvalidTimestamp)
            ));
        }
        let mut json = serde_json::to_value(request()).unwrap();
        json["extra"] = true.into();
        assert!(serde_json::from_value::<AdmissionRequest>(json).is_err());
    }

    #[test]
    fn issues_for_each_supported_provider() {
        let issuer = issuer(1, vec![]);
        for provider in ["near", "github", "google"] {
            let mut identity = user();
            identity.auth_provider = provider;
            let assertion = issuer.issue(&identity, &request(), 1000).unwrap();
            assert!(verify(&assertion.assertion, &issuer.jwks().keys[0]));
            assert_eq!(claims(&assertion.assertion).auth_provider, provider);
        }
    }

    #[test]
    fn subject_v1_matches_persisted_identity_vector() {
        let identity = UserId(Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap());
        // Independent Python hashlib/hmac vector: key [3;32], v1 domain with
        // NUL terminator, big-endian u64 audience length, audience, UUID bytes.
        assert_eq!(
            issuer(1, vec![]).subject(&identity, "commons").unwrap(),
            "g739eTtkdmpYKu9wBODvh6HpiviJ6z_0Eo4lJRaXfHE"
        );
    }

    #[test]
    fn rejects_invalid_configuration_and_duplicate_rotation_keys() {
        for issuer_url in [
            "http://cloud.example",
            "https://u:p@cloud.example",
            "https://cloud.example?x=1",
            "https://cloud.example/#x",
            " https://cloud.example",
            "https://cloud.example\\alias",
            "invalid",
        ] {
            assert!(AdmissionIssuerConfig::new(
                issuer_url.into(),
                vec!["commons".into()],
                [3; 32],
                [1; 32],
                vec![]
            )
            .is_err());
        }
        for audiences in [
            vec![],
            vec!["".into()],
            vec!["bad audience".into()],
            vec!["x".into(); 17],
        ] {
            assert!(AdmissionIssuerConfig::new(
                "https://cloud.example".into(),
                audiences,
                [3; 32],
                [1; 32],
                vec![]
            )
            .is_err());
        }
        let active = *SigningKey::from_bytes(&[1; 32]).verifying_key().as_bytes();
        let retained = *SigningKey::from_bytes(&[2; 32]).verifying_key().as_bytes();
        for (subject, seed, keys) in [
            ([0; 32], [1; 32], vec![]),
            ([3; 32], [0; 32], vec![]),
            ([1; 32], [1; 32], vec![]),
            ([3; 32], [1; 32], vec![[0; 32]]),
            ([3; 32], [1; 32], vec![active]),
            ([3; 32], [1; 32], vec![retained, retained]),
            ([3; 32], [1; 32], vec![retained; 9]),
        ] {
            assert!(AdmissionIssuerConfig::new(
                "https://cloud.example".into(),
                vec!["commons".into()],
                subject,
                seed,
                keys
            )
            .is_err());
        }
    }
}
