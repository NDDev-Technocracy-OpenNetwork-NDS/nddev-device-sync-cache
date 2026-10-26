//! HTTPS upstream client with deadlines, conditional requests and redirects limited to
//! the allowlisted hosts of the route table. Registries that answer with a Bearer challenge
//! get an anonymous token from their allowlisted token service.
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::{StatusCode, header};

use crate::{domain::Routes, store::Validators};

const MAX_REDIRECTS: usize = 5;
/// A token response is small JSON; anything larger is refused.
const TOKEN_LIMIT: usize = 64 * 1024;
/// Tokens are reused until shortly before their stated lifetime ends.
const TOKEN_MARGIN: Duration = Duration::from_secs(10);
const TOKEN_LIFETIME: u64 = 60;
const TOKENS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream_connect")]
    Connect,
    #[error("upstream_timeout")]
    Timeout,
    #[error("upstream_redirect")]
    Redirect,
    #[error("upstream_status")]
    Status,
    #[error("upstream_body")]
    Body,
    #[error("upstream_auth")]
    Auth,
}

pub type Body = Pin<Box<dyn Stream<Item = Result<Bytes, UpstreamError>> + Send>>;

pub enum Fetched {
    /// Validators, the declared `Content-Length` and the body stream.
    Content(Validators, Option<u64>, Body),
    NotModified(Validators),
    NotFound,
}

pub struct Upstream {
    client: reqwest::Client,
    routes: Arc<Routes>,
    /// Anonymous registry tokens by token service request, with their expiry.
    tokens: Mutex<HashMap<String, (String, Instant)>>,
}

/// The parameters of a `WWW-Authenticate: Bearer` challenge, quoted or not.
fn bearer_challenge(header: &str) -> Option<HashMap<String, String>> {
    let mut rest = header.strip_prefix("Bearer ")?.trim();
    let mut parameters = HashMap::new();
    while !rest.is_empty() {
        let (name, after) = rest.split_once('=')?;
        let (value, after) = match after.strip_prefix('"') {
            Some(quoted) => {
                let (value, after) = quoted.split_once('"')?;
                (value, after)
            }
            None => after
                .split_once(',')
                .map_or((after, ""), |(value, after)| (value, after)),
        };
        parameters.insert(name.trim().to_ascii_lowercase(), value.to_owned());
        rest = after.trim_start_matches(',').trim();
    }
    Some(parameters)
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
}

fn classify(error: &reqwest::Error) -> UpstreamError {
    if error.is_timeout() {
        UpstreamError::Timeout
    } else if error.is_redirect() {
        UpstreamError::Redirect
    } else if error.is_connect() || error.is_request() {
        UpstreamError::Connect
    } else {
        UpstreamError::Body
    }
}

fn header_text(headers: &header::HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 256)
        .map(str::to_owned)
}

fn validators(headers: &header::HeaderMap) -> Validators {
    Validators {
        content_type: header_text(headers, header::CONTENT_TYPE),
        etag: header_text(headers, header::ETAG),
        last_modified: header_text(headers, header::LAST_MODIFIED),
    }
}

impl Upstream {
    pub fn new(
        routes: Arc<Routes>,
        user_agent: &str,
        connect: Duration,
        read: Duration,
    ) -> Result<Self, UpstreamError> {
        // The adapter owns its TLS stack; installing the provider is idempotent.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let routes_for_tokens = routes.clone();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            let url = attempt.url();
            let allowed = routes.redirect_allowed(url.scheme(), url.host_str().unwrap_or_default());
            if attempt.previous().len() < MAX_REDIRECTS && allowed {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let client = reqwest::Client::builder()
            .redirect(policy)
            .connect_timeout(connect)
            .read_timeout(read)
            .user_agent(user_agent)
            .build()
            .map_err(|_| UpstreamError::Connect)?;
        Ok(Self {
            client,
            routes: routes_for_tokens,
            tokens: Mutex::default(),
        })
    }

    /// Fetches an object, in `media_type` when the object is one representation of an index
    /// document or an OCI manifest. Stored validators make the request conditional; a Bearer
    /// challenge is answered once with an anonymous token.
    pub async fn fetch(
        &self,
        url: &str,
        media_type: Option<&str>,
        stored: Option<&Validators>,
    ) -> Result<Fetched, UpstreamError> {
        let request = |token: Option<&str>| {
            let mut request = self.client.get(url);
            if let Some(media_type) = media_type {
                request = request.header(header::ACCEPT, media_type);
            }
            if let Some(stored) = stored {
                if let Some(etag) = &stored.etag {
                    request = request.header(header::IF_NONE_MATCH, etag);
                }
                if let Some(modified) = &stored.last_modified {
                    request = request.header(header::IF_MODIFIED_SINCE, modified);
                }
            }
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            request
        };
        let mut response = request(None)
            .send()
            .await
            .map_err(|error| classify(&error))?;
        if response.status() == StatusCode::UNAUTHORIZED
            && let Some(token) = self.token(response.headers()).await?
        {
            response = request(Some(&token))
                .send()
                .await
                .map_err(|error| classify(&error))?;
        }
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified(validators(response.headers())));
        }
        if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            return Ok(Fetched::NotFound);
        }
        if status.is_redirection() {
            return Err(UpstreamError::Redirect);
        }
        if !status.is_success() {
            return Err(UpstreamError::Status);
        }
        let found = validators(response.headers());
        let length = response.content_length();
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|error| classify(&error)));
        Ok(Fetched::Content(found, length, Box::pin(body)))
    }

    /// An anonymous token for the Bearer challenge of a `401` response, when its token service
    /// is allowlisted; tokens are reused until shortly before they expire.
    async fn token(&self, headers: &header::HeaderMap) -> Result<Option<String>, UpstreamError> {
        let Some(challenge) = headers
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_challenge)
        else {
            return Ok(None);
        };
        let Some(mut url) = challenge
            .get("realm")
            .and_then(|realm| reqwest::Url::parse(realm).ok())
        else {
            return Ok(None);
        };
        if !self
            .routes
            .redirect_allowed(url.scheme(), url.host_str().unwrap_or_default())
        {
            return Ok(None);
        }
        for name in ["service", "scope"] {
            if let Some(value) = challenge.get(name) {
                url.query_pairs_mut().append_pair(name, value);
            }
        }
        let key = url.to_string();
        let cached = self
            .tokens
            .lock()
            .ok()
            .and_then(|tokens| tokens.get(&key).cloned());
        if let Some((token, expiry)) = cached
            && Instant::now() < expiry
        {
            return Ok(Some(token));
        }
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| UpstreamError::Auth)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > TOKEN_LIMIT as u64)
        {
            return Err(UpstreamError::Auth);
        }
        let mut body = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            body.extend_from_slice(&chunk.map_err(|_| UpstreamError::Auth)?);
            if body.len() > TOKEN_LIMIT {
                return Err(UpstreamError::Auth);
            }
        }
        let parsed: TokenResponse =
            serde_json::from_slice(&body).map_err(|_| UpstreamError::Auth)?;
        let token = parsed
            .token
            .or(parsed.access_token)
            .ok_or(UpstreamError::Auth)?;
        let lifetime = Duration::from_secs(parsed.expires_in.unwrap_or(TOKEN_LIFETIME))
            .saturating_sub(TOKEN_MARGIN);
        if let Ok(mut tokens) = self.tokens.lock() {
            if tokens.len() >= TOKENS {
                tokens.clear();
            }
            tokens.insert(key, (token.clone(), Instant::now() + lifetime));
        }
        Ok(Some(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_challenges_parse_quoted_and_bare_parameters() {
        let docker = bearer_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        )
        .unwrap();
        assert_eq!(docker["realm"], "https://auth.docker.io/token");
        assert_eq!(docker["scope"], "repository:library/alpine:pull");
        let gcr =
            bearer_challenge(r#"Bearer realm="https://gcr.io/v2/token",service=gcr.io"#).unwrap();
        assert_eq!(gcr["service"], "gcr.io");
        assert!(bearer_challenge("Basic realm=x").is_none());
    }
}
