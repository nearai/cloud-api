use chrono::{DateTime, Utc};

/// Domain of the placeholder login address stored for NEAR wallet users. A wallet
/// has no email, and `users.email` is required and unique, so the login identity
/// is `{account_id}@near`. Nothing can be delivered to it.
const NEAR_PLACEHOLDER_EMAIL_DOMAIN: &str = "near";

/// The placeholder login address stored for a NEAR wallet account.
pub fn near_placeholder_email(account_id: &str) -> String {
    format!("{account_id}@{NEAR_PLACEHOLDER_EMAIL_DOMAIN}")
}

/// The wallet account id behind a placeholder login address, or `None` when the
/// address is not a placeholder.
pub fn placeholder_account_id(email: &str) -> Option<&str> {
    let (account_id, domain) = email.trim().rsplit_once('@')?;
    (!account_id.is_empty() && domain.eq_ignore_ascii_case(NEAR_PLACEHOLDER_EMAIL_DOMAIN))
        .then_some(account_id)
}

/// Whether `email` is a placeholder login address rather than a real mailbox.
/// Every email flow must use this (through [`deliverable_email`]) instead of
/// matching on the address itself.
pub fn is_placeholder_email(email: &str) -> bool {
    placeholder_account_id(email).is_some()
}

/// The address a user can be reached at: the verified contact email when there is
/// one, else the login email unless it is a placeholder, else `None`. A contact
/// email without a verification timestamp is never used.
pub fn deliverable_email<'a>(
    login_email: &'a str,
    contact_email: Option<&'a str>,
    contact_email_verified_at: Option<DateTime<Utc>>,
) -> Option<&'a str> {
    match (contact_email, contact_email_verified_at) {
        (Some(contact_email), Some(_)) => Some(contact_email),
        _ => (!is_placeholder_email(login_email)).then_some(login_email),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn placeholder_detection_matches_only_wallet_login_addresses() {
        assert!(is_placeholder_email("alice.near@near"));
        assert!(is_placeholder_email(&near_placeholder_email("alice.near")));
        assert!(is_placeholder_email(&near_placeholder_email(
            "98793cd91a3f870fb126f66285808c7e094afcfc4eda8a970f6648cdf0dbd6de"
        )));
        assert!(is_placeholder_email("alice.testnet@NEAR"));
        assert!(is_placeholder_email(" alice.near@near "));
        assert_eq!(
            placeholder_account_id("alice.near@near"),
            Some("alice.near")
        );

        // Real mailboxes, including ones that merely mention "near".
        assert!(!is_placeholder_email("alice@example.com"));
        assert!(!is_placeholder_email("alice@near.org"));
        assert!(!is_placeholder_email("alice@near.ai"));
        assert!(!is_placeholder_email("near@example.com"));
        assert!(!is_placeholder_email("alice@mail.near"));
        // Not addresses at all.
        assert!(!is_placeholder_email("@near"));
        assert!(!is_placeholder_email("alice.near"));
        assert!(!is_placeholder_email(""));
        assert_eq!(placeholder_account_id("alice@example.com"), None);
    }

    #[test]
    fn deliverable_email_prefers_verified_contact_email() {
        let verified_at = Some(Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap());

        // Wallet user with a verified contact email.
        assert_eq!(
            deliverable_email("alice.near@near", Some("alice@example.com"), verified_at),
            Some("alice@example.com")
        );
        // Wallet user with nothing verified: no deliverable address.
        assert_eq!(deliverable_email("alice.near@near", None, None), None);
        // A contact email with no verification timestamp is ignored.
        assert_eq!(
            deliverable_email("alice.near@near", Some("alice@example.com"), None),
            None
        );
        // A verification timestamp with no address changes nothing.
        assert_eq!(
            deliverable_email("alice.near@near", None, verified_at),
            None
        );
        // A real login email is deliverable as it is.
        assert_eq!(
            deliverable_email("bob@example.com", None, None),
            Some("bob@example.com")
        );
        // A verified contact email wins over a real login email.
        assert_eq!(
            deliverable_email("bob@example.com", Some("bob@work.example"), verified_at),
            Some("bob@work.example")
        );
        assert_eq!(
            deliverable_email("bob@example.com", Some("bob@work.example"), None),
            Some("bob@example.com")
        );
    }
}
