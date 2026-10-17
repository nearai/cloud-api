use super::ports::{AuthError, OAuthUserInfo};
use config::OAuthProviderConfig;
use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use reqwest::Client;
use serde::Deserialize;
use tracing::debug;

// Type alias for a fully configured OAuth client
type ConfiguredClient = oauth2::Client<
    oauth2::basic::BasicErrorResponse,
    oauth2::basic::BasicTokenResponse,
    oauth2::basic::BasicTokenIntrospectionResponse,
    oauth2::StandardRevocableToken,
    oauth2::basic::BasicRevocationErrorResponse,
    oauth2::EndpointSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointSet,
>;

fn token_exchange_error<E: std::error::Error + 'static>(
    error: oauth2::RequestTokenError<E, oauth2::basic::BasicErrorResponse>,
) -> AuthError {
    let message = format!("Token exchange failed: {error}");
    match error {
        oauth2::RequestTokenError::ServerResponse(response) => match response.error() {
            oauth2::basic::BasicErrorResponseType::InvalidGrant => AuthError::OAuthError(message),
            oauth2::basic::BasicErrorResponseType::InvalidClient
            | oauth2::basic::BasicErrorResponseType::UnauthorizedClient => {
                AuthError::ConfigError(message)
            }
            _ => AuthError::InternalError(message),
        },
        oauth2::RequestTokenError::Request(_) => AuthError::NetworkError(message),
        _ => AuthError::InternalError(message),
    }
}

/// OAuth2 authentication manager
pub struct OAuthManager {
    github_client: Option<ConfiguredClient>,
    google_client: Option<ConfiguredClient>,
    http_client: Client,
    github_user_url: String,
    github_emails_url: String,
    google_user_url: String,
}

impl OAuthManager {
    pub fn new(
        github_config: Option<OAuthProviderConfig>,
        google_config: Option<OAuthProviderConfig>,
    ) -> Result<Self, AuthError> {
        let github_client = github_config.map(Self::create_github_client).transpose()?;

        let google_client = google_config.map(Self::create_google_client).transpose()?;

        Ok(Self {
            github_client,
            google_client,
            http_client: Client::new(),
            github_user_url: "https://api.github.com/user".into(),
            github_emails_url: "https://api.github.com/user/emails".into(),
            google_user_url: "https://www.googleapis.com/oauth2/v2/userinfo".into(),
        })
    }

    #[cfg(feature = "test-mocks")]
    pub fn with_test_provider_endpoints(mut self, provider: &str, base_url: &str) -> Self {
        let token_url = TokenUrl::new(format!("{base_url}/token")).unwrap();
        let auth_url = AuthUrl::new(format!("{base_url}/authorize")).unwrap();
        match provider {
            "github" => {
                self.github_client = self
                    .github_client
                    .map(|client| client.set_token_uri(token_url).set_auth_uri(auth_url));
                self.github_user_url = format!("{base_url}/user");
                self.github_emails_url = format!("{base_url}/emails");
            }
            "google" => {
                self.google_client = self
                    .google_client
                    .map(|client| client.set_token_uri(token_url).set_auth_uri(auth_url));
                self.google_user_url = format!("{base_url}/user");
            }
            _ => panic!("Unsupported test provider"),
        }
        self
    }

    fn create_github_client(config: OAuthProviderConfig) -> Result<ConfiguredClient, AuthError> {
        let auth_url = AuthUrl::new("https://github.com/login/oauth/authorize".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid GitHub auth URL: {e}")))?;

        let token_url = TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid GitHub token URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(config.client_id))
            .set_client_secret(ClientSecret::new(config.client_secret))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(
                RedirectUrl::new(config.redirect_uri)
                    .map_err(|e| AuthError::ConfigError(format!("Invalid redirect URL: {e}")))?,
            );

        Ok(client)
    }

    fn create_google_client(config: OAuthProviderConfig) -> Result<ConfiguredClient, AuthError> {
        let auth_url = AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid Google auth URL: {e}")))?;

        let token_url = TokenUrl::new("https://www.googleapis.com/oauth2/v3/token".to_string())
            .map_err(|e| AuthError::ConfigError(format!("Invalid Google token URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(config.client_id))
            .set_client_secret(ClientSecret::new(config.client_secret))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(
                RedirectUrl::new(config.redirect_uri)
                    .map_err(|e| AuthError::ConfigError(format!("Invalid redirect URL: {e}")))?,
            );

        Ok(client)
    }

    /// Generate authorization URL for GitHub
    pub fn github_auth_url(&self) -> Result<(String, String), AuthError> {
        let client = self
            .github_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("GitHub OAuth not configured".to_string()))?;

        let (auth_url, csrf_state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("user:email".to_string()))
            .url();

        Ok((auth_url.to_string(), csrf_state.secret().to_string()))
    }

    pub fn github_link_auth_url(&self) -> Result<(String, String, String), AuthError> {
        let client = self
            .github_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("GitHub OAuth not configured".into()))?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("user:email".into()))
            .set_pkce_challenge(challenge)
            .url();
        Ok((
            url.to_string(),
            state.secret().clone(),
            verifier.secret().clone(),
        ))
    }

    /// Generate authorization URL for Google with PKCE
    pub fn google_auth_url(&self) -> Result<(String, String, String), AuthError> {
        let client = self
            .google_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("Google OAuth not configured".to_string()))?;

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let (auth_url, csrf_state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .url();

        Ok((
            auth_url.to_string(),
            csrf_state.secret().to_string(),
            pkce_verifier.secret().to_string(),
        ))
    }

    pub fn google_link_auth_url(&self) -> Result<(String, String, String), AuthError> {
        let (url, state, verifier) = self.google_auth_url()?;
        let mut url = url::Url::parse(&url)
            .map_err(|e| AuthError::ConfigError(format!("Invalid Google auth URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("prompt", "select_account");
        Ok((url.to_string(), state, verifier))
    }

    /// Handle GitHub OAuth callback
    pub async fn handle_github_callback(
        &self,
        code: String,
        _state: String,
    ) -> Result<(OAuthUserInfo, String), AuthError> {
        self.handle_github_callback_with_pkce(code, None).await
    }

    pub async fn handle_github_callback_with_pkce(
        &self,
        code: String,
        verifier: Option<String>,
    ) -> Result<(OAuthUserInfo, String), AuthError> {
        let client = self
            .github_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("GitHub OAuth not configured".to_string()))?;

        debug!("Exchanging GitHub code for token");

        // Exchange code for token
        let mut exchange = client.exchange_code(AuthorizationCode::new(code));
        if let Some(verifier) = verifier {
            exchange = exchange.set_pkce_verifier(PkceCodeVerifier::new(verifier));
        }
        let token = exchange
            .request_async(&self.http_client)
            .await
            .map_err(token_exchange_error)?;

        let access_token = token.access_token().secret();

        // Get user info from GitHub
        let user_info = self.fetch_github_user(access_token).await?;

        let oauth_info = OAuthUserInfo {
            email_verified: user_info.email_verified,
            provider: "github".to_string(),
            provider_user_id: user_info.id.to_string(),
            email: user_info.email.ok_or_else(|| {
                AuthError::AuthFailed("GitHub user has no public email".to_string())
            })?,
            username: user_info.login,
            display_name: user_info.name,
            avatar_url: user_info.avatar_url,
        };

        debug!(
            provider = "github",
            provider_user_id = %oauth_info.provider_user_id,
            "OAuth user authenticated"
        );
        Ok((oauth_info, access_token.to_string()))
    }

    /// Handle Google OAuth callback
    pub async fn handle_google_callback(
        &self,
        code: String,
        _state: String,
        pkce_verifier_str: String,
    ) -> Result<(OAuthUserInfo, String), AuthError> {
        let client = self
            .google_client
            .as_ref()
            .ok_or_else(|| AuthError::ConfigError("Google OAuth not configured".to_string()))?;

        debug!("Exchanging Google code for token");

        // Convert verifier string back to PkceCodeVerifier
        let pkce_verifier = PkceCodeVerifier::new(pkce_verifier_str);

        // Exchange code for token with PKCE
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pkce_verifier)
            .request_async(&self.http_client)
            .await
            .map_err(token_exchange_error)?;

        let access_token = token.access_token().secret();

        // Get user info from Google
        let user_info = self.fetch_google_user(access_token).await?;

        let oauth_info = OAuthUserInfo {
            email_verified: user_info.email_verified,
            provider: "google".to_string(),
            provider_user_id: user_info.sub.clone(),
            email: user_info.email.clone(),
            username: user_info
                .email
                .split('@')
                .next()
                .unwrap_or("user")
                .to_string(),
            display_name: user_info.name,
            avatar_url: user_info.picture,
        };

        debug!(
            provider = "google",
            provider_user_id = %oauth_info.provider_user_id,
            "OAuth user authenticated"
        );
        Ok((oauth_info, access_token.to_string()))
    }

    /// Fetch GitHub user information
    async fn fetch_github_user(&self, access_token: &str) -> Result<GitHubUser, AuthError> {
        let response = self
            .http_client
            .get(&self.github_user_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("User-Agent", "cloud-api")
            .send()
            .await
            .map_err(|e| AuthError::NetworkError(format!("Failed to fetch GitHub user: {e}")))?;

        if !response.status().is_success() {
            return Err(AuthError::AuthFailed(format!(
                "GitHub API returned status: {}",
                response.status()
            )));
        }

        let mut user: GitHubUser = response
            .json()
            .await
            .map_err(|e| AuthError::AuthFailed(format!("Failed to parse GitHub user: {e}")))?;

        // The public profile has no email verification flag. Read /user/emails
        // even for a public email before accepting it for account linking.
        {
            let emails_response = self
                .http_client
                .get(&self.github_emails_url)
                .header("Authorization", format!("Bearer {access_token}"))
                .header("User-Agent", "cloud-api")
                .send()
                .await
                .map_err(|e| {
                    AuthError::NetworkError(format!("Failed to fetch GitHub emails: {e}"))
                })?;

            if emails_response.status().is_success() {
                let emails: Vec<GitHubEmail> = emails_response.json().await.map_err(|e| {
                    AuthError::AuthFailed(format!("Failed to parse GitHub emails: {e}"))
                })?;

                let selected = if let Some(public_email) = user.email.as_ref() {
                    emails.iter().find(|entry| &entry.email == public_email)
                } else {
                    emails
                        .iter()
                        .find(|e| e.primary && e.verified)
                        .or_else(|| emails.iter().find(|e| e.verified))
                        .or_else(|| emails.iter().find(|e| e.primary))
                        .or_else(|| emails.first())
                };
                if let Some(selected) = selected {
                    user.email = Some(selected.email.clone());
                    user.email_verified = selected.verified;
                }
            }
        }

        Ok(user)
    }

    /// Fetch Google user information
    async fn fetch_google_user(&self, access_token: &str) -> Result<GoogleUser, AuthError> {
        debug!("Fetching Google user info with access token");

        let response = self
            .http_client
            .get(&self.google_user_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|e| AuthError::NetworkError(format!("Failed to fetch Google user: {e}")))?;

        let status = response.status();
        debug!("Google API response status: {}", status);

        if !status.is_success() {
            let response_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read response".to_string());
            return Err(AuthError::AuthFailed(format!(
                "Google API returned status: {status}, body: {response_text}"
            )));
        }

        response
            .json()
            .await
            .map_err(|e| AuthError::AuthFailed(format!("Failed to parse Google user: {e}")))
    }
}

#[derive(Deserialize)]
struct GitHubUser {
    id: i64,
    email: Option<String>,
    #[serde(skip)]
    email_verified: bool,
    #[serde(default)]
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    #[serde(default)]
    verified: bool,
}

#[derive(Debug, Deserialize)]
struct GoogleUser {
    #[serde(alias = "sub", alias = "id")]
    sub: String,
    email: String,
    #[serde(default, alias = "verified_email")]
    email_verified: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    picture: Option<String>,
}

#[cfg(test)]
mod token_exchange_tests {
    use super::*;
    use oauth2::{basic::BasicErrorResponse, basic::BasicErrorResponseType, RequestTokenError};

    #[test]
    fn invalid_grant_is_an_authorization_failure() {
        let error =
            RequestTokenError::<std::io::Error, _>::ServerResponse(BasicErrorResponse::new(
                BasicErrorResponseType::InvalidGrant,
                Some("expired authorization code".into()),
                None,
            ));
        assert!(matches!(
            token_exchange_error(error),
            AuthError::OAuthError(_)
        ));
    }

    #[test]
    fn configuration_and_provider_faults_are_not_authorization_failures() {
        for kind in [
            BasicErrorResponseType::InvalidClient,
            BasicErrorResponseType::UnauthorizedClient,
        ] {
            let error = RequestTokenError::<std::io::Error, _>::ServerResponse(
                BasicErrorResponse::new(kind, None, None),
            );
            assert!(matches!(
                token_exchange_error(error),
                AuthError::ConfigError(_)
            ));
        }
        let error =
            RequestTokenError::<std::io::Error, _>::ServerResponse(BasicErrorResponse::new(
                BasicErrorResponseType::Extension("server_error".into()),
                None,
                None,
            ));
        assert!(matches!(
            token_exchange_error(error),
            AuthError::InternalError(_)
        ));
        let error = RequestTokenError::<std::io::Error, BasicErrorResponse>::Other(
            "unexpected provider response".into(),
        );
        assert!(matches!(
            token_exchange_error(error),
            AuthError::InternalError(_)
        ));
    }

    #[test]
    fn transport_failure_stays_a_network_error() {
        let error = RequestTokenError::<_, BasicErrorResponse>::Request(std::io::Error::other(
            "connection failed",
        ));
        assert!(matches!(
            token_exchange_error(error),
            AuthError::NetworkError(_)
        ));
    }
}
