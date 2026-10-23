use super::models::*;
use super::ports::{MockCreditTransferRepositoryPort, MockOrganizationCredentialRepositoryPort};
use super::{OrganizationCreditService, OrganizationCreditServiceTrait};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

#[tokio::test]
async fn invalid_transfer_never_reaches_persistence() {
    let service = OrganizationCreditService::new(
        Arc::new(MockCreditTransferRepositoryPort::new()),
        Arc::new(MockOrganizationCredentialRepositoryPort::new()),
    );
    let org = Uuid::new_v4();
    let request = TransferRequest {
        recipient_organization_id: org,
        source_credit_type: "payment".into(),
        amount: Some(10),
        recipient_cumulative: None,
        payout_program: None,
        memo: String::new(),
        metadata: Default::default(),
    };

    let error = service
        .transfer(org, &TransferActor::Owner(Uuid::new_v4()), "key", &request)
        .await
        .unwrap_err();

    assert_eq!(
        error.downcast_ref::<TransferError>().unwrap().0,
        "invalid_transfer_terms"
    );
}

#[tokio::test]
async fn valid_send_preserves_atomic_adapter_result() {
    let mut transfers = MockCreditTransferRepositoryPort::new();
    transfers
        .expect_transfer()
        .times(1)
        .returning(|_, _, _, _| Ok((json!({"id": "committed", "sender_snapshot": {}}), true)));
    let service = OrganizationCreditService::new(
        Arc::new(transfers),
        Arc::new(MockOrganizationCredentialRepositoryPort::new()),
    );
    let request = TransferRequest {
        recipient_organization_id: Uuid::new_v4(),
        source_credit_type: "payment".into(),
        amount: Some(10),
        recipient_cumulative: None,
        payout_program: None,
        memo: String::new(),
        metadata: Default::default(),
    };

    let (result, replay) = service
        .transfer(
            Uuid::new_v4(),
            &TransferActor::Owner(Uuid::new_v4()),
            "key",
            &request,
        )
        .await
        .unwrap();

    assert!(replay);
    assert_eq!(result["id"], "committed");
}

#[tokio::test]
async fn invalid_policy_and_credential_never_reach_persistence() {
    let service = OrganizationCreditService::new(
        Arc::new(MockCreditTransferRepositoryPort::new()),
        Arc::new(MockOrganizationCredentialRepositoryPort::new()),
    );
    let org = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let policy = OrganizationPolicy::default();
    let credential: CredentialRequest = serde_json::from_value(json!({
        "name": "payout", "permissions": ["admin"],
        "expires_at": chrono::Utc::now() + chrono::Duration::hours(1)
    }))
    .unwrap();

    let policy_error = service
        .configure_policy(org, Some((policy, owner, String::new())))
        .await
        .unwrap_err();
    let credential_error = service
        .save_credential(org, owner, None, &credential)
        .await
        .unwrap_err();

    assert_eq!(
        policy_error.downcast_ref::<TransferError>().unwrap().0,
        "invalid_policy_configuration"
    );
    assert_eq!(
        credential_error.downcast_ref::<TransferError>().unwrap().0,
        "invalid_credential_configuration"
    );
}

#[test]
fn constraints_apply_source_recipient_and_lifetime_budget() {
    let recipient = Uuid::new_v4();
    let constraints = TransferConstraints {
        max_amount_per_transfer: Some(20),
        max_total_amount: Some(50),
        allowed_source_types: Some(vec!["payment".into()]),
        allowed_recipient_ids: Some(vec![recipient]),
    };

    assert!(constraints.allows(10, "payment", recipient, 40));
    assert!(!constraints.allows(11, "payment", recipient, 40));
    assert!(!constraints.allows(21, "payment", recipient, 0));
    assert!(!constraints.allows(10, "grant", recipient, 0));
    assert!(!constraints.allows(10, "payment", Uuid::new_v4(), 0));
}

#[test]
fn send_policy_requires_enablement_and_explicit_grant_opt_in() {
    let recipient = Uuid::new_v4();
    let authorization = Authorization {
        actor_id: Uuid::new_v4(),
        credential_id: None,
        permissions: vec![TRANSFER_CREDITS.into()],
        constraints: TransferConstraints::default(),
    };
    let request = TransferRequest {
        recipient_organization_id: recipient,
        source_credit_type: "grant".into(),
        amount: Some(10),
        recipient_cumulative: None,
        payout_program: None,
        memo: String::new(),
        metadata: Default::default(),
    };
    let mut policy = TransferPolicy::default();

    let disabled = check_send_policy(&policy, &authorization, &request).unwrap_err();
    assert_eq!(
        disabled.downcast_ref::<TransferError>().unwrap().0,
        "sender_not_enabled"
    );

    policy.enabled = true;
    let default_grant = check_send_policy(&policy, &authorization, &request).unwrap_err();
    assert_eq!(
        default_grant.downcast_ref::<TransferError>().unwrap().0,
        "organization_transfer_limit_exceeded"
    );

    policy.constraints.allowed_source_types = Some(vec!["grant".into()]);
    assert!(check_send_policy(&policy, &authorization, &request).is_ok());
}
