use async_trait::async_trait;
use chrono::{DateTime, Utc};
use config::InvitationEmailConfig;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const RESEND_EMAILS_URL: &str = "https://api.resend.com/emails";

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
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(api_key)
            .json(&request)
            .send()
            .await
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
