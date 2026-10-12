//! Lets users whose login identity is a placeholder address (NEAR wallet logins)
//! add a contact email. The address is stored on the user only after the owner of
//! the mailbox opens a single-use link sent to it. `users.email` stays the login
//! identity and is never changed here.

use crate::auth::{User, UserId, UserRepository};
use crate::email::{
    is_placeholder_email, placeholder_account_id, ContactEmailVerificationEmail,
    EmailDeliveryOutcome, EmailSender,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

/// How long a verification link stays valid.
pub const VERIFICATION_TOKEN_TTL_HOURS: i64 = 24;

/// Send limits, counted from the stored verification rows. The per-address limit
/// applies across all users: wallet accounts are free to create, so a per-user
/// limit alone would let anyone flood a mailbox.
pub const SEND_LIMITS: SendLimits = SendLimits {
    per_user: 3,
    per_user_window_secs: 60 * 60,
    per_address: 3,
    per_address_window_secs: 24 * 60 * 60,
};

/// Rows older than this can neither be confirmed nor count toward a limit.
const VERIFICATION_RETENTION_SECS: i64 = 24 * 60 * 60;

const NEAR_AUTH_PROVIDER: &str = "near";
const TOKEN_BYTES: usize = 32;
/// Generated tokens are 43 characters; anything much longer is not one of ours.
const MAX_TOKEN_LENGTH: usize = 128;
const MAX_EMAIL_LENGTH: usize = 254;
const MAX_EMAIL_LOCAL_PART_LENGTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendLimits {
    pub per_user: i64,
    pub per_user_window_secs: i64,
    pub per_address: i64,
    pub per_address_window_secs: i64,
}

pub struct NewVerification<'a> {
    pub user_id: Uuid,
    pub email: &'a str,
    /// SHA-256 of the token. The token itself is never stored.
    pub token_hash: &'a str,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateVerificationOutcome {
    Created,
    /// The user does not exist or is no longer active.
    UserNotFound,
    RateLimited {
        retry_after_secs: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmOutcome {
    Verified {
        user_id: Uuid,
    },
    /// Unknown, expired or already used token.
    InvalidToken,
    /// The address is another user's login email or verified contact email.
    /// Nothing was written and the token stays usable.
    AddressInUse {
        user_id: Uuid,
    },
}

#[async_trait]
pub trait ContactEmailRepository: Send + Sync {
    /// One transaction: enforce `limits`, expire the user's earlier unused tokens,
    /// store the new one.
    async fn create_verification(
        &self,
        verification: NewVerification<'_>,
        limits: SendLimits,
    ) -> anyhow::Result<CreateVerificationOutcome>;

    /// One transaction: consume the token and set the user's contact email, its
    /// verification time and the opt-in time, or change nothing.
    async fn confirm(&self, token_hash: &str) -> anyhow::Result<ConfirmOutcome>;

    /// Delete verification rows created more than `max_age_secs` ago. Returns
    /// how many.
    async fn delete_older_than(&self, max_age_secs: i64) -> anyhow::Result<u64>;
}

#[derive(Debug, thiserror::Error)]
pub enum ContactEmailError {
    #[error("contact email verification is disabled")]
    Disabled,
    #[error("user not found")]
    UserNotFound,
    #[error("the account already signs in with an email address")]
    NotSupported,
    #[error("{0}")]
    InvalidEmail(&'static str),
    #[error("too many verification emails")]
    RateLimited { retry_after_secs: u64 },
    #[error("invalid or expired verification token")]
    InvalidToken,
    #[error("email address is used by another account")]
    AddressInUse,
    #[error("the verification email could not be sent")]
    SendFailed,
    #[error("contact email operation failed")]
    Internal(#[source] anyhow::Error),
}

/// Whether a stored verification can still be confirmed at `now`: not used yet and
/// not past its expiry.
pub fn verification_is_usable(
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    consumed_at.is_none() && now < expires_at
}

/// SHA-256 of a verification token, hex encoded.
pub fn hash_verification_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// A new verification token: 32 random bytes, URL-safe base64 without padding.
fn generate_verification_token() -> String {
    use base64::Engine;
    use rand_core::{OsRng, RngCore};

    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Trims an address a user typed and checks its shape: `local@domain` in plain
/// ASCII, with a dotted domain. This rejects placeholder addresses, display names
/// and anything with whitespace or control characters. Whether the mailbox exists
/// is settled by the verification email, not here.
pub fn normalize_contact_email(input: &str) -> Result<String, ContactEmailError> {
    let email = input.trim();
    if email.is_empty() {
        return Err(ContactEmailError::InvalidEmail("email is required"));
    }
    if email.len() > MAX_EMAIL_LENGTH {
        return Err(ContactEmailError::InvalidEmail("email is too long"));
    }
    let invalid = ContactEmailError::InvalidEmail("email is not a valid email address");
    let Some((local, domain)) = email.split_once('@') else {
        return Err(invalid);
    };

    let is_local_char = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c);
    let local_ok = !local.is_empty()
        && local.len() <= MAX_EMAIL_LOCAL_PART_LENGTH
        && local.chars().all(is_local_char)
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..");
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    if !local_ok || !domain_ok {
        return Err(invalid);
    }

    Ok(email.to_string())
}

pub struct ContactEmailService {
    repository: Arc<dyn ContactEmailRepository>,
    user_repository: Arc<dyn UserRepository>,
    email_sender: Arc<dyn EmailSender>,
    /// Frontend page the emailed link points to. `None` switches the feature off.
    verification_url: Option<String>,
}

impl ContactEmailService {
    /// `verification_url` is the frontend page that confirms a token; pass `None`
    /// to switch the feature off.
    pub fn new(
        repository: Arc<dyn ContactEmailRepository>,
        user_repository: Arc<dyn UserRepository>,
        email_sender: Arc<dyn EmailSender>,
        verification_url: Option<String>,
    ) -> Self {
        Self {
            repository,
            user_repository,
            email_sender,
            verification_url,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.verification_url.is_some()
    }

    /// Whether the frontend should ask this user for a contact email: a wallet
    /// user with a placeholder login address and no verified contact email. Always
    /// false while the feature is off.
    pub fn needs_contact_email(&self, user: &User) -> bool {
        self.is_enabled()
            && user.auth_provider == NEAR_AUTH_PROVIDER
            && is_placeholder_email(&user.email)
            && user.contact_email_verified_at.is_none()
    }

    /// Store a verification for `email` and send the link to it. The outcome is
    /// the same whether or not the address already belongs to another account;
    /// that is only reported when the link is opened.
    pub async fn request_verification(
        &self,
        user_id: UserId,
        email: &str,
    ) -> Result<(), ContactEmailError> {
        let Some(verification_url) = self.verification_url.as_deref() else {
            return Err(ContactEmailError::Disabled);
        };
        let email = normalize_contact_email(email)?;

        let user = self
            .user_repository
            .get_by_id(user_id.clone())
            .await
            .map_err(ContactEmailError::Internal)?
            .ok_or(ContactEmailError::UserNotFound)?;
        let Some(account_id) = placeholder_account_id(&user.email) else {
            return Err(ContactEmailError::NotSupported);
        };

        if self
            .repository
            .delete_older_than(VERIFICATION_RETENTION_SECS)
            .await
            .is_err()
        {
            tracing::warn!("Failed to delete old contact email verifications");
        }

        let token = generate_verification_token();
        let expires_at = Utc::now() + Duration::hours(VERIFICATION_TOKEN_TTL_HOURS);
        let outcome = self
            .repository
            .create_verification(
                NewVerification {
                    user_id: user_id.0,
                    email: &email,
                    token_hash: &hash_verification_token(&token),
                    expires_at,
                },
                SEND_LIMITS,
            )
            .await
            .map_err(ContactEmailError::Internal)?;
        match outcome {
            CreateVerificationOutcome::Created => {}
            CreateVerificationOutcome::UserNotFound => return Err(ContactEmailError::UserNotFound),
            CreateVerificationOutcome::RateLimited { retry_after_secs } => {
                return Err(ContactEmailError::RateLimited { retry_after_secs })
            }
        }

        let message = ContactEmailVerificationEmail {
            recipient_email: email,
            account_id: account_id.to_string(),
            verification_url: format!("{verification_url}?token={token}"),
            expires_at,
        };
        // The provider's error text can quote the recipient, so only the user id is logged.
        match self
            .email_sender
            .send_contact_email_verification(&message)
            .await
        {
            Ok(EmailDeliveryOutcome::Sent { .. }) => Ok(()),
            Ok(EmailDeliveryOutcome::Skipped) => {
                tracing::warn!(user_id = %user_id.0, "Contact email verification not sent: no email sender configured");
                Err(ContactEmailError::SendFailed)
            }
            Err(_) => {
                tracing::warn!(user_id = %user_id.0, "Failed to send contact email verification");
                Err(ContactEmailError::SendFailed)
            }
        }
    }

    /// Confirm the address behind `token`. The token is the only credential: the
    /// link may be opened in a browser with no session.
    pub async fn confirm(&self, token: &str) -> Result<(), ContactEmailError> {
        if !self.is_enabled() {
            return Err(ContactEmailError::Disabled);
        }
        let token = token.trim();
        if token.is_empty() || token.len() > MAX_TOKEN_LENGTH {
            return Err(ContactEmailError::InvalidToken);
        }

        match self
            .repository
            .confirm(&hash_verification_token(token))
            .await
            .map_err(ContactEmailError::Internal)?
        {
            ConfirmOutcome::Verified { user_id } => {
                tracing::info!(%user_id, "Contact email verified");
                Ok(())
            }
            ConfirmOutcome::InvalidToken => Err(ContactEmailError::InvalidToken),
            ConfirmOutcome::AddressInUse { user_id } => {
                tracing::info!(%user_id, "Contact email not verified: address used by another account");
                Err(ContactEmailError::AddressInUse)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::UserRole;
    use crate::email::{
        EmailError, InvitationEmail, ModelDeprecationEmail, NoopEmailSender, PricingChangeEmail,
    };
    use chrono::TimeZone;
    use std::sync::Mutex;

    const VERIFICATION_URL: &str = "https://cloud.example.com/verify-contact-email";

    fn user(email: &str, auth_provider: &str) -> User {
        User {
            id: UserId(Uuid::new_v4()),
            email: email.to_string(),
            username: "user".to_string(),
            display_name: None,
            avatar_url: None,
            auth_provider: auth_provider.to_string(),
            provider_user_id: "provider-id".to_string(),
            role: UserRole::User,
            is_active: true,
            last_login: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tokens_revoked_at: None,
            contact_email: None,
            contact_email_verified_at: None,
            email_opt_in_at: None,
        }
    }

    struct FakeUsers(Option<User>);

    #[async_trait]
    impl UserRepository for FakeUsers {
        async fn create(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }
        async fn create_from_oauth(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
            _: String,
            _: String,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }
        async fn get_by_id(&self, id: UserId) -> anyhow::Result<Option<User>> {
            Ok(self.0.clone().filter(|user| user.id == id))
        }
        async fn get_by_email(&self, _: &str) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }
        async fn get_by_provider(&self, _: &str, _: &str) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }
        async fn update_email(&self, _: UserId, _: String) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn update(
            &self,
            _: UserId,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }
        async fn update_last_login(&self, _: UserId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn update_tokens_revoked_at(&self, _: UserId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete(&self, _: UserId) -> anyhow::Result<bool> {
            unimplemented!()
        }
        async fn list(&self, _: i64, _: i64) -> anyhow::Result<Vec<User>> {
            unimplemented!()
        }
    }

    /// What a stored verification looks like to the fake repository.
    #[derive(Debug, Clone)]
    struct StoredVerification {
        user_id: Uuid,
        email: String,
        token_hash: String,
        expires_at: DateTime<Utc>,
    }

    struct FakeRepository {
        create_outcome: CreateVerificationOutcome,
        confirm_outcome: ConfirmOutcome,
        created: Mutex<Vec<StoredVerification>>,
        limits: Mutex<Option<SendLimits>>,
        confirmed_hashes: Mutex<Vec<String>>,
    }

    impl FakeRepository {
        fn new(create_outcome: CreateVerificationOutcome, confirm_outcome: ConfirmOutcome) -> Self {
            Self {
                create_outcome,
                confirm_outcome,
                created: Mutex::new(Vec::new()),
                limits: Mutex::new(None),
                confirmed_hashes: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ContactEmailRepository for FakeRepository {
        async fn create_verification(
            &self,
            verification: NewVerification<'_>,
            limits: SendLimits,
        ) -> anyhow::Result<CreateVerificationOutcome> {
            *self.limits.lock().unwrap() = Some(limits);
            self.created.lock().unwrap().push(StoredVerification {
                user_id: verification.user_id,
                email: verification.email.to_string(),
                token_hash: verification.token_hash.to_string(),
                expires_at: verification.expires_at,
            });
            Ok(self.create_outcome.clone())
        }

        async fn confirm(&self, token_hash: &str) -> anyhow::Result<ConfirmOutcome> {
            self.confirmed_hashes
                .lock()
                .unwrap()
                .push(token_hash.to_string());
            Ok(self.confirm_outcome.clone())
        }

        async fn delete_older_than(&self, _: i64) -> anyhow::Result<u64> {
            Ok(0)
        }
    }

    struct RecordingSender {
        outcome: Result<EmailDeliveryOutcome, EmailError>,
        sent: Mutex<Vec<ContactEmailVerificationEmail>>,
    }

    impl RecordingSender {
        fn sending() -> Self {
            Self {
                outcome: Ok(EmailDeliveryOutcome::Sent { message_id: None }),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl EmailSender for RecordingSender {
        async fn send_invitation(
            &self,
            _: &InvitationEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            unimplemented!()
        }
        async fn send_model_deprecation(
            &self,
            _: &ModelDeprecationEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            unimplemented!()
        }
        async fn send_pricing_change(
            &self,
            _: &PricingChangeEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            unimplemented!()
        }
        async fn send_contact_email_verification(
            &self,
            email: &ContactEmailVerificationEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            self.sent.lock().unwrap().push(email.clone());
            self.outcome.clone()
        }
    }

    fn service(
        user: &User,
        repository: Arc<FakeRepository>,
        sender: Arc<RecordingSender>,
    ) -> ContactEmailService {
        ContactEmailService::new(
            repository,
            Arc::new(FakeUsers(Some(user.clone()))),
            sender,
            Some(VERIFICATION_URL.to_string()),
        )
    }

    fn happy_repository() -> Arc<FakeRepository> {
        Arc::new(FakeRepository::new(
            CreateVerificationOutcome::Created,
            ConfirmOutcome::Verified {
                user_id: Uuid::new_v4(),
            },
        ))
    }

    #[test]
    fn verification_expires_at_its_expiry_time() {
        let expires_at = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();

        assert!(verification_is_usable(
            expires_at,
            None,
            expires_at - Duration::seconds(1)
        ));
        assert!(!verification_is_usable(expires_at, None, expires_at));
        assert!(!verification_is_usable(
            expires_at,
            None,
            expires_at + Duration::hours(1)
        ));
    }

    #[test]
    fn verification_is_single_use() {
        let expires_at = Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap();
        let before_expiry = expires_at - Duration::hours(1);

        assert!(!verification_is_usable(
            expires_at,
            Some(before_expiry - Duration::minutes(1)),
            before_expiry
        ));
    }

    #[test]
    fn tokens_are_random_url_safe_and_stored_only_as_a_hash() {
        let first = generate_verification_token();
        let second = generate_verification_token();

        assert_ne!(first, second);
        // 32 bytes in unpadded base64.
        assert_eq!(first.len(), 43);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        let hash = hash_verification_token(&first);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash_verification_token(&first));
        assert_ne!(hash, hash_verification_token(&second));
        assert!(!hash.contains(&first));
    }

    #[test]
    fn normalize_contact_email_accepts_ordinary_addresses() {
        for (input, expected) in [
            ("alice@example.com", "alice@example.com"),
            (
                "  Alice.Smith+cloud@Example.co.uk \n",
                "Alice.Smith+cloud@Example.co.uk",
            ),
            (
                "a_b-c@sub-domain.example.org",
                "a_b-c@sub-domain.example.org",
            ),
        ] {
            assert_eq!(normalize_contact_email(input).unwrap(), expected);
        }
    }

    #[test]
    fn normalize_contact_email_rejects_malformed_and_placeholder_addresses() {
        let too_long = format!("{}@example.com", "a".repeat(250));
        let long_local = format!("{}@example.com", "a".repeat(65));
        for input in [
            "",
            "   ",
            "alice",
            "alice@",
            "@example.com",
            "alice@example",
            "alice@.com",
            "alice@example.",
            "alice@example..com",
            "alice @example.com",
            "alice@exa mple.com",
            "alice@@example.com",
            "a@b@example.com",
            ".alice@example.com",
            "alice.@example.com",
            "al..ice@example.com",
            "alice@-example.com",
            "alice@example-.com",
            "alice@exam_ple.com",
            "Alice <alice@example.com>",
            "alice@example.com,bob@example.com",
            "alice@example.com\r\nBcc: bob@example.com",
            "alice\u{0}@example.com",
            "älice@example.com",
            // Placeholder login addresses are not mailboxes.
            "alice.near@near",
            too_long.as_str(),
            long_local.as_str(),
        ] {
            assert!(
                matches!(
                    normalize_contact_email(input),
                    Err(ContactEmailError::InvalidEmail(_))
                ),
                "{input:?} should be rejected"
            );
        }
    }

    #[test]
    fn needs_contact_email_only_for_wallet_users_without_a_verified_address() {
        let wallet = user("alice.near@near", "near");
        let on = service(
            &wallet,
            happy_repository(),
            Arc::new(RecordingSender::sending()),
        );
        let off = ContactEmailService::new(
            happy_repository(),
            Arc::new(FakeUsers(None)),
            Arc::new(NoopEmailSender),
            None,
        );

        assert!(on.needs_contact_email(&wallet));
        // Off: never, so the frontend shows no prompt.
        assert!(!off.needs_contact_email(&wallet));
        assert!(!off.is_enabled());

        let mut verified = wallet.clone();
        verified.contact_email = Some("alice@example.com".to_string());
        verified.contact_email_verified_at = Some(Utc::now());
        assert!(!on.needs_contact_email(&verified));

        assert!(!on.needs_contact_email(&user("bob@example.com", "github")));
        assert!(!on.needs_contact_email(&user("bob@example.com", "near")));
        assert!(!on.needs_contact_email(&user("bob@near", "google")));
    }

    #[tokio::test]
    async fn request_stores_the_token_hash_and_emails_the_link() {
        let wallet = user("alice.near@near", "near");
        let repository = happy_repository();
        let sender = Arc::new(RecordingSender::sending());
        let service = service(&wallet, repository.clone(), sender.clone());
        let before = Utc::now();

        service
            .request_verification(wallet.id.clone(), " alice@example.com ")
            .await
            .unwrap();

        let created = repository.created.lock().unwrap();
        let sent = sender.sent.lock().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(sent.len(), 1);
        assert_eq!(created[0].user_id, wallet.id.0);
        assert_eq!(created[0].email, "alice@example.com");
        assert_eq!(*repository.limits.lock().unwrap(), Some(SEND_LIMITS));
        // Valid for 24 hours.
        let ttl = created[0].expires_at - before;
        assert!(ttl >= Duration::hours(24) && ttl < Duration::hours(24) + Duration::minutes(1));

        assert_eq!(sent[0].recipient_email, "alice@example.com");
        assert_eq!(sent[0].account_id, "alice.near");
        assert_eq!(sent[0].expires_at, created[0].expires_at);
        let token = sent[0]
            .verification_url
            .strip_prefix("https://cloud.example.com/verify-contact-email?token=")
            .expect("link goes to the frontend verification page");
        // The repository got the hash; the token only went into the email.
        assert_eq!(created[0].token_hash, hash_verification_token(token));
        assert_ne!(created[0].token_hash, token);
    }

    #[tokio::test]
    async fn request_is_refused_when_disabled_before_anything_is_stored() {
        let wallet = user("alice.near@near", "near");
        let repository = happy_repository();
        let service = ContactEmailService::new(
            repository.clone(),
            Arc::new(FakeUsers(Some(wallet.clone()))),
            Arc::new(RecordingSender::sending()),
            None,
        );

        let error = service
            .request_verification(wallet.id.clone(), "alice@example.com")
            .await
            .unwrap_err();

        assert!(matches!(error, ContactEmailError::Disabled));
        assert!(repository.created.lock().unwrap().is_empty());
        assert!(matches!(
            service.confirm("token").await.unwrap_err(),
            ContactEmailError::Disabled
        ));
        assert!(repository.confirmed_hashes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn request_is_only_for_placeholder_logins() {
        let github = user("bob@example.com", "github");
        let repository = happy_repository();
        let sender = Arc::new(RecordingSender::sending());
        let service = service(&github, repository.clone(), sender.clone());

        let error = service
            .request_verification(github.id.clone(), "bob@work.example")
            .await
            .unwrap_err();

        assert!(matches!(error, ContactEmailError::NotSupported));
        assert!(repository.created.lock().unwrap().is_empty());
        assert!(sender.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn request_rejects_an_invalid_address_before_anything_is_stored() {
        let wallet = user("alice.near@near", "near");
        let repository = happy_repository();
        let service = service(
            &wallet,
            repository.clone(),
            Arc::new(RecordingSender::sending()),
        );

        let error = service
            .request_verification(wallet.id.clone(), "not-an-address")
            .await
            .unwrap_err();

        assert!(matches!(error, ContactEmailError::InvalidEmail(_)));
        assert!(repository.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn request_over_the_limit_sends_nothing() {
        let wallet = user("alice.near@near", "near");
        let repository = Arc::new(FakeRepository::new(
            CreateVerificationOutcome::RateLimited {
                retry_after_secs: 1200,
            },
            ConfirmOutcome::InvalidToken,
        ));
        let sender = Arc::new(RecordingSender::sending());
        let service = service(&wallet, repository, sender.clone());

        let error = service
            .request_verification(wallet.id.clone(), "alice@example.com")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ContactEmailError::RateLimited {
                retry_after_secs: 1200
            }
        ));
        assert!(sender.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn request_reports_a_failed_send() {
        let wallet = user("alice.near@near", "near");
        for outcome in [
            Err(EmailError::new("provider rejected alice@example.com")),
            Ok(EmailDeliveryOutcome::Skipped),
        ] {
            let sender = Arc::new(RecordingSender {
                outcome,
                sent: Mutex::new(Vec::new()),
            });
            let service = service(&wallet, happy_repository(), sender);

            let error = service
                .request_verification(wallet.id.clone(), "alice@example.com")
                .await
                .unwrap_err();

            assert!(matches!(error, ContactEmailError::SendFailed));
            // The error shown to the caller never repeats the provider's text.
            assert!(!error.to_string().contains("alice@example.com"));
        }
    }

    #[tokio::test]
    async fn confirm_looks_the_token_up_by_its_hash() {
        let wallet = user("alice.near@near", "near");
        let repository = happy_repository();
        let service = service(
            &wallet,
            repository.clone(),
            Arc::new(RecordingSender::sending()),
        );

        service.confirm(" some-token ").await.unwrap();

        assert_eq!(
            *repository.confirmed_hashes.lock().unwrap(),
            vec![hash_verification_token("some-token")]
        );
    }

    #[tokio::test]
    async fn confirm_reports_a_duplicate_address_and_an_invalid_token_differently() {
        let wallet = user("alice.near@near", "near");
        let duplicate = Arc::new(FakeRepository::new(
            CreateVerificationOutcome::Created,
            ConfirmOutcome::AddressInUse {
                user_id: wallet.id.0,
            },
        ));
        let invalid = Arc::new(FakeRepository::new(
            CreateVerificationOutcome::Created,
            ConfirmOutcome::InvalidToken,
        ));

        let duplicate_error = service(&wallet, duplicate, Arc::new(RecordingSender::sending()))
            .confirm("token")
            .await
            .unwrap_err();
        let invalid_service = service(
            &wallet,
            invalid.clone(),
            Arc::new(RecordingSender::sending()),
        );

        assert!(matches!(duplicate_error, ContactEmailError::AddressInUse));
        assert!(matches!(
            invalid_service.confirm("token").await.unwrap_err(),
            ContactEmailError::InvalidToken
        ));
        // Empty and oversized tokens never reach the database.
        for token in [
            String::new(),
            "  ".to_string(),
            "t".repeat(MAX_TOKEN_LENGTH + 1),
        ] {
            assert!(matches!(
                invalid_service.confirm(&token).await.unwrap_err(),
                ContactEmailError::InvalidToken
            ));
        }
        assert_eq!(invalid.confirmed_hashes.lock().unwrap().len(), 1);
    }
}
