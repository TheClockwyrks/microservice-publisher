//! The repository publish: the repos service's publish route.
//!
//! The run's public repository is the repos service's to create and push (see
//! `components/repos/overview.md`, "Publishing"): it holds the run's
//! implementation bundle and the GitHub credential, which the publisher Job never
//! holds. The publisher asks it to publish with `POST {repos_url}/runs/{id}/publish`,
//! presenting its per-publish-job token as a bearer credential and naming its
//! publish job in the `x-tcab-publish-job-id` header, exactly as the `tree.tar`
//! download does. The service answers `200` with `{ "url": <string or null> }`:
//! the repository's URL, or `null` for a run that releases no source.
//!
//! The route is idempotent, so a transient failure (no response, a `408`, `429`
//! or `5xx`, the documented `502` upstream failure among them) is retried with
//! backoff (see [`crate::retry`]). A `409` means the repository's `main` already
//! holds a different tree, which the service leaves as it is, so it is
//! [`Error::Diverged`] and is not retried; so are the other refusals (`401`,
//! `404`, `422`, and the `503` of a service not configured to publish), each
//! reported with the service's message.

use std::time::Duration;

use serde::Deserialize;

use crate::download::PUBLISH_JOB_ID_HEADER;
use crate::error::{Error, Result};
use crate::retry::{Backoff, retry};

/// How long one publish request may take. The service fetches the bundle, scrubs
/// it, creates the repository and pushes to it before it answers, so the bound
/// is generous, while staying well inside the publisher Job's 45-minute
/// deadline.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The body a successful publish answers.
#[derive(Debug, Deserialize)]
struct PublishAnswer {
    /// The run's public repository, or `None` for a run that releases no source.
    url: Option<String>,
}

/// The error envelope the repos service answers a refusal with.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    /// The error itself.
    error: ErrorBody,
}

/// The body of an [`ErrorEnvelope`].
#[derive(Debug, Deserialize)]
struct ErrorBody {
    /// The human-readable reason.
    message: String,
}

/// A client of the repos service's publish route for one publish job.
#[derive(Debug, Clone)]
pub struct ReposClient {
    http: reqwest::Client,
    repos_url: String,
    run_id: String,
    publish_job_id: String,
    job_token: String,
    backoff: Backoff,
}

impl ReposClient {
    /// A client publishing run `run_id` through the repos service at
    /// `repos_url`, authenticating as publish job `publish_job_id` with its
    /// per-publish-job `job_token`.
    pub fn new(
        repos_url: impl Into<String>,
        run_id: impl Into<String>,
        publish_job_id: impl Into<String>,
        job_token: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(PUBLISH_TIMEOUT)
                .build()
                .unwrap_or_default(),
            repos_url: repos_url.into().trim_end_matches('/').to_string(),
            run_id: run_id.into(),
            publish_job_id: publish_job_id.into(),
            job_token: job_token.into(),
            backoff: Backoff::default(),
        }
    }

    /// Retry a transient failure with `backoff` instead of [`Backoff::default`].
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Publish the run's implementation to its public repository, returning the
    /// repository's URL, or `None` when the service answers that the run releases
    /// no source. A transient failure is retried with backoff.
    pub async fn publish(&self) -> Result<Option<String>> {
        retry(&self.backoff, "the repository publish", || {
            self.publish_once()
        })
        .await
    }

    /// One `POST` of the publish route.
    async fn publish_once(&self) -> Result<Option<String>> {
        let url = format!("{}/runs/{}/publish", self.repos_url, self.run_id);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.job_token)
            .header(PUBLISH_JOB_ID_HEADER, &self.publish_job_id)
            .send()
            .await
            .map_err(|err| {
                if err.is_builder() {
                    Error::Publish(format!(
                        "building the repos service request to `{url}`: {err}"
                    ))
                } else {
                    Error::Repos {
                        status: None,
                        detail: err.to_string(),
                    }
                }
            })?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|err| Error::Repos {
            status: Some(status),
            detail: format!("reading the answer: {err}"),
        })?;
        if (200..300).contains(&status) {
            let answer: PublishAnswer =
                serde_json::from_str(&body).map_err(|err| Error::Repos {
                    status: Some(status),
                    detail: format!("the answer is not `{{ \"url\" }}`: {err}"),
                })?;
            return Ok(answer.url);
        }
        let message = error_message(&body);
        if status == 409 {
            return Err(Error::Diverged(message));
        }
        Err(Error::Repos {
            status: Some(status),
            detail: message,
        })
    }
}

/// The reason a refusal's `body` gives: the envelope's message, or the body as
/// it came when it is not an envelope.
fn error_message(body: &str) -> String {
    match serde_json::from_str::<ErrorEnvelope>(body) {
        Ok(envelope) => envelope.error.message,
        Err(_) if body.trim().is_empty() => "(no body)".to_string(),
        Err(_) => body.trim().to_string(),
    }
}

#[cfg(test)]
#[path = "repos.test.rs"]
mod tests;
