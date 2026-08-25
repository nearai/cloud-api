use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Encrypt,
    Remove,
    ApprovedPlaintext,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Policy {
    pub classification: Classification,
    pub reason: &'static str,
}

const fn encrypt(reason: &'static str) -> Policy {
    Policy {
        classification: Classification::Encrypt,
        reason,
    }
}

const fn approved(reason: &'static str) -> Policy {
    Policy {
        classification: Classification::ApprovedPlaintext,
        reason,
    }
}

const HASH: &str = "One-way digest or non-secret prefix required for indexed lookup";
const ENUM: &str = "Bounded operational enum required for filtering and constraints";
const ID: &str = "Operational identifier required for indexed lookup, routing, or audit";
const DISPLAY: &str = "Queryable display metadata approved pending searchable-encryption support";
const CONFIG: &str = "Queryable operational configuration required by the service";
const AUDIT: &str = "Restricted operator audit metadata; must not contain customer payloads";
const ERROR: &str = "Redacted operational error; producers must not include customer payloads";

/// Explicit policy for every application-owned text, varchar, char, JSON, and JSONB column.
/// Unknown columns fail the worker's inventory gate.
pub fn policy_for(table: &str, column: &str) -> Option<Policy> {
    let policy = match (table, column) {
        ("response_items", "item") => encrypt("Persisted customer transcript and tool payload"),
        ("responses", "instructions") => encrypt("Customer prompt instructions"),
        ("responses", "metadata") => encrypt("Customer response metadata"),
        ("conversations", "metadata") => encrypt("Customer conversation title and metadata"),
        ("files", "filename" | "content_type") => encrypt("Customer file metadata"),
        ("files", "storage_key") => encrypt("Private object pointer"),
        ("mcp_connectors", "description" | "mcp_server_url" | "auth_config" | "error_message" | "capabilities" | "metadata") => encrypt("Customer MCP configuration or payload"),
        ("mcp_connector_usage", "request_payload" | "response_payload" | "error_message") => encrypt("Customer MCP request, response, or error"),

        ("admin_access_token", "token_hash") | ("api_keys", "key_hash" | "key_prefix")
        | ("organization_reporting_tokens", "token_hash" | "token_prefix")
        | ("refresh_tokens", "token_hash") => approved(HASH),
        ("admin_access_token", "name" | "creation_reason" | "revocation_reason" | "user_agent")
        | ("aml_allowlisted_accounts", "reason")
        | ("organization_limits_history", "changed_by" | "change_reason" | "changed_by_user_email")
        | ("model_history", "change_reason" | "changed_by_user_email")
        | ("model_deprecation_email_deliveries", "initiated_by_user_email")
        | ("model_pricing_change_email_deliveries", "initiated_by_user_email")
        | ("scheduled_model_pricing_changes", "cancelled_by_user_email" | "created_by_user_email" | "change_reason") => approved(AUDIT),
        ("aml_allowlisted_accounts", "account_id")
        | ("aml_reports", "account_id" | "report_id")
        | ("chat_signatures", "chat_id")
        | ("model_aliases", "alias_name")
        | ("model_deprecation_email_deliveries", "model_name" | "successor_model_name" | "email_message_id")
        | ("model_pricing_change_email_deliveries", "email_message_id")
        | ("model_history", "model_name" | "hugging_face_id" | "openrouter_slug")
        | ("models", "model_name" | "hugging_face_id" | "openrouter_slug")
        | ("organization_staking_farm_sources", "near_account_id" | "network_id" | "contract_id" | "farm_product_id" | "farm_price_id")
        | ("organization_usage_log", "model_name" | "provider_request_id")
        | ("scheduled_model_pricing_changes", "model_name")
        | ("services", "service_name") => approved(ID),
        ("aml_allowlisted_accounts", "address_type")
        | ("aml_reports", "flow" | "provider" | "address_type" | "risk_level")
        | ("chat_signatures", "signing_algo" | "signature_kind")
        | ("feature_request_targets", "kind" | "status")
        | ("feature_request_votes", "source")
        | ("files", "purpose")
        | ("mcp_connector_usage", "method")
        | ("mcp_connectors", "auth_type" | "connection_status")
        | ("model_deprecation_email_deliveries", "status")
        | ("model_pricing_change_email_deliveries", "status")
        | ("model_history", "provider_type" | "quantization" | "attestation_policy")
        | ("models", "provider_type" | "quantization" | "attestation_policy")
        | ("oauth_states", "provider")
        | ("organization_invitations", "role" | "status" | "email_status")
        | ("organization_limits_history", "credit_type" | "source" | "currency")
        | ("organization_members", "role")
        | ("organization_staking_farm_sources", "status" | "sync_status")
        | ("organization_usage_log", "request_type" | "inference_type" | "stop_reason" | "served_provider_tier" | "served_provider_type" | "service_tier" | "context_band")
        | ("responses", "status")
        | ("scheduled_model_pricing_changes", "status")
        | ("services", "unit")
        | ("users", "auth_provider") => approved(ENUM),
        ("feature_request_targets", "key") => approved(ID),
        ("feature_request_targets", "title")
        | ("feature_request_votes", "note")
        | ("mcp_connectors", "name")
        | ("model_deprecation_email_deliveries", "model_display_name" | "organization_name")
        | ("model_pricing_change_email_deliveries", "organization_name")
        | ("model_history", "model_display_name" | "model_description" | "owned_by")
        | ("models", "model_display_name" | "model_description" | "owned_by")
        | ("organizations", "name" | "description")
        | ("scheduled_model_pricing_changes", "model_display_name")
        | ("services", "display_name" | "description")
        | ("users", "display_name" | "avatar_url")
        | ("workspaces", "name" | "description")
        | ("api_keys", "name")
        | ("organization_reporting_tokens", "name") => approved(DISPLAY),
        ("model_history", "model_icon" | "provider_config" | "input_modalities" | "output_modalities" | "inference_url" | "text_pricing")
        | ("models", "model_icon" | "provider_config" | "input_modalities" | "output_modalities" | "inference_url" | "text_pricing")
        | ("organizations", "settings")
        | ("scheduled_model_pricing_changes", "old_text_pricing" | "new_text_pricing")
        | ("workspaces", "settings") => approved(CONFIG),
        ("aml_reports", "reason")
        | ("model_deprecation_email_deliveries", "email_last_error")
        | ("model_pricing_change_email_deliveries", "email_last_error")
        | ("organization_invitations", "email_last_error")
        | ("organization_staking_farm_sources", "last_sync_error")
        | ("scheduled_model_pricing_changes", "last_error") => approved(ERROR),
        ("aml_reports", "result_json") => approved("Restricted compliance result required for enforcement and audit"),
        ("chat_signatures", "text") => approved("Cryptographic attestation statement returned to its tenant"),
        ("chat_signatures", "signature" | "signing_address") => approved("Public cryptographic verification material"),
        ("database_encryption_jobs", "mode" | "status" | "scope" | "actions" | "cursor" | "progress" | "last_error_class" | "last_error_message" | "operator") => approved("Encryption-worker control state containing identifiers, counters, and redacted errors only"),
        ("model_deprecation_email_deliveries", "recipient_email")
        | ("model_pricing_change_email_deliveries", "recipient_email") => approved("Delivery address required for notification audit"),
        ("near_used_nonces", "nonce_hex") => approved("Public replay-prevention nonce"),
        ("oauth_states", "state") => approved("Short-lived random correlation token required for indexed OAuth lookup"),
        ("oauth_states", "pkce_verifier" | "frontend_callback") => approved("Short-lived OAuth protocol value; expired state rows are deleted"),
        ("organization_invitations", "email") => approved("Invitation identity required for indexed lookup and delivery"),
        ("organization_invitations", "token") => approved("Short-lived bearer token required for indexed invitation acceptance; hash migration is required separately"),
        ("organization_invitations", "email_message_id") => approved(ID),
        ("organization_staking_farm_sources", "active_positions") => approved("Queryable staking accounting state"),
        ("organization_usage_log", "billing_details") => approved("Queryable billing ledger inputs without prompt or response content"),
        ("refresh_tokens", "ip_address" | "user_agent") => approved("Restricted account-security audit attribute"),
        ("responses", "model") => approved(ID),
        ("responses", "usage") => approved("Numeric token accounting without customer content"),
        ("responses", "next_response_ids") => approved("Response graph identifiers"),
        ("users", "email") => approved("Queryable login identity protected by account access controls"),
        ("users", "username") => approved("Queryable unique account identity"),
        ("users", "provider_user_id") => approved(ID),
        _ => return None,
    };
    Some(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_examples_have_explicit_policies() {
        for (table, column) in [
            ("oauth_states", "pkce_verifier"),
            ("organization_invitations", "token"),
            ("users", "email"),
            ("organizations", "settings"),
            ("workspaces", "settings"),
        ] {
            let policy = policy_for(table, column).expect("field must be classified");
            assert!(!policy.reason.is_empty());
        }
    }
}
