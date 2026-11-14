//! The confidential OIDC client: exchanges a one-time code for an ID token, then checks
//! that token. The browser never talks to the token endpoint, so the client secret stays
//! on the server.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::ConsoleConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// An unknown key id is normal just after the provider rotates its keys, but anything
/// can claim one, so it mustn't turn into a request to the provider every time.
const MIN_JWKS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub enum OidcError {
    /// The provider or its token said no. The reason is for the log: this sign-in failed,
    /// and whoever is attempting it needn't learn exactly why.
    Rejected(&'static str),
    Unavailable,
}

#[derive(Debug, Deserialize)]
pub struct IdentityClaims {
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    /// Never kept: sessions renew by signing in again, so this is revoked straight away.
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: Option<String>,
    crv: Option<String>,
    x: Option<String>,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

pub struct OidcClient {
    http: reqwest::Client,
    issuer: String,
    token_url: String,
    revoke_url: String,
    jwks_url: String,
    client_id: String,
    client_secret: String,
    keys: RwLock<KeyCache>,
}

/// Parsed rather than trusted: only Ed25519 keys are ever usable here.
fn usable_keys(set: JwkSet) -> HashMap<String, DecodingKey> {
    set.keys
        .into_iter()
        .filter(|jwk| jwk.kty.as_deref() == Some("OKP") && jwk.crv.as_deref() == Some("Ed25519"))
        .filter_map(|jwk| {
            let key = DecodingKey::from_ed_components(jwk.x.as_deref()?).ok()?;
            Some((jwk.kid?, key))
        })
        .collect()
}

impl OidcClient {
    pub fn new(config: &ConsoleConfig) -> Result<Self, reqwest::Error> {
        let base = config
            .internal_url
            .as_deref()
            .unwrap_or(&config.issuer)
            .trim_end_matches('/');
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("loony-s3")
                .build()?,
            issuer: config.issuer.clone(),
            token_url: format!("{base}/oauth/token"),
            revoke_url: format!("{base}/oauth/revoke"),
            jwks_url: format!("{base}/.well-known/jwks.json"),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            keys: RwLock::new(KeyCache::default()),
        })
    }

    pub async fn sign_in(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
        nonce: &str,
    ) -> Result<IdentityClaims, OidcError> {
        let id_token = self
            .exchange_code(code, code_verifier, redirect_uri)
            .await?;
        self.verify_id_token(&id_token, nonce).await
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<String, OidcError> {
        let response = self
            .http
            .post(&self.token_url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err.without_url(), "couldn't reach the identity provider");
                OidcError::Unavailable
            })?;

        let status = response.status();
        if status.is_client_error() {
            return Err(OidcError::Rejected("the provider refused the sign-in code"));
        }
        if !status.is_success() {
            tracing::warn!(%status, "the identity provider's token endpoint failed");
            return Err(OidcError::Unavailable);
        }

        let tokens: TokenResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err.without_url(), "unreadable token response");
            OidcError::Unavailable
        })?;
        if let Some(refresh_token) = &tokens.refresh_token {
            self.revoke(refresh_token).await;
        }
        tokens
            .id_token
            .ok_or(OidcError::Rejected("the provider returned no ID token"))
    }

    /// Revokes a refresh token this server will never use. A failure is only logged: the
    /// sign-in itself is fine, and the token was never stored anywhere.
    async fn revoke(&self, refresh_token: &str) {
        let result = self
            .http
            .post(&self.revoke_url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
            ])
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(status = %response.status(), "the provider refused to revoke a refresh token");
            }
            Err(err) => {
                tracing::warn!(error = %err.without_url(), "couldn't revoke a refresh token");
            }
        }
    }

    async fn verify_id_token(&self, token: &str, nonce: &str) -> Result<IdentityClaims, OidcError> {
        let header = decode_header(token).map_err(|_| OidcError::Rejected("malformed ID token"))?;
        // Pinned here and in `validation`: the token doesn't get to pick its own algorithm.
        if header.alg != Algorithm::EdDSA {
            return Err(OidcError::Rejected("ID token uses the wrong algorithm"));
        }
        let kid = header
            .kid
            .ok_or(OidcError::Rejected("ID token names no key"))?;
        let key = self.key_for(&kid).await?;

        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);

        let claims = decode::<IdentityClaims>(token, &key, &validation)
            .map_err(|err| {
                tracing::info!(error = %err, "rejected an ID token");
                OidcError::Rejected("invalid ID token")
            })?
            .claims;

        if claims.nonce.as_deref() != Some(nonce) {
            return Err(OidcError::Rejected("ID token has the wrong nonce"));
        }
        Ok(claims)
    }

    async fn key_for(&self, kid: &str) -> Result<DecodingKey, OidcError> {
        if let Some(key) = self.keys.read().await.keys.get(kid) {
            return Ok(key.clone());
        }

        let mut cache = self.keys.write().await;
        // Another request may have refreshed while this one waited for the lock.
        if let Some(key) = cache.keys.get(kid) {
            return Ok(key.clone());
        }
        let too_soon = cache
            .fetched_at
            .is_some_and(|at| at.elapsed() < MIN_JWKS_REFRESH_INTERVAL);
        if too_soon {
            return Err(OidcError::Rejected("ID token names an unknown key"));
        }

        cache.keys = self.fetch_keys().await?;
        cache.fetched_at = Some(Instant::now());
        cache
            .keys
            .get(kid)
            .cloned()
            .ok_or(OidcError::Rejected("ID token names an unknown key"))
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>, OidcError> {
        let response = self.http.get(&self.jwks_url).send().await.map_err(|err| {
            tracing::warn!(error = %err.without_url(), "couldn't fetch the provider's keys");
            OidcError::Unavailable
        })?;
        if !response.status().is_success() {
            tracing::warn!(status = %response.status(), "the provider's key endpoint failed");
            return Err(OidcError::Unavailable);
        }
        let set: JwkSet = response.json().await.map_err(|err| {
            tracing::warn!(error = %err.without_url(), "unreadable key set");
            OidcError::Unavailable
        })?;
        Ok(usable_keys(set))
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }
}
