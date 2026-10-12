//! Contact email for NEAR wallet users: request a verification link, confirm it,
//! and keep the address across later wallet logins.

use crate::admin_deprecate_model::{create_model, seed_recent_usage};
use crate::common::*;
use crate::user_erasure::{erase, new_user, personal_org_id, signup};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use database::Database;
use services::admin::AdminRepository;
use services::auth::ports::OAuthUserInfo;
use services::auth::AuthServiceTrait;
use services::contact_email::{
    hash_verification_token, ConfirmOutcome, ContactEmailRepository, ContactEmailService,
    CreateVerificationOutcome, NewVerification, SendLimits,
};
use services::email::{
    near_placeholder_email, ContactEmailVerificationEmail, EmailDeliveryOutcome, EmailError,
    EmailSender, InvitationEmail, ModelDeprecationEmail, PricingChangeEmail,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const VERIFY_PAGE: &str = "https://cloud.test/verify-contact-email";

/// Far above anything the suite sends, so rows left by other tests and by
/// earlier runs on the same database never reach the global cap.
const NO_DAILY_CAP: i64 = i64::MAX;

/// Records verification emails instead of sending them, so a test can read the
/// token out of the link the way the recipient would.
#[derive(Default)]
struct RecordingEmailSender {
    sent: Mutex<Vec<ContactEmailVerificationEmail>>,
    /// While set, every send fails the way a provider outage would.
    failing: AtomicBool,
}

impl RecordingEmailSender {
    fn sent_to(&self, address: &str) -> Vec<ContactEmailVerificationEmail> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|email| email.recipient_email == address)
            .cloned()
            .collect()
    }

    /// Token from the latest email about `account_id`.
    fn latest_token(&self, account_id: &str) -> String {
        let sent = self.sent.lock().unwrap();
        let email = sent
            .iter()
            .rev()
            .find(|email| email.account_id == account_id)
            .expect("a verification email for the account");
        email
            .verification_url
            .strip_prefix(&format!("{VERIFY_PAGE}#token="))
            .expect("the link goes to the frontend page, with the token in the fragment")
            .to_string()
    }
}

#[async_trait]
impl EmailSender for RecordingEmailSender {
    async fn send_invitation(
        &self,
        _email: &InvitationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        Ok(EmailDeliveryOutcome::Skipped)
    }

    async fn send_model_deprecation(
        &self,
        _email: &ModelDeprecationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        Ok(EmailDeliveryOutcome::Skipped)
    }

    async fn send_pricing_change(
        &self,
        _email: &PricingChangeEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        Ok(EmailDeliveryOutcome::Skipped)
    }

    async fn send_contact_email_verification(
        &self,
        email: &ContactEmailVerificationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(EmailError::new("provider unavailable"));
        }
        self.sent.lock().unwrap().push(email.clone());
        Ok(EmailDeliveryOutcome::Sent { message_id: None })
    }
}

/// A server with contact email verification switched on and a recording sender.
async fn setup() -> (
    axum_test::TestServer,
    Arc<Database>,
    Arc<RecordingEmailSender>,
) {
    setup_with(NO_DAILY_CAP, false).await
}

/// The real repository, except that expiring a user's earlier links always
/// fails, as if the process had died between sending the email and that step.
struct CleanupFails(database::repositories::PgContactEmailRepository);

#[async_trait]
impl ContactEmailRepository for CleanupFails {
    async fn create_verification(
        &self,
        verification: NewVerification<'_>,
        limits: SendLimits,
    ) -> anyhow::Result<CreateVerificationOutcome> {
        self.0.create_verification(verification, limits).await
    }

    async fn expire_earlier_verifications(&self, _: Uuid, _: Uuid) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("the cleanup step did not run"))
    }

    async fn delete_verification(&self, verification_id: Uuid) -> anyhow::Result<()> {
        self.0.delete_verification(verification_id).await
    }

    async fn confirm(&self, token_hash: &str) -> anyhow::Result<ConfirmOutcome> {
        self.0.confirm(token_hash).await
    }

    async fn delete_older_than(&self, max_age_secs: i64) -> anyhow::Result<u64> {
        self.0.delete_older_than(max_age_secs).await
    }

    async fn remove_contact_email(&self, user_id: Uuid) -> anyhow::Result<bool> {
        self.0.remove_contact_email(user_id).await
    }
}

async fn setup_with(
    daily_cap: i64,
    cleanup_fails: bool,
) -> (
    axum_test::TestServer,
    Arc<Database>,
    Arc<RecordingEmailSender>,
) {
    let sender = Arc::new(RecordingEmailSender::default());
    let recording = sender.clone();
    let (server, database) = setup_test_server_with_domain_services(move |services, database| {
        let repository =
            database::repositories::PgContactEmailRepository::new(database.pool().clone());
        let repository: Arc<dyn ContactEmailRepository> = if cleanup_fails {
            Arc::new(CleanupFails(repository))
        } else {
            Arc::new(repository)
        };
        services.contact_email_service = Arc::new(ContactEmailService::new(
            repository,
            Arc::new(database::UserRepository::new(database.pool().clone())),
            recording,
            Some(VERIFY_PAGE.to_string()),
            daily_cap,
        ));
    })
    .await;
    (server, database, sender)
}

/// The real (non-mock) auth service, which holds the login code under test.
fn real_auth(database: &Arc<Database>) -> Arc<dyn AuthServiceTrait> {
    let mut config = test_config();
    config.auth.mock = false;
    api::init_auth_services(database.clone(), &config).auth_service
}

/// What a NEAR wallet login does once the signature is verified: the same
/// `OAuthUserInfo` that `NearAuthService` builds, through the real auth service.
/// Returns the session accepted by the mock-auth e2e server and the user id.
async fn near_login(auth: &Arc<dyn AuthServiceTrait>, account_id: &str) -> (String, Uuid) {
    let user = auth
        .get_or_create_oauth_user(OAuthUserInfo {
            provider: "near".to_string(),
            provider_user_id: account_id.to_string(),
            email: near_placeholder_email(account_id),
            username: account_id.to_string(),
            display_name: Some(account_id.to_string()),
            avatar_url: None,
        })
        .await
        .expect("wallet login should succeed");
    (format!("rt_{}", user.id.0), user.id.0)
}

fn new_account_id() -> String {
    format!("ce-{}.testnet", Uuid::new_v4().simple())
}

fn new_address() -> String {
    format!("ce-{}@example.com", Uuid::new_v4().simple())
}

async fn me(server: &axum_test::TestServer, session: &str) -> serde_json::Value {
    let response = server
        .get("/v1/users/me")
        .add_header("Authorization", format!("Bearer {session}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    response.json()
}

async fn request(
    server: &axum_test::TestServer,
    session: &str,
    email: &str,
) -> axum_test::TestResponse {
    server
        .post("/v1/users/me/contact-email")
        .add_header("Authorization", format!("Bearer {session}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "email": email }))
        .await
}

async fn remove(server: &axum_test::TestServer, session: &str) -> axum_test::TestResponse {
    server
        .delete("/v1/users/me/contact-email")
        .add_header("Authorization", format!("Bearer {session}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

/// Confirms with the token alone: no session, as when the link is opened in
/// another browser.
async fn verify(server: &axum_test::TestServer, token: &str) -> axum_test::TestResponse {
    server
        .post("/v1/users/contact-email/verify")
        .json(&serde_json::json!({ "token": token }))
        .await
}

fn error_type(response: &axum_test::TestResponse) -> String {
    response.json::<serde_json::Value>()["error"]["type"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn retry_after_secs(response: &axum_test::TestResponse) -> u64 {
    response
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .expect("a rate-limited response carries Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .expect("Retry-After is a number of seconds")
}

type ContactColumns = (
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

/// `(email, contact_email, contact_email_verified_at, email_opt_in_at)`.
async fn contact_columns(client: &deadpool_postgres::Object, user_id: Uuid) -> ContactColumns {
    let row = client
        .query_one(
            "SELECT email, contact_email, contact_email_verified_at, email_opt_in_at \
             FROM users WHERE id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1), row.get(2), row.get(3))
}

async fn verification_rows(client: &deadpool_postgres::Object, user_id: Uuid) -> i64 {
    client
        .query_one(
            "SELECT COUNT(*) FROM user_contact_email_verifications WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap()
        .get(0)
}

/// Requests and confirms `address` for the wallet user, asserting both succeed.
async fn add_contact_email(
    server: &axum_test::TestServer,
    sender: &RecordingEmailSender,
    session: &str,
    account_id: &str,
    address: &str,
) {
    let requested = request(server, session, address).await;
    assert_eq!(requested.status_code(), 200, "{}", requested.text());
    let confirmed = verify(server, &sender.latest_token(account_id)).await;
    assert_eq!(confirmed.status_code(), 200, "{}", confirmed.text());
}

#[tokio::test]
async fn wallet_user_requests_confirms_and_me_shows_the_contact_email() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let address = new_address();
    let (session, user_id) = near_login(&auth, &account_id).await;

    let before = me(&server, &session).await;
    assert_eq!(before["email"], near_placeholder_email(&account_id));
    assert_eq!(before["needs_contact_email"], true, "{before}");
    assert_eq!(before["contact_email_verified"], false, "{before}");
    assert!(before["contact_email"].is_null(), "{before}");

    let requested = request(&server, &session, &format!("  {address} ")).await;
    assert_eq!(requested.status_code(), 200, "{}", requested.text());
    assert_eq!(
        requested.json::<serde_json::Value>()["status"],
        "verification_sent"
    );

    // One email, to the address, naming the wallet account and linking to the frontend.
    let emails = sender.sent_to(&address);
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].account_id, account_id);
    let token = sender.latest_token(&account_id);

    // Only the hash of the token is stored, and nothing is on the user yet.
    let stored = client
        .query_one(
            "SELECT email, email_limit_key, token_hash, consumed_at, expires_at - created_at > INTERVAL '23 hours 59 minutes' \
                    AND expires_at - created_at < INTERVAL '24 hours 1 minute' AS valid_one_day \
             FROM user_contact_email_verifications WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>("email"), address);
    assert_eq!(stored.get::<_, String>("email_limit_key"), address);
    assert_eq!(
        stored.get::<_, String>("token_hash"),
        hash_verification_token(&token)
    );
    assert_ne!(stored.get::<_, String>("token_hash"), token);
    assert!(stored
        .get::<_, Option<DateTime<Utc>>>("consumed_at")
        .is_none());
    assert!(stored.get::<_, bool>("valid_one_day"));
    assert_eq!(
        contact_columns(&client, user_id).await,
        (near_placeholder_email(&account_id), None, None, None)
    );
    assert_eq!(me(&server, &session).await["needs_contact_email"], true);

    let confirmed = verify(&server, &token).await;
    assert_eq!(confirmed.status_code(), 200, "{}", confirmed.text());
    assert_eq!(confirmed.json::<serde_json::Value>()["status"], "verified");

    let after = me(&server, &session).await;
    assert_eq!(after["contact_email"], address, "{after}");
    assert_eq!(after["contact_email_verified"], true, "{after}");
    assert_eq!(after["needs_contact_email"], false, "{after}");
    // The login identity is untouched, and the opt-in is recorded at confirmation.
    assert_eq!(after["email"], near_placeholder_email(&account_id));
    let (login_email, contact_email, verified_at, opt_in_at) =
        contact_columns(&client, user_id).await;
    assert_eq!(login_email, near_placeholder_email(&account_id));
    assert_eq!(contact_email.as_deref(), Some(address.as_str()));
    assert!(verified_at.is_some());
    assert_eq!(opt_in_at, verified_at);

    // The link works once.
    let again = verify(&server, &token).await;
    assert_eq!(again.status_code(), 400, "{}", again.text());
    assert_eq!(error_type(&again), "invalid_verification_token");
}

#[tokio::test]
async fn contact_email_is_unchanged_by_later_wallet_logins() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let address = new_address();
    let (session, user_id) = near_login(&auth, &account_id).await;
    add_contact_email(&server, &sender, &session, &account_id, &address).await;
    let confirmed = contact_columns(&client, user_id).await;
    assert_eq!(confirmed.1.as_deref(), Some(address.as_str()));

    // The same wallet signs in again, twice.
    for _ in 0..2 {
        let (_, same_user) = near_login(&auth, &account_id).await;
        assert_eq!(same_user, user_id);
        assert_eq!(contact_columns(&client, user_id).await, confirmed);
    }

    // A login that has to rewrite `users.email` and the profile (the stored row
    // drifted from what the provider reports) still leaves the three columns alone.
    client
        .execute(
            "UPDATE users SET email = $2, display_name = 'Renamed' WHERE id = $1",
            &[&user_id, &format!("drifted-{}@example.com", Uuid::new_v4())],
        )
        .await
        .unwrap();
    let (_, same_user) = near_login(&auth, &account_id).await;
    assert_eq!(same_user, user_id);
    assert_eq!(contact_columns(&client, user_id).await, confirmed);

    let after = me(&server, &session).await;
    assert_eq!(after["email"], near_placeholder_email(&account_id));
    assert_eq!(after["display_name"], account_id);
    assert_eq!(after["contact_email"], address, "{after}");
    assert_eq!(after["contact_email_verified"], true, "{after}");
    assert_eq!(after["needs_contact_email"], false, "{after}");
}

#[tokio::test]
async fn expired_unknown_and_replaced_tokens_get_the_same_error() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;

    assert_eq!(
        request(&server, &session, &new_address())
            .await
            .status_code(),
        200
    );
    let replaced = sender.latest_token(&account_id);
    // A new request invalidates the earlier link.
    assert_eq!(
        request(&server, &session, &new_address())
            .await
            .status_code(),
        200
    );
    let expired = sender.latest_token(&account_id);
    client
        .execute(
            "UPDATE user_contact_email_verifications SET expires_at = NOW() - INTERVAL '1 second' \
             WHERE token_hash = $1",
            &[&hash_verification_token(&expired)],
        )
        .await
        .unwrap();

    let unknown = verify(&server, "not-a-token-we-issued").await;
    let unknown_body = unknown.text();
    assert_eq!(unknown.status_code(), 400, "{unknown_body}");
    assert_eq!(error_type(&unknown), "invalid_verification_token");
    let oversized = "x".repeat(5000);
    for token in [replaced.as_str(), expired.as_str(), "", oversized.as_str()] {
        let response = verify(&server, token).await;
        assert_eq!(response.status_code(), 400, "{}", response.text());
        assert_eq!(response.text(), unknown_body);
    }
    assert_eq!(contact_columns(&client, user_id).await.1, None);
    assert_eq!(me(&server, &session).await["needs_contact_email"], true);
}

#[tokio::test]
async fn confirmation_fails_when_the_address_belongs_to_another_user() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();

    // Another account signs in with the address (a GitHub or Google login).
    let (_, _, login_address) = new_user(&database).await;
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;

    // The request answers exactly as for a free address and still sends the
    // email, so it cannot be used to find out which addresses have accounts.
    let free = request(&server, &session, &new_address()).await;
    let taken = request(&server, &session, &login_address.to_uppercase()).await;
    assert_eq!(taken.status_code(), 200, "{}", taken.text());
    assert_eq!(taken.text(), free.text());
    assert_eq!(sender.sent_to(&login_address.to_uppercase()).len(), 1);

    let token = sender.latest_token(&account_id);
    let refused = verify(&server, &token).await;
    assert_eq!(refused.status_code(), 409, "{}", refused.text());
    assert_eq!(error_type(&refused), "conflict");
    // Nothing was written: no contact email, and the token was not used up.
    assert_eq!(
        contact_columns(&client, user_id).await,
        (near_placeholder_email(&account_id), None, None, None)
    );
    let consumed: Option<DateTime<Utc>> = client
        .query_one(
            "SELECT consumed_at FROM user_contact_email_verifications WHERE token_hash = $1",
            &[&hash_verification_token(&token)],
        )
        .await
        .unwrap()
        .get(0);
    assert!(consumed.is_none());
    assert_eq!(me(&server, &session).await["needs_contact_email"], true);

    // Another wallet user's verified contact email is taken too, in any letter case.
    let other_account = new_account_id();
    let other_address = new_address();
    let (other_session, other_user) = near_login(&auth, &other_account).await;
    add_contact_email(
        &server,
        &sender,
        &other_session,
        &other_account,
        &other_address,
    )
    .await;
    let requested = request(&server, &session, &other_address.to_uppercase()).await;
    assert_eq!(requested.status_code(), 200, "{}", requested.text());
    let refused = verify(&server, &sender.latest_token(&account_id)).await;
    assert_eq!(refused.status_code(), 409, "{}", refused.text());
    assert_eq!(contact_columns(&client, user_id).await.1, None);
    assert_eq!(
        contact_columns(&client, other_user).await.1.as_deref(),
        Some(other_address.as_str())
    );

    // The owner can confirm their own address again (a re-verification is not a conflict).
    add_contact_email(
        &server,
        &sender,
        &other_session,
        &other_account,
        &other_address,
    )
    .await;
}

#[tokio::test]
async fn concurrent_confirmations_let_exactly_one_through() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();

    // Two wallet users confirm the same address at the same time. Whichever of
    // the application check or the unique index catches it, one wins and the
    // other gets a conflict.
    let mut loser = None;
    for _ in 0..4 {
        let address = new_address();
        let first_account = new_account_id();
        let second_account = new_account_id();
        let (first_session, first_user) = near_login(&auth, &first_account).await;
        let (second_session, second_user) = near_login(&auth, &second_account).await;
        assert_eq!(
            request(&server, &first_session, &address)
                .await
                .status_code(),
            200
        );
        assert_eq!(
            request(&server, &second_session, &address)
                .await
                .status_code(),
            200
        );
        let first_token = sender.latest_token(&first_account);
        let second_token = sender.latest_token(&second_account);

        let (first, second) = tokio::join!(
            verify(&server, &first_token),
            verify(&server, &second_token)
        );

        let mut statuses = [first.status_code().as_u16(), second.status_code().as_u16()];
        statuses.sort();
        assert_eq!(statuses, [200, 409]);
        let holders: Vec<Uuid> = client
            .query(
                "SELECT id FROM users WHERE lower(contact_email) = lower($1) \
                   AND contact_email_verified_at IS NOT NULL",
                &[&address],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(holders.len(), 1);
        let losing_user = if holders[0] == first_user {
            second_user
        } else {
            first_user
        };
        assert_eq!(contact_columns(&client, losing_user).await.1, None);
        loser = Some((losing_user, address));
    }

    // The unique index holds even with the application check out of the picture.
    let (losing_user, address) = loser.unwrap();
    let error = client
        .execute(
            "UPDATE users SET contact_email = upper($2), contact_email_verified_at = NOW() WHERE id = $1",
            &[&losing_user, &address],
        )
        .await
        .expect_err("a second verified holder of the address must be rejected");
    assert_eq!(
        error.code(),
        Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
    );

    // The same link opened twice at once is confirmed once.
    let account_id = new_account_id();
    let (session, _) = near_login(&auth, &account_id).await;
    assert_eq!(
        request(&server, &session, &new_address())
            .await
            .status_code(),
        200
    );
    let token = sender.latest_token(&account_id);
    let (first, second) = tokio::join!(verify(&server, &token), verify(&server, &token));
    let mut statuses = [first.status_code().as_u16(), second.status_code().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 400]);
}

#[tokio::test]
async fn a_user_can_request_three_verification_emails_per_hour() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;

    for _ in 0..3 {
        let response = request(&server, &session, &new_address()).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
    }
    let latest_token = sender.latest_token(&account_id);

    let limited = request(&server, &session, &new_address()).await;
    assert_eq!(limited.status_code(), 429, "{}", limited.text());
    assert_eq!(error_type(&limited), "rate_limit_exceeded");
    let wait = retry_after_secs(&limited);
    assert!((3000..=3600).contains(&wait), "Retry-After was {wait}");
    // The refused request sent nothing and stored nothing.
    assert_eq!(sender.sent.lock().unwrap().len(), 3);
    assert_eq!(verification_rows(&client, user_id).await, 3);

    // The limit is a rolling hour: once the earlier sends are older, it lifts.
    client
        .execute(
            "UPDATE user_contact_email_verifications SET created_at = created_at - INTERVAL '61 minutes' \
             WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .unwrap();
    // A refused request did not invalidate the pending link.
    assert_eq!(verify(&server, &latest_token).await.status_code(), 200);
    let response = request(&server, &session, &new_address()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
}

#[tokio::test]
async fn an_address_gets_three_verification_emails_per_day_across_users() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let address = new_address();

    let mut senders = Vec::new();
    for _ in 0..3 {
        let (session, _) = near_login(&auth, &new_account_id()).await;
        let response = request(&server, &session, &address).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        senders.push(session);
    }

    // A fourth wallet account, with one delivered link of its own.
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;
    let ordinary = request(&server, &session, &new_address()).await;
    assert_eq!(ordinary.status_code(), 200, "{}", ordinary.text());
    let earlier_link = sender.latest_token(&account_id);

    // Asking for the address that is at its limit gets the ordinary answer, so
    // the caller learns nothing about it, but nothing is sent or stored. The
    // limit follows the mailbox: letter case and a `+tag` make no difference.
    let (local, domain) = address.split_once('@').unwrap();
    let tagged = format!("{local}+news@{domain}");
    for target in [address.clone(), address.to_uppercase(), tagged.clone()] {
        let quiet = request(&server, &session, &target).await;
        assert_eq!(quiet.status_code(), 200, "{}", quiet.text());
        assert_eq!(quiet.text(), ordinary.text());
        assert!(quiet
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .is_none());
    }
    assert_eq!(sender.sent_to(&address).len(), 3);
    assert!(sender.sent_to(&address.to_uppercase()).is_empty());
    assert!(sender.sent_to(&tagged).is_empty());
    assert_eq!(verification_rows(&client, user_id).await, 1);
    // No email went out, so the user's earlier link was not replaced.
    assert_eq!(verify(&server, &earlier_link).await.status_code(), 200);

    // Gmail ignores dots too: three variants of one mailbox use up its sends.
    let mailbox = format!("ce{}", Uuid::new_v4().simple());
    let (head, tail) = mailbox.split_at(4);
    let variants = [
        format!("{mailbox}@gmail.com"),
        format!("{head}.{tail}@Gmail.com"),
        format!("{mailbox}+cloud@gmail.com"),
    ];
    for (variant, sender_session) in variants.iter().zip(&senders) {
        let response = request(&server, sender_session, variant).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        // Sent to the address exactly as typed.
        assert_eq!(sender.sent_to(variant).len(), 1);
    }
    let fourth_variant = format!("c.e{}@gmail.com", &mailbox[2..]);
    let quiet = request(&server, &session, &fourth_variant).await;
    assert_eq!(quiet.status_code(), 200, "{}", quiet.text());
    assert!(sender.sent_to(&fourth_variant).is_empty());
    let stored: Vec<String> = client
        .query(
            "SELECT email FROM user_contact_email_verifications WHERE email_limit_key = $1 \
             ORDER BY created_at",
            &[&format!("{mailbox}@gmail.com")],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(stored, variants);

    // The limit is a rolling day. Just short of it the address is still closed.
    client
        .execute(
            "UPDATE user_contact_email_verifications \
             SET created_at = created_at - INTERVAL '23 hours 50 minutes' \
             WHERE email_limit_key = $1",
            &[&address],
        )
        .await
        .unwrap();
    assert_eq!(
        request(&server, &session, &address).await.status_code(),
        200
    );
    assert_eq!(sender.sent_to(&address).len(), 3);
    // Past it, the address is mailed again.
    client
        .execute(
            "UPDATE user_contact_email_verifications SET created_at = created_at - INTERVAL '11 minutes' \
             WHERE email_limit_key = $1",
            &[&address],
        )
        .await
        .unwrap();
    let response = request(&server, &session, &address).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(sender.sent_to(&address).len(), 4);
    // Rows older than a day are deleted when the next request comes in.
    let remaining: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM user_contact_email_verifications WHERE email_limit_key = $1",
            &[&address],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(remaining, 1);
}

#[tokio::test]
async fn only_the_newest_link_confirms_even_when_expiring_the_older_one_failed() {
    // On this server the step that expires earlier links never succeeds.
    let (server, database, sender) = setup_with(NO_DAILY_CAP, true).await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;

    let older_address = new_address();
    let newer_address = new_address();
    assert_eq!(
        request(&server, &session, &older_address)
            .await
            .status_code(),
        200
    );
    let older_link = sender.latest_token(&account_id);
    // The request still succeeds: the email went out, the cleanup is not part
    // of the answer.
    let response = request(&server, &session, &newer_address).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let newer_link = sender.latest_token(&account_id);

    // The older link really was left unexpired and unused.
    let left_usable: bool = client
        .query_one(
            "SELECT consumed_at IS NULL AND expires_at > NOW() + INTERVAL '23 hours' \
             FROM user_contact_email_verifications WHERE token_hash = $1",
            &[&hash_verification_token(&older_link)],
        )
        .await
        .unwrap()
        .get(0);
    assert!(left_usable);

    // It is refused all the same, with the answer an unknown token gets.
    let unknown = verify(&server, "not-a-token-we-issued").await;
    let refused = verify(&server, &older_link).await;
    assert_eq!(refused.status_code(), 400, "{}", refused.text());
    assert_eq!(refused.text(), unknown.text());
    assert_eq!(contact_columns(&client, user_id).await.1, None);

    // A send that fails removes its row, so the newest delivered link is still
    // the newest and confirms.
    sender.failing.store(true, Ordering::SeqCst);
    assert_eq!(
        request(&server, &session, &new_address())
            .await
            .status_code(),
        502
    );
    assert_eq!(verify(&server, &older_link).await.status_code(), 400);
    let confirmed = verify(&server, &newer_link).await;
    assert_eq!(confirmed.status_code(), 200, "{}", confirmed.text());
    assert_eq!(
        contact_columns(&client, user_id).await.1.as_deref(),
        Some(newer_address.as_str())
    );
}

#[tokio::test]
async fn retry_after_waits_for_the_send_that_restores_capacity() {
    let (server, database, _sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let (session, user_id) = near_login(&auth, &new_account_id()).await;

    // Five sends in the last hour against a limit of three: 50, 40, 30, 20 and
    // 10 minutes ago. (The API never lets a user get there; a lowered limit or
    // the overshoot the global cap allows does.)
    client
        .execute(
            "INSERT INTO user_contact_email_verifications \
                 (user_id, email, email_limit_key, token_hash, expires_at, created_at) \
             SELECT $1, address, address, md5(random()::text) || md5(random()::text), \
                    NOW() + INTERVAL '1 day', NOW() - make_interval(mins => minutes_ago) \
             FROM (SELECT minutes_ago, 'ra-' || minutes_ago || '-' || $2 || '@example.com' AS address \
                   FROM unnest(ARRAY[50, 40, 30, 20, 10]) AS minutes_ago) sends",
            &[&user_id, &Uuid::new_v4().simple().to_string()],
        )
        .await
        .unwrap();

    // The user is under the limit again once three of the five have left the
    // hour: when the send from 30 minutes ago does, not the oldest one.
    let limited = request(&server, &session, &new_address()).await;
    assert_eq!(limited.status_code(), 429, "{}", limited.text());
    let wait = retry_after_secs(&limited);
    assert!((1700..=1800).contains(&wait), "Retry-After was {wait}");

    // Exactly at the limit it is the oldest send that matters. Left: 50, 40, 10.
    client
        .execute(
            "DELETE FROM user_contact_email_verifications WHERE user_id = $1 \
               AND created_at BETWEEN NOW() - INTERVAL '35 minutes' AND NOW() - INTERVAL '15 minutes'",
            &[&user_id],
        )
        .await
        .unwrap();
    assert_eq!(verification_rows(&client, user_id).await, 3);
    let limited = request(&server, &session, &new_address()).await;
    assert_eq!(limited.status_code(), 429, "{}", limited.text());
    let wait = retry_after_secs(&limited);
    assert!((500..=600).contains(&wait), "Retry-After was {wait}");
}

#[tokio::test]
async fn a_failed_send_spends_no_limit_and_keeps_the_earlier_link() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;

    // One delivered link.
    let response = request(&server, &session, &new_address()).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let earlier_link = sender.latest_token(&account_id);

    // The provider now refuses every send. Four attempts: were they counted,
    // the third would already be over the per-user limit of three an hour.
    sender.failing.store(true, Ordering::SeqCst);
    let address = new_address();
    for _ in 0..4 {
        let failed = request(&server, &session, &address).await;
        assert_eq!(failed.status_code(), 502, "{}", failed.text());
        assert_eq!(error_type(&failed), "email_delivery_failed");
    }
    // The unsent rows are gone, and the earlier link is still good.
    assert_eq!(verification_rows(&client, user_id).await, 1);
    let unsent: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM user_contact_email_verifications WHERE email_limit_key = $1",
            &[&address],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(unsent, 0);
    assert_eq!(verify(&server, &earlier_link).await.status_code(), 200);

    // Once the provider is back, the address gets its email: the failed
    // attempts did not use up its sends either.
    sender.failing.store(false, Ordering::SeqCst);
    let response = request(&server, &session, &address).await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    assert_eq!(sender.sent_to(&address).len(), 1);
    // Two sends counted so far: one more is allowed, the next is over the limit.
    assert_eq!(
        request(&server, &session, &new_address())
            .await
            .status_code(),
        200
    );
    assert_eq!(
        request(&server, &session, &new_address())
            .await
            .status_code(),
        429
    );
}

/// Runs alone (see `.config/nextest.toml`): the cap counts every verification
/// row in the database, so a request from another test would move the count.
#[tokio::test]
async fn serial_daily_cap_limits_sends_across_all_users() {
    let pool = db_setup::create_test_pool().await;
    let already_sent: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT COUNT(*) FROM user_contact_email_verifications \
             WHERE created_at > NOW() - INTERVAL '24 hours'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    // Room for exactly two more sends today.
    let (server, database, sender) = setup_with(already_sent + 2, false).await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();

    for _ in 0..2 {
        let (session, _) = near_login(&auth, &new_account_id()).await;
        let response = request(&server, &session, &new_address()).await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
    }

    // A third user and a third address: neither has sent or received anything.
    let (session, user_id) = near_login(&auth, &new_account_id()).await;
    let address = new_address();
    let limited = request(&server, &session, &address).await;
    assert_eq!(limited.status_code(), 429, "{}", limited.text());
    assert_eq!(error_type(&limited), "rate_limit_exceeded");
    let wait = retry_after_secs(&limited);
    assert!((1..=86_400).contains(&wait), "Retry-After was {wait}");
    assert!(sender.sent_to(&address).is_empty());
    assert_eq!(verification_rows(&client, user_id).await, 0);
    assert_eq!(sender.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn only_placeholder_logins_can_add_a_contact_email() {
    let (server, database, sender) = setup().await;
    let client = database.pool().get().await.unwrap();
    let (session, user_id, login_address) = new_user(&database).await;

    // A user who signs in with an email is not asked and cannot use the endpoint.
    assert_eq!(me(&server, &session).await["needs_contact_email"], false);
    let address = new_address();
    let refused = request(&server, &session, &address).await;
    assert_eq!(refused.status_code(), 403, "{}", refused.text());
    assert_eq!(error_type(&refused), "forbidden");
    assert!(sender.sent_to(&address).is_empty());
    assert_eq!(verification_rows(&client, user_id).await, 0);
    assert_eq!(
        contact_columns(&client, user_id).await,
        (login_address, None, None, None)
    );

    // Neither is an account from another provider whose login address merely
    // looks like a placeholder: the wallet provider is required as well.
    let unique = Uuid::new_v4().simple().to_string();
    let (lookalike_session, lookalike_user, _) = signup(
        &database,
        "github",
        &format!("gh-{unique}"),
        &format!("gh-{unique}@near"),
    )
    .await;
    assert_eq!(
        me(&server, &lookalike_session).await["needs_contact_email"],
        false
    );
    let refused = request(&server, &lookalike_session, &address).await;
    assert_eq!(refused.status_code(), 403, "{}", refused.text());
    assert_eq!(verification_rows(&client, lookalike_user).await, 0);

    // A wallet user must give a real address.
    let auth = real_auth(&database);
    let account_id = new_account_id();
    let (wallet_session, wallet_user) = near_login(&auth, &account_id).await;
    for invalid in [
        "",
        "not-an-address",
        "someone@near",
        "a@b@example.com",
        "Someone <someone@example.com>",
        "someone@mail.test",
        "someone@example.invalid",
        "someone@host.localhost",
        "someone@printer.local",
        "someone@mail.example",
    ] {
        let response = request(&server, &wallet_session, invalid).await;
        assert_eq!(
            response.status_code(),
            400,
            "{invalid:?}: {}",
            response.text()
        );
        assert_eq!(error_type(&response), "bad_request");
    }
    assert_eq!(verification_rows(&client, wallet_user).await, 0);
    assert!(sender.sent.lock().unwrap().is_empty());

    // The request endpoint needs a session.
    let anonymous = server
        .post("/v1/users/me/contact-email")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "email": new_address() }))
        .await;
    assert_eq!(anonymous.status_code(), 401, "{}", anonymous.text());
}

#[tokio::test]
async fn switched_off_the_endpoints_answer_404_and_an_address_can_still_be_removed() {
    // The default configuration: contact email verification is off.
    let (server, database) = setup_test_server_with_database().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;

    let profile = me(&server, &session).await;
    assert_eq!(profile["needs_contact_email"], false, "{profile}");
    assert_eq!(profile["contact_email_verified"], false, "{profile}");
    assert!(profile["contact_email"].is_null(), "{profile}");

    // Both endpoints answer as if they were not there: never a 5xx.
    let refused = request(&server, &session, &new_address()).await;
    assert_eq!(refused.status_code(), 404, "{}", refused.text());
    assert_eq!(error_type(&refused), "not_found");
    assert_eq!(verification_rows(&client, user_id).await, 0);

    let refused = verify(&server, "any-token").await;
    assert_eq!(refused.status_code(), 404, "{}", refused.text());
    assert_eq!(error_type(&refused), "not_found");

    // An address confirmed while the feature was on can still be withdrawn.
    let address = new_address();
    client
        .execute(
            "UPDATE users SET contact_email = $2, contact_email_verified_at = NOW(), \
             email_opt_in_at = NOW() WHERE id = $1",
            &[&user_id, &address],
        )
        .await
        .unwrap();
    let profile = me(&server, &session).await;
    assert_eq!(profile["contact_email"], address, "{profile}");
    assert_eq!(profile["needs_contact_email"], false, "{profile}");

    for _ in 0..2 {
        let removed = remove(&server, &session).await;
        assert_eq!(removed.status_code(), 204, "{}", removed.text());
        assert_eq!(
            contact_columns(&client, user_id).await,
            (near_placeholder_email(&account_id), None, None, None)
        );
    }
    assert_eq!(me(&server, &session).await["needs_contact_email"], false);
}

#[tokio::test]
async fn switched_on_by_configuration_wallet_users_are_asked_for_an_email() {
    let (server, database) = setup_test_server_with_config_and_database(|config| {
        config.invitation_email = config::InvitationEmailConfig {
            contact_email_verification_enabled: true,
            from_email: Some("NEAR AI Cloud <no-reply@example.com>".to_string()),
            resend_api_key: Some("re_test".to_string()),
            frontend_base_url: Some("https://cloud.test".to_string()),
            ..Default::default()
        };
    })
    .await;
    let auth = real_auth(&database);
    let (wallet_session, _) = near_login(&auth, &new_account_id()).await;
    let (email_session, _, _) = new_user(&database).await;

    assert_eq!(
        me(&server, &wallet_session).await["needs_contact_email"],
        true
    );
    assert_eq!(
        me(&server, &email_session).await["needs_contact_email"],
        false
    );
}

#[tokio::test]
async fn erasure_clears_the_contact_email_and_its_verifications() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let address = new_address();
    let (session, user_id) = near_login(&auth, &account_id).await;
    add_contact_email(&server, &sender, &session, &account_id, &address).await;
    // A further, still pending request of the user.
    let pending_address = new_address();
    assert_eq!(
        request(&server, &session, &pending_address)
            .await
            .status_code(),
        200
    );
    let pending_token = sender.latest_token(&account_id);
    // Someone else has a pending request for the erased user's address.
    let other_account = new_account_id();
    let (other_session, other_user) = near_login(&auth, &other_account).await;
    assert_eq!(
        request(&server, &other_session, &address)
            .await
            .status_code(),
        200
    );
    assert_eq!(verification_rows(&client, user_id).await, 2);
    assert_eq!(verification_rows(&client, other_user).await, 1);
    // And a third account has one for the same mailbox, spelled with a `+tag`.
    let (local, domain) = address.split_once('@').unwrap();
    let tagged = format!("{}+Promo@{domain}", local.to_uppercase());
    let (tagging_session, tagging_user) = near_login(&auth, &new_account_id()).await;
    assert_eq!(
        request(&server, &tagging_session, &tagged)
            .await
            .status_code(),
        200
    );
    assert_eq!(sender.sent_to(&tagged).len(), 1);
    // An organization invited the user at the contact address.
    let org = create_org(&server).await;
    let invited = server
        .post(format!("/v1/organizations/{}/members/invite-by-email", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "invitations": [{ "email": address.to_uppercase(), "role": "member" }]
        }))
        .await;
    assert_eq!(invited.status_code(), 200, "{}", invited.text());
    let invitations = || async {
        client
            .query_one(
                "SELECT COUNT(*) FROM organization_invitations WHERE lower(email) = lower($1)",
                &[&address],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    assert_eq!(invitations().await, 1);

    let erased = erase(&server, user_id, &near_placeholder_email(&account_id)).await;
    assert_eq!(erased.status_code(), 200, "{}", erased.text());

    let (login_email, contact_email, verified_at, opt_in_at) =
        contact_columns(&client, user_id).await;
    assert_eq!(login_email, format!("erased+{user_id}@erased.invalid"));
    assert_eq!((contact_email, verified_at, opt_in_at), (None, None, None));
    assert_eq!(verification_rows(&client, user_id).await, 0);
    // No row anywhere still holds either of the erased user's addresses.
    let leftovers: i64 = client
        .query_one(
            "SELECT (SELECT COUNT(*) FROM user_contact_email_verifications \
                      WHERE lower(email) IN (lower($1), lower($2))) \
                  + (SELECT COUNT(*) FROM users WHERE lower(contact_email) IN (lower($1), lower($2)))",
            &[&address, &pending_address],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(leftovers, 0);
    assert_eq!(invitations().await, 0);
    // That includes the other spelling of the contact address.
    assert_eq!(verification_rows(&client, tagging_user).await, 0);
    let by_key: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM user_contact_email_verifications WHERE email_limit_key = $1",
            &[&address],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(by_key, 0);

    // The erased user's pending link is dead, and the address is free again.
    assert_eq!(verify(&server, &pending_token).await.status_code(), 400);
    add_contact_email(&server, &sender, &other_session, &other_account, &address).await;
    assert_eq!(
        contact_columns(&client, other_user).await.1.as_deref(),
        Some(address.as_str())
    );
}

#[tokio::test]
async fn a_user_can_remove_the_contact_email() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let address = new_address();
    let (session, user_id) = near_login(&auth, &account_id).await;
    add_contact_email(&server, &sender, &session, &account_id, &address).await;
    // A link for a further address is still pending.
    assert_eq!(
        request(&server, &session, &new_address())
            .await
            .status_code(),
        200
    );
    let pending_link = sender.latest_token(&account_id);

    let removed = remove(&server, &session).await;
    assert_eq!(removed.status_code(), 204, "{}", removed.text());
    assert!(removed.text().is_empty());

    // The address and both timestamps are gone, and the user is asked again.
    assert_eq!(
        contact_columns(&client, user_id).await,
        (near_placeholder_email(&account_id), None, None, None)
    );
    let profile = me(&server, &session).await;
    assert!(profile["contact_email"].is_null(), "{profile}");
    assert_eq!(profile["contact_email_verified"], false, "{profile}");
    assert_eq!(profile["needs_contact_email"], true, "{profile}");
    // The pending link cannot bring an address back.
    assert_eq!(verify(&server, &pending_link).await.status_code(), 400);
    assert_eq!(contact_columns(&client, user_id).await.1, None);

    // Removing again changes nothing and still succeeds.
    let updated_at = || async {
        client
            .query_one("SELECT updated_at FROM users WHERE id = $1", &[&user_id])
            .await
            .unwrap()
            .get::<_, DateTime<Utc>>(0)
    };
    let before = updated_at().await;
    assert_eq!(remove(&server, &session).await.status_code(), 204);
    assert_eq!(updated_at().await, before);

    // The address is free for another account.
    let other_account = new_account_id();
    let (other_session, other_user) = near_login(&auth, &other_account).await;
    add_contact_email(&server, &sender, &other_session, &other_account, &address).await;
    // One user's removal does not touch another user.
    assert_eq!(remove(&server, &session).await.status_code(), 204);
    assert_eq!(
        contact_columns(&client, other_user).await.1.as_deref(),
        Some(address.as_str())
    );

    // It needs a session, and works for an account with nothing to remove.
    let anonymous = server
        .delete("/v1/users/me/contact-email")
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(anonymous.status_code(), 401, "{}", anonymous.text());
    let (email_session, _, _) = new_user(&database).await;
    assert_eq!(remove(&server, &email_session).await.status_code(), 204);
}

#[tokio::test]
async fn confirming_a_new_address_replaces_the_old_one() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let client = database.pool().get().await.unwrap();
    let account_id = new_account_id();
    let first_address = new_address();
    let second_address = new_address();
    let (session, user_id) = near_login(&auth, &account_id).await;
    add_contact_email(&server, &sender, &session, &account_id, &first_address).await;
    let (_, _, first_verified_at, first_opt_in_at) = contact_columns(&client, user_id).await;

    // Asking for another address changes nothing until its link is confirmed.
    assert_eq!(
        request(&server, &session, &second_address)
            .await
            .status_code(),
        200
    );
    assert_eq!(
        me(&server, &session).await["contact_email"],
        first_address.as_str()
    );
    let confirmed = verify(&server, &sender.latest_token(&account_id)).await;
    assert_eq!(confirmed.status_code(), 200, "{}", confirmed.text());

    // The new address took the place of the old one, with a new opt-in time.
    let (_, contact_email, verified_at, opt_in_at) = contact_columns(&client, user_id).await;
    assert_eq!(contact_email.as_deref(), Some(second_address.as_str()));
    assert!(verified_at > first_verified_at);
    assert!(opt_in_at > first_opt_in_at);
    let profile = me(&server, &session).await;
    assert_eq!(profile["contact_email"], second_address.as_str());
    assert_eq!(profile["needs_contact_email"], false, "{profile}");

    // Nobody holds the old address any more: another account can confirm it.
    let holders: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM users WHERE lower(contact_email) = lower($1)",
            &[&first_address],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(holders, 0);
    let other_account = new_account_id();
    let (other_session, _) = near_login(&auth, &other_account).await;
    add_contact_email(
        &server,
        &sender,
        &other_session,
        &other_account,
        &first_address,
    )
    .await;
}

/// Forces the branch that the application check cannot cover: another holder of
/// the address exists but is not committed yet, so only the unique index sees it.
#[tokio::test]
async fn the_unique_index_refuses_an_address_another_transaction_is_taking() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let address = new_address();
    let (_, holder_user) = near_login(&auth, &new_account_id()).await;
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;
    assert_eq!(
        request(&server, &session, &address).await.status_code(),
        200
    );
    let token = sender.latest_token(&account_id);

    // Another user's confirmation of the same address has written its row and
    // not committed. A plain read, like the application check, does not see it.
    let mut holder = database.pool().get().await.unwrap();
    let uncommitted = holder.transaction().await.unwrap();
    uncommitted
        .execute(
            "UPDATE users SET contact_email = upper($2), contact_email_verified_at = NOW(), \
             email_opt_in_at = NOW() WHERE id = $1",
            &[&holder_user, &address],
        )
        .await
        .unwrap();
    let holder_xid: String = uncommitted
        .query_one(
            "SELECT backend_xid::text FROM pg_stat_activity WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    let observer = database.pool().get().await.unwrap();
    let release_once_blocked = async {
        // The confirmation can only wait on that transaction at the unique
        // index: nothing before it in the confirmation takes a lock it holds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let waiting: i64 = observer
                .query_one(
                    "SELECT COUNT(*) FROM pg_locks WHERE NOT granted \
                       AND locktype = 'transactionid' AND transactionid::text = $1",
                    &[&holder_xid],
                )
                .await
                .unwrap()
                .get(0);
            if waiting > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the confirmation never waited on the unique index"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        uncommitted.commit().await.unwrap();
    };
    let (refused, ()) = tokio::join!(verify(&server, &token), release_once_blocked);

    assert_eq!(refused.status_code(), 409, "{}", refused.text());
    assert_eq!(error_type(&refused), "conflict");
    // The loser's transaction was rolled back whole: no address, token unused.
    assert_eq!(
        contact_columns(&observer, user_id).await,
        (near_placeholder_email(&account_id), None, None, None)
    );
    let consumed: Option<DateTime<Utc>> = observer
        .query_one(
            "SELECT consumed_at FROM user_contact_email_verifications WHERE token_hash = $1",
            &[&hash_verification_token(&token)],
        )
        .await
        .unwrap()
        .get(0);
    assert!(consumed.is_none());
    assert_eq!(
        contact_columns(&observer, holder_user).await.1,
        Some(address.to_uppercase())
    );
}

#[tokio::test]
async fn notification_recipients_skip_placeholder_users_and_use_the_contact_email() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let old = format!("test-contact-email-old-{}", Uuid::new_v4());
    let successor = format!("test-contact-email-successor-{}", Uuid::new_v4());
    create_model(&server, &old, &[]).await;
    create_model(&server, &successor, &[]).await;

    // Two organizations used the model: one owned by a wallet user, one by a
    // user who signs in with an email.
    let account_id = new_account_id();
    let (wallet_session, wallet_user) = near_login(&auth, &account_id).await;
    let (email_session, email_user, login_address) = new_user(&database).await;
    for session in [&wallet_session, &email_session] {
        let org_id = personal_org_id(&server, session).await;
        let workspaces = list_workspaces_with_session(&server, org_id.clone(), session).await;
        let workspace = workspaces.first().unwrap();
        let api_key = create_api_key_in_workspace_with_session(
            &server,
            workspace.id.clone(),
            "contact-email".to_string(),
            session,
        )
        .await;
        seed_recent_usage(
            &database,
            Uuid::parse_str(&org_id).unwrap(),
            Uuid::parse_str(&workspace.id).unwrap(),
            Uuid::parse_str(&api_key.id).unwrap(),
            &old,
        )
        .await;
    }

    let preview = || async {
        let response = server
            .post(&format!("/v1/admin/models/{old}/deprecation/preview"))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .json(&serde_json::json!({
                "successorModelId": successor,
                "deprecationDate": "2030-01-01"
            }))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
        response.json::<serde_json::Value>()
    };
    let repository = database::repositories::AdminCompositeRepository::new(database.pool().clone());
    let since = Utc::now() - chrono::Duration::days(30);
    let recipients = || async {
        let deprecation: Vec<(Uuid, String)> = repository
            .list_model_deprecation_recipients(&old, since)
            .await
            .unwrap()
            .into_iter()
            .map(|recipient| (recipient.user_id, recipient.email))
            .collect();
        let pricing: Vec<(Uuid, String)> = repository
            .list_pricing_change_recipients(std::slice::from_ref(&old), since)
            .await
            .unwrap()
            .into_iter()
            .map(|recipient| (recipient.user_id, recipient.email))
            .collect();
        (deprecation, pricing)
    };

    // The wallet user has only a placeholder address: not a recipient.
    let only_email_user = vec![(email_user, login_address.clone())];
    let before = preview().await;
    assert_eq!(before["recipientCount"], 1, "{before}");
    assert_eq!(before["organizationCount"], 1, "{before}");
    assert_eq!(
        recipients().await,
        (only_email_user.clone(), only_email_user)
    );

    // Once the contact email is verified, it is the address used.
    let address = new_address();
    add_contact_email(&server, &sender, &wallet_session, &account_id, &address).await;
    let after = preview().await;
    assert_eq!(after["recipientCount"], 2, "{after}");
    assert_eq!(after["organizationCount"], 2, "{after}");
    let (mut deprecation, mut pricing) = recipients().await;
    let mut expected = vec![(email_user, login_address), (wallet_user, address)];
    deprecation.sort();
    pricing.sort();
    expected.sort();
    assert_eq!(deprecation, expected);
    assert_eq!(pricing, expected);
}

#[tokio::test]
async fn admin_user_listing_includes_the_contact_email_fields() {
    let (server, database, sender) = setup().await;
    let auth = real_auth(&database);
    let account_id = new_account_id();
    let (session, user_id) = near_login(&auth, &account_id).await;

    let listed_by = |search: String, include_organizations: bool| {
        let server = &server;
        async move {
            let response = server
                .get("/v1/admin/users")
                .add_query_param("search", &search)
                .add_query_param("include_organizations", include_organizations)
                .add_header("Authorization", format!("Bearer {}", get_session_id()))
                .add_header("User-Agent", MOCK_USER_AGENT)
                .await;
            assert_eq!(response.status_code(), 200, "{}", response.text());
            let body = response.json::<serde_json::Value>();
            let users = body["users"].as_array().unwrap();
            assert_eq!(users.len(), 1, "{body}");
            assert_eq!(users[0]["id"], user_id.to_string());
            users[0].clone()
        }
    };
    let listed = |include_organizations: bool| listed_by(account_id.clone(), include_organizations);

    // An address without its verification time is not a verified address: it
    // is shown neither to admins nor to the user. (The two columns are always
    // written together; this row is made by hand.)
    let client = database.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE users SET contact_email = $2 WHERE id = $1",
            &[&user_id, &new_address()],
        )
        .await
        .unwrap();
    assert!(me(&server, &session).await["contact_email"].is_null());

    // The keys are always present, null until an address is verified.
    for include_organizations in [false, true] {
        let user = listed(include_organizations).await;
        let fields = user.as_object().unwrap();
        for key in [
            "contact_email",
            "contact_email_verified_at",
            "email_opt_in_at",
        ] {
            assert!(
                fields.get(key).is_some_and(|value| value.is_null()),
                "{user}"
            );
        }
    }

    drop(client);

    let address = new_address();
    add_contact_email(&server, &sender, &session, &account_id, &address).await;

    for include_organizations in [false, true] {
        let user = listed(include_organizations).await;
        assert_eq!(user["email"], near_placeholder_email(&account_id));
        assert_eq!(user["contact_email"], address, "{user}");
        assert!(user["contact_email_verified_at"].is_string(), "{user}");
        assert!(user["email_opt_in_at"].is_string(), "{user}");

        // Support can find the wallet user by the contact address, which is the
        // only real address held for them. Partial and in any letter case.
        let found = listed_by(address.to_uppercase(), include_organizations).await;
        assert_eq!(found["contact_email"], address, "{found}");
        let local_part = address.split('@').next().unwrap().to_string();
        let found = listed_by(local_part, include_organizations).await;
        assert_eq!(found["contact_email"], address, "{found}");
    }

    // The organization member listing for admins carries the same user fields.
    let org_id = personal_org_id(&server, &session).await;
    let response = server
        .get(format!("/v1/admin/organizations/{org_id}/members").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let members = response.json::<serde_json::Value>();
    assert_eq!(members["members"][0]["user"]["contact_email"], address);
    assert!(members["members"][0]["user"]["email_opt_in_at"].is_string());
}

#[test]
fn openapi_documents_the_contact_email_routes_and_fields() {
    use utoipa::OpenApi;

    let spec = serde_json::to_value(api::openapi::ApiDoc::openapi()).unwrap();

    assert!(spec["paths"]["/v1/users/me/contact-email"]["post"].is_object());
    assert!(spec["paths"]["/v1/users/me/contact-email"]["delete"].is_object());
    assert!(spec["paths"]["/v1/users/contact-email/verify"]["post"].is_object());
    let schemas = &spec["components"]["schemas"];
    for (schema, fields) in [
        (
            "UserResponse",
            [
                "contact_email",
                "contact_email_verified",
                "needs_contact_email",
            ],
        ),
        (
            "AdminUserResponse",
            [
                "contact_email",
                "contact_email_verified_at",
                "email_opt_in_at",
            ],
        ),
    ] {
        for field in fields {
            assert!(
                schemas[schema]["properties"][field].is_object(),
                "{schema}.{field} is missing from the OpenAPI document"
            );
        }
    }
    for schema in [
        "RequestContactEmailRequest",
        "VerifyContactEmailRequest",
        "ContactEmailStatusResponse",
    ] {
        assert!(schemas[schema].is_object(), "{schema} is not exported");
    }
}
