use async_trait::async_trait;
use chrono::{DateTime, Utc};
use config::InvitationEmailConfig;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

const RESEND_EMAILS_URL: &str = "https://api.resend.com/emails";
const RESEND_SEND_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct InvitationEmail {
    pub recipient_email: String,
    pub organization_name: String,
    pub role: String,
    pub inviter_name: Option<String>,
    pub inviter_email: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub invitations_url: String,
}

#[derive(Debug, Clone)]
pub struct ModelDeprecationEmail {
    pub recipient_email: String,
    pub model_id: String,
    pub model_display_name: String,
    pub deprecation_date: DateTime<Utc>,
    pub successor_model_id: String,
}

/// One model's entry in a consolidated pricing change email.
/// Amounts are nano-dollars (scale 9); `new_*` is `None` when unchanged.
#[derive(Debug, Clone)]
pub struct PricingChangeEmailModel {
    pub model_id: String,
    pub model_display_name: String,
    pub effective_at: DateTime<Utc>,
    pub old_input_cost_per_token: i64,
    pub new_input_cost_per_token: Option<i64>,
    pub old_output_cost_per_token: i64,
    pub new_output_cost_per_token: Option<i64>,
    pub old_cache_read_cost_per_token: i64,
    pub new_cache_read_cost_per_token: Option<i64>,
    pub old_cost_per_image: i64,
    pub new_cost_per_image: Option<i64>,
}

/// Consolidated pricing change notification: one email per recipient
/// covering every affected model their organization(s) used.
#[derive(Debug, Clone)]
pub struct PricingChangeEmail {
    pub recipient_email: String,
    pub models: Vec<PricingChangeEmailModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailDeliveryOutcome {
    Sent { message_id: Option<String> },
    Skipped,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct EmailError {
    message: String,
}

impl EmailError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn sanitized_message(&self) -> String {
        sanitize_error(&self.message)
    }
}

#[async_trait]
pub trait EmailSender: Send + Sync {
    async fn send_invitation(
        &self,
        email: &InvitationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError>;

    async fn send_model_deprecation(
        &self,
        email: &ModelDeprecationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError>;

    async fn send_pricing_change(
        &self,
        email: &PricingChangeEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError>;
}

pub struct NoopEmailSender;

#[async_trait]
impl EmailSender for NoopEmailSender {
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
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ResendSendEmailRequest {
    from: String,
    to: Vec<String>,
    subject: String,
    html: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ResendSendEmailResponse {
    id: String,
}

#[async_trait]
trait ResendTransport: Send + Sync {
    async fn send_email(
        &self,
        api_key: &str,
        request: ResendSendEmailRequest,
    ) -> Result<ResendSendEmailResponse, EmailError>;
}

#[derive(Clone)]
struct ReqwestResendTransport {
    client: reqwest::Client,
    endpoint: String,
}

impl Default for ReqwestResendTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: RESEND_EMAILS_URL.to_string(),
        }
    }
}

#[async_trait]
impl ResendTransport for ReqwestResendTransport {
    async fn send_email(
        &self,
        api_key: &str,
        request: ResendSendEmailRequest,
    ) -> Result<ResendSendEmailResponse, EmailError> {
        let response = tokio::time::timeout(
            RESEND_SEND_TIMEOUT,
            self.client
                .post(&self.endpoint)
                .bearer_auth(api_key)
                .json(&request)
                .send(),
        )
        .await
        .map_err(|_| {
            EmailError::new(format!(
                "Resend send_email timed out after {} seconds",
                RESEND_SEND_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|err| EmailError::new(format!("Resend send_email request failed: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(EmailError::new(resend_error_message(status, &body)));
        }

        response
            .json::<ResendSendEmailResponse>()
            .await
            .map_err(|err| EmailError::new(format!("Failed to parse Resend response: {err}")))
    }
}

#[derive(Clone)]
pub struct ResendEmailSender {
    from_email: String,
    reply_to: Option<String>,
    api_key: String,
    transport: Arc<dyn ResendTransport>,
}

impl ResendEmailSender {
    pub fn new(config: &InvitationEmailConfig) -> Result<Self, EmailError> {
        let from_email = config.from_email.clone().ok_or_else(|| {
            EmailError::new("INVITATION_EMAIL_FROM is required for Resend invitation email sending")
        })?;
        let api_key = config.resend_api_key.clone().ok_or_else(|| {
            EmailError::new("RESEND_API_KEY is required for Resend invitation email sending")
        })?;

        Ok(Self::new_with_transport(
            from_email,
            config.reply_to.clone(),
            api_key,
            Arc::new(ReqwestResendTransport::default()),
        ))
    }

    fn new_with_transport(
        from_email: String,
        reply_to: Option<String>,
        api_key: String,
        transport: Arc<dyn ResendTransport>,
    ) -> Self {
        Self {
            from_email,
            reply_to,
            api_key,
            transport,
        }
    }
}

#[async_trait]
impl EmailSender for ResendEmailSender {
    async fn send_invitation(
        &self,
        email: &InvitationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        let rendered = render_invitation_email(email);
        let request = ResendSendEmailRequest {
            from: self.from_email.clone(),
            to: vec![email.recipient_email.clone()],
            subject: rendered.subject,
            html: rendered.html_body,
            text: rendered.text_body,
            reply_to: self.reply_to.clone(),
        };
        let response = self.transport.send_email(&self.api_key, request).await?;

        Ok(EmailDeliveryOutcome::Sent {
            message_id: Some(response.id),
        })
    }

    async fn send_model_deprecation(
        &self,
        email: &ModelDeprecationEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        let rendered = render_model_deprecation_email(email);
        let request = ResendSendEmailRequest {
            from: self.from_email.clone(),
            to: vec![email.recipient_email.clone()],
            subject: rendered.subject,
            html: rendered.html_body,
            text: rendered.text_body,
            reply_to: self.reply_to.clone(),
        };
        let response = self.transport.send_email(&self.api_key, request).await?;

        Ok(EmailDeliveryOutcome::Sent {
            message_id: Some(response.id),
        })
    }

    async fn send_pricing_change(
        &self,
        email: &PricingChangeEmail,
    ) -> Result<EmailDeliveryOutcome, EmailError> {
        let rendered = render_pricing_change_email(email);
        let request = ResendSendEmailRequest {
            from: self.from_email.clone(),
            to: vec![email.recipient_email.clone()],
            subject: rendered.subject,
            html: rendered.html_body,
            text: rendered.text_body,
            reply_to: self.reply_to.clone(),
        };
        let response = self.transport.send_email(&self.api_key, request).await?;

        Ok(EmailDeliveryOutcome::Sent {
            message_id: Some(response.id),
        })
    }
}

pub fn sender_from_config(
    config: &InvitationEmailConfig,
) -> Result<Arc<dyn EmailSender>, EmailError> {
    if config.enabled {
        Ok(Arc::new(ResendEmailSender::new(config)?))
    } else {
        Ok(Arc::new(NoopEmailSender))
    }
}

fn resend_error_message(status: reqwest::StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .or_else(|| value.get("error"))
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
        })
        .or_else(|| {
            let body = body.trim();
            if body.is_empty() {
                None
            } else {
                Some(body.to_string())
            }
        })
        .unwrap_or_else(|| "empty response body".to_string());

    format!("Resend send_email failed with status {status}: {detail}")
}

#[derive(Debug, Clone)]
pub struct RenderedEmail {
    pub subject: String,
    pub html_body: String,
    pub text_body: String,
}

pub fn render_invitation_email(email: &InvitationEmail) -> RenderedEmail {
    let organization_name = email.organization_name.trim();
    let organization_name = if organization_name.is_empty() {
        "an organization"
    } else {
        organization_name
    };
    let role = email.role.trim();
    let role = if role.is_empty() { "member" } else { role };
    let inviter = invitation_sender_name(email);
    let expires_at = email.expires_at.format("%B %-d, %Y at %H:%M UTC");
    let subject = format!("You’ve been invited to join {organization_name} on NEAR AI Cloud");

    let html_body = format!(
        r#"<!doctype html>
<html lang="en">
  <body style="font-family:Arial,sans-serif;line-height:1.5;color:#111827;">
    <h1 style="font-size:20px;margin-bottom:16px;">You’ve been invited to NEAR AI Cloud</h1>
    <p>{inviter} invited you to join <strong>{organization}</strong> as <strong>{role}</strong>.</p>
    <p>This invitation expires on {expires_at}.</p>
    <p style="margin:24px 0;">
      <a href="{url}" style="background:#111827;color:#ffffff;padding:12px 18px;text-decoration:none;border-radius:6px;display:inline-block;">View invitation</a>
    </p>
    <p>If the button does not work, open this link:</p>
    <p><a href="{url}">{url}</a></p>
  </body>
</html>"#,
        inviter = escape_html(&inviter),
        organization = escape_html(organization_name),
        role = escape_html(role),
        expires_at = escape_html(&expires_at.to_string()),
        url = escape_html(&email.invitations_url),
    );

    let text_body = format!(
        "You’ve been invited to NEAR AI Cloud\n\n{inviter} invited you to join {organization_name} as {role}.\n\nThis invitation expires on {expires_at}.\n\nView invitation: {url}\n",
        url = email.invitations_url,
    );

    RenderedEmail {
        subject,
        html_body,
        text_body,
    }
}

pub fn render_model_deprecation_email(email: &ModelDeprecationEmail) -> RenderedEmail {
    let model_display_name = email.model_display_name.trim();
    let model_label = if model_display_name.is_empty() {
        email.model_id.as_str()
    } else {
        model_display_name
    };
    let deprecation_date = email.deprecation_date.format("%B %-d, %Y at %H:%M UTC");
    let subject = format!("NEAR AI Cloud model deprecation: {}", email.model_id);

    let html_body = format!(
        r#"<!doctype html>
<html lang="en">
  <body style="font-family:Arial,sans-serif;line-height:1.5;color:#111827;">
    <h1 style="font-size:20px;margin-bottom:16px;">Model deprecation notice</h1>
    <p>The NEAR AI Cloud model <strong>{model_label}</strong> (<code>{model_id}</code>) is scheduled for deprecation on <strong>{deprecation_date}</strong>.</p>
    <p>We recommend migrating affected workloads to <strong>{successor}</strong> before that date.</p>
    <p>No traffic has been rerouted automatically. Existing calls continue to use the current model until it is removed in a later step.</p>
  </body>
</html>"#,
        model_label = escape_html(model_label),
        model_id = escape_html(&email.model_id),
        deprecation_date = escape_html(&deprecation_date.to_string()),
        successor = escape_html(&email.successor_model_id),
    );

    let text_body = format!(
        "NEAR AI Cloud model deprecation notice\n\nThe model {model_label} ({model_id}) is scheduled for deprecation on {deprecation_date}.\n\nWe recommend migrating affected workloads to {successor} before that date.\n\nNo traffic has been rerouted automatically. Existing calls continue to use the current model until it is removed in a later step.\n",
        model_id = email.model_id,
        successor = email.successor_model_id,
    );

    RenderedEmail {
        subject,
        html_body,
        text_body,
    }
}

/// Format a nano-dollar-per-token amount as USD per 1M tokens.
fn format_usd_per_million_tokens(nano_dollars_per_token: i64) -> String {
    format_usd(nano_dollars_per_token as f64 / 1_000.0)
}

/// Format a nano-dollar amount as USD.
fn format_usd_per_image(nano_dollars: i64) -> String {
    format_usd(nano_dollars as f64 / 1_000_000_000.0)
}

fn format_usd(dollars: f64) -> String {
    let formatted = format!("{dollars:.6}");
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    // Always keep at least two decimals for readability ("$5" -> "$5.00").
    if trimmed.split('.').nth(1).map_or(0, str::len) < 2 {
        format!("${dollars:.2}")
    } else {
        format!("${trimmed}")
    }
}

fn pricing_change_lines(model: &PricingChangeEmailModel) -> Vec<String> {
    let mut lines = Vec::new();
    let mut per_million = |label: &str, old: i64, new: Option<i64>| {
        if let Some(new) = new {
            lines.push(format!(
                "{label}: {} → {} per 1M tokens",
                format_usd_per_million_tokens(old),
                format_usd_per_million_tokens(new),
            ));
        }
    };
    per_million(
        "Input tokens",
        model.old_input_cost_per_token,
        model.new_input_cost_per_token,
    );
    per_million(
        "Output tokens",
        model.old_output_cost_per_token,
        model.new_output_cost_per_token,
    );
    per_million(
        "Cache reads",
        model.old_cache_read_cost_per_token,
        model.new_cache_read_cost_per_token,
    );
    if let Some(new) = model.new_cost_per_image {
        lines.push(format!(
            "Images: {} → {} per image",
            format_usd_per_image(model.old_cost_per_image),
            format_usd_per_image(new),
        ));
    }
    lines
}

pub fn render_pricing_change_email(email: &PricingChangeEmail) -> RenderedEmail {
    let subject = match email.models.as_slice() {
        [only] => format!("NEAR AI Cloud pricing update: {}", only.model_id),
        models => format!("NEAR AI Cloud pricing update for {} models", models.len()),
    };
    let intro = if email.models.len() == 1 {
        "The pricing of the following NEAR AI Cloud model your organization recently used will change."
    } else {
        "The pricing of the following NEAR AI Cloud models your organization recently used will change."
    };

    let mut html_sections = String::new();
    let mut text_sections = String::new();
    for model in &email.models {
        let model_display_name = model.model_display_name.trim();
        let model_label = if model_display_name.is_empty() {
            model.model_id.as_str()
        } else {
            model_display_name
        };
        let effective_at = model.effective_at.format("%B %-d, %Y at %H:%M UTC");
        let lines = pricing_change_lines(model);

        let html_lines = lines
            .iter()
            .map(|line| format!("      <li>{}</li>\n", escape_html(line)))
            .collect::<String>();
        html_sections.push_str(&format!(
            r#"    <h2 style="font-size:16px;margin:24px 0 4px;">{model_label} (<code>{model_id}</code>)</h2>
    <p style="margin:4px 0;">Effective <strong>{effective_at}</strong>:</p>
    <ul style="margin:4px 0 16px;">
{html_lines}    </ul>
"#,
            model_label = escape_html(model_label),
            model_id = escape_html(&model.model_id),
            effective_at = escape_html(&effective_at.to_string()),
        ));

        text_sections.push_str(&format!(
            "{model_label} ({model_id})\nEffective {effective_at}:\n",
            model_id = model.model_id,
        ));
        for line in &lines {
            text_sections.push_str(&format!("- {line}\n"));
        }
        text_sections.push('\n');
    }

    let html_body = format!(
        r#"<!doctype html>
<html lang="en">
  <body style="font-family:Arial,sans-serif;line-height:1.5;color:#111827;">
    <h1 style="font-size:20px;margin-bottom:16px;">Pricing update notice</h1>
    <p>{intro}</p>
{html_sections}    <p>No action is required; your API usage will be billed at the new rates from each effective time.</p>
  </body>
</html>"#,
    );

    let text_body = format!(
        "NEAR AI Cloud pricing update notice\n\n{intro}\n\n{text_sections}No action is required; your API usage will be billed at the new rates from each effective time.\n",
    );

    RenderedEmail {
        subject,
        html_body,
        text_body,
    }
}

fn invitation_sender_name(email: &InvitationEmail) -> String {
    if let Some(name) = email
        .inviter_name
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return name.to_string();
    }

    if let Some(email) = email
        .inviter_email
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return email.to_string();
    }

    "A team admin".to_string()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn sanitize_error(message: &str) -> String {
    const MAX_LEN: usize = 1000;
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() <= MAX_LEN {
        return collapsed;
    }

    let mut truncated = collapsed.chars().take(MAX_LEN).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Mutex;

    struct StubResendTransport {
        outcome: Result<ResendSendEmailResponse, EmailError>,
        requests: Mutex<Vec<(String, ResendSendEmailRequest)>>,
    }

    #[async_trait]
    impl ResendTransport for StubResendTransport {
        async fn send_email(
            &self,
            api_key: &str,
            request: ResendSendEmailRequest,
        ) -> Result<ResendSendEmailResponse, EmailError> {
            self.requests
                .lock()
                .unwrap()
                .push((api_key.to_string(), request));
            self.outcome.clone()
        }
    }

    fn example_invitation_email() -> InvitationEmail {
        InvitationEmail {
            recipient_email: "invitee@example.com".to_string(),
            organization_name: "Example Org".to_string(),
            role: "admin".to_string(),
            inviter_name: Some("Alice".to_string()),
            inviter_email: Some("alice@example.com".to_string()),
            expires_at: Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap(),
            invitations_url: "https://cloud.example.com/dashboard/invitations".to_string(),
        }
    }

    #[test]
    fn render_invitation_email_includes_expected_content() {
        let email = example_invitation_email();

        let rendered = render_invitation_email(&email);

        assert!(rendered.subject.contains("Example Org"));
        assert!(rendered.html_body.contains("Alice"));
        assert!(rendered.html_body.contains("Example Org"));
        assert!(rendered.html_body.contains("admin"));
        assert!(rendered
            .html_body
            .contains("https://cloud.example.com/dashboard/invitations"));
        assert!(rendered.text_body.contains("May 15, 2026"));
    }

    #[test]
    fn render_invitation_email_escapes_html() {
        let email = InvitationEmail {
            recipient_email: "invitee@example.com".to_string(),
            organization_name: "<Org>".to_string(),
            role: "member".to_string(),
            inviter_name: Some("Alice & Bob".to_string()),
            inviter_email: None,
            expires_at: Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap(),
            invitations_url: "https://cloud.example.com/dashboard/invitations".to_string(),
        };

        let rendered = render_invitation_email(&email);

        assert!(rendered.html_body.contains("&lt;Org&gt;"));
        assert!(rendered.html_body.contains("Alice &amp; Bob"));
    }

    #[test]
    fn render_model_deprecation_email_includes_expected_content_and_escapes_html() {
        let email = ModelDeprecationEmail {
            recipient_email: "admin@example.com".to_string(),
            model_id: "nearai/old-model".to_string(),
            model_display_name: "<Old & Model>".to_string(),
            deprecation_date: Utc.with_ymd_and_hms(2026, 7, 1, 13, 0, 0).unwrap(),
            successor_model_id: "nearai/new-model".to_string(),
        };

        let rendered = render_model_deprecation_email(&email);

        assert!(rendered.subject.contains("nearai/old-model"));
        assert!(rendered.html_body.contains("&lt;Old &amp; Model&gt;"));
        assert!(rendered.html_body.contains("nearai/new-model"));
        assert!(rendered.text_body.contains("July 1, 2026"));
    }

    fn example_pricing_change_email() -> PricingChangeEmail {
        PricingChangeEmail {
            recipient_email: "admin@example.com".to_string(),
            models: vec![
                PricingChangeEmailModel {
                    model_id: "nearai/model-a".to_string(),
                    model_display_name: "<Model & A>".to_string(),
                    effective_at: Utc.with_ymd_and_hms(2026, 7, 1, 13, 0, 0).unwrap(),
                    old_input_cost_per_token: 250,
                    new_input_cost_per_token: Some(280),
                    old_output_cost_per_token: 850,
                    new_output_cost_per_token: None,
                    old_cache_read_cost_per_token: 25,
                    new_cache_read_cost_per_token: None,
                    old_cost_per_image: 0,
                    new_cost_per_image: None,
                },
                PricingChangeEmailModel {
                    model_id: "nearai/model-b".to_string(),
                    model_display_name: "Model B".to_string(),
                    effective_at: Utc.with_ymd_and_hms(2026, 8, 15, 0, 0, 0).unwrap(),
                    old_input_cost_per_token: 5_000,
                    new_input_cost_per_token: None,
                    old_output_cost_per_token: 15_000,
                    new_output_cost_per_token: Some(12_000),
                    old_cache_read_cost_per_token: 0,
                    new_cache_read_cost_per_token: None,
                    old_cost_per_image: 2_000_000_000,
                    new_cost_per_image: Some(2_500_000_000),
                },
            ],
        }
    }

    #[test]
    fn render_pricing_change_email_lists_all_models_and_escapes_html() {
        let rendered = render_pricing_change_email(&example_pricing_change_email());

        assert_eq!(
            rendered.subject,
            "NEAR AI Cloud pricing update for 2 models"
        );
        assert!(rendered.html_body.contains("&lt;Model &amp; A&gt;"));
        assert!(rendered.html_body.contains("nearai/model-a"));
        assert!(rendered.html_body.contains("nearai/model-b"));
        assert!(rendered.text_body.contains("July 1, 2026 at 13:00 UTC"));
        assert!(rendered.text_body.contains("August 15, 2026 at 00:00 UTC"));
    }

    #[test]
    fn render_pricing_change_email_only_renders_changed_fields() {
        let rendered = render_pricing_change_email(&example_pricing_change_email());

        // Model A: input changed ($0.25 -> $0.28 per 1M), output/cache/image not.
        assert!(rendered
            .text_body
            .contains("Input tokens: $0.25 → $0.28 per 1M tokens"));
        assert!(!rendered.text_body.contains("$0.85"));
        // Model B: output ($15 -> $12 per 1M) and image ($2 -> $2.50) changed.
        assert!(rendered
            .text_body
            .contains("Output tokens: $15.00 → $12.00 per 1M tokens"));
        assert!(rendered
            .text_body
            .contains("Images: $2.00 → $2.50 per image"));
        assert!(!rendered.text_body.contains("Cache reads"));
    }

    #[test]
    fn render_pricing_change_email_single_model_subject() {
        let mut email = example_pricing_change_email();
        email.models.truncate(1);

        let rendered = render_pricing_change_email(&email);

        assert_eq!(
            rendered.subject,
            "NEAR AI Cloud pricing update: nearai/model-a"
        );
    }

    #[tokio::test]
    async fn resend_sender_builds_pricing_change_payload() {
        let transport = Arc::new(StubResendTransport {
            outcome: Ok(ResendSendEmailResponse {
                id: "pricing-email-id".to_string(),
            }),
            requests: Mutex::new(Vec::new()),
        });
        let sender = ResendEmailSender::new_with_transport(
            "NEAR AI Cloud <no-reply@near.ai>".to_string(),
            None,
            "re_test".to_string(),
            transport.clone(),
        );

        let result = sender
            .send_pricing_change(&example_pricing_change_email())
            .await
            .unwrap();

        assert_eq!(
            result,
            EmailDeliveryOutcome::Sent {
                message_id: Some("pricing-email-id".to_string())
            }
        );
        let requests = transport.requests.lock().unwrap();
        let (_, request) = &requests[0];
        assert_eq!(request.to, vec!["admin@example.com"]);
        assert!(request.subject.contains("2 models"));
        assert!(request.text.contains("nearai/model-b"));
    }

    #[test]
    fn sanitize_error_removes_newlines_and_truncates() {
        let message = format!("line one\n{}", "x".repeat(1100));

        let sanitized = sanitize_error(&message);

        assert!(!sanitized.contains('\n'));
        assert!(sanitized.ends_with('…'));
    }

    #[tokio::test]
    async fn resend_sender_builds_payload_and_returns_message_id() {
        let transport = Arc::new(StubResendTransport {
            outcome: Ok(ResendSendEmailResponse {
                id: "email-id".to_string(),
            }),
            requests: Mutex::new(Vec::new()),
        });
        let sender = ResendEmailSender::new_with_transport(
            "NEAR AI Cloud <no-reply@near.ai>".to_string(),
            Some("support@near.ai".to_string()),
            "re_test".to_string(),
            transport.clone(),
        );

        let result = sender
            .send_invitation(&example_invitation_email())
            .await
            .unwrap();

        assert_eq!(
            result,
            EmailDeliveryOutcome::Sent {
                message_id: Some("email-id".to_string())
            }
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let (api_key, request) = &requests[0];
        assert_eq!(api_key, "re_test");
        assert_eq!(request.from, "NEAR AI Cloud <no-reply@near.ai>");
        assert_eq!(request.to, vec!["invitee@example.com"]);
        assert_eq!(request.reply_to.as_deref(), Some("support@near.ai"));
        assert!(request.subject.contains("Example Org"));
        assert!(request.html.contains("View invitation"));
        assert!(request
            .text
            .contains("https://cloud.example.com/dashboard/invitations"));
    }

    #[tokio::test]
    async fn resend_sender_builds_model_deprecation_payload() {
        let transport = Arc::new(StubResendTransport {
            outcome: Ok(ResendSendEmailResponse {
                id: "deprecation-email-id".to_string(),
            }),
            requests: Mutex::new(Vec::new()),
        });
        let sender = ResendEmailSender::new_with_transport(
            "NEAR AI Cloud <no-reply@near.ai>".to_string(),
            None,
            "re_test".to_string(),
            transport.clone(),
        );

        let result = sender
            .send_model_deprecation(&ModelDeprecationEmail {
                recipient_email: "admin@example.com".to_string(),
                model_id: "nearai/old-model".to_string(),
                model_display_name: "Old Model".to_string(),
                deprecation_date: Utc.with_ymd_and_hms(2026, 7, 1, 13, 0, 0).unwrap(),
                successor_model_id: "nearai/new-model".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(
            result,
            EmailDeliveryOutcome::Sent {
                message_id: Some("deprecation-email-id".to_string())
            }
        );
        let requests = transport.requests.lock().unwrap();
        let (_, request) = &requests[0];
        assert_eq!(request.to, vec!["admin@example.com"]);
        assert!(request.subject.contains("nearai/old-model"));
        assert!(request.text.contains("nearai/new-model"));
    }

    #[tokio::test]
    async fn resend_sender_propagates_transport_error() {
        let transport = Arc::new(StubResendTransport {
            outcome: Err(EmailError::new("Resend failed\nwith details")),
            requests: Mutex::new(Vec::new()),
        });
        let sender = ResendEmailSender::new_with_transport(
            "NEAR AI Cloud <no-reply@near.ai>".to_string(),
            None,
            "re_test".to_string(),
            transport,
        );

        let error = sender
            .send_invitation(&example_invitation_email())
            .await
            .unwrap_err();

        assert_eq!(error.sanitized_message(), "Resend failed with details");
    }

    #[test]
    fn resend_error_message_extracts_json_message() {
        let message = resend_error_message(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"name":"validation_error","message":"domain is not verified"}"#,
        );

        assert_eq!(
            message,
            "Resend send_email failed with status 403 Forbidden: domain is not verified"
        );
    }

    #[tokio::test]
    async fn sender_from_config_uses_noop_when_disabled() {
        let sender = sender_from_config(&InvitationEmailConfig::default()).unwrap();

        let outcome = sender
            .send_invitation(&example_invitation_email())
            .await
            .unwrap();

        assert_eq!(outcome, EmailDeliveryOutcome::Skipped);
    }

    #[test]
    fn resend_sender_requires_api_key() {
        let config = InvitationEmailConfig {
            enabled: true,
            from_email: Some("NEAR AI Cloud <no-reply@near.ai>".to_string()),
            reply_to: None,
            resend_api_key: None,
            frontend_base_url: Some("https://cloud.example.com".to_string()),
        };

        let error = match ResendEmailSender::new(&config) {
            Ok(_) => panic!("expected missing Resend API key to fail"),
            Err(error) => error,
        };

        assert!(error.sanitized_message().contains("RESEND_API_KEY"));
    }
}
