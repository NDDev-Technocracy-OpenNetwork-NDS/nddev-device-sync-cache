//! HTTPS upstream client with deadlines, conditional requests and redirects limited to
//! the allowlisted hosts of the route table.
use std::{pin::Pin, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::{StatusCode, header};

use crate::{domain::Routes, store::Validators};

const MAX_REDIRECTS: usize = 5;

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
}

pub type Body = Pin<Box<dyn Stream<Item = Result<Bytes, UpstreamError>> + Send>>;

pub enum Fetched {
    Content(Validators, Body),
    NotModified(Validators),
    NotFound,
}

pub struct Upstream {
    client: reqwest::Client,
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
        Ok(Self { client })
    }

    /// Fetches an object. Stored validators make the request conditional.
    pub async fn fetch(
        &self,
        url: &str,
        stored: Option<&Validators>,
    ) -> Result<Fetched, UpstreamError> {
        let mut request = self.client.get(url);
        if let Some(stored) = stored {
            if let Some(etag) = &stored.etag {
                request = request.header(header::IF_NONE_MATCH, etag);
            }
            if let Some(modified) = &stored.last_modified {
                request = request.header(header::IF_MODIFIED_SINCE, modified);
            }
        }
        let response = request.send().await.map_err(|error| classify(&error))?;
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
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|error| classify(&error)));
        Ok(Fetched::Content(found, Box::pin(body)))
    }
}
