//! Tests for the repos service's publish route client, against a local axum stub
//! that answers a scripted sequence of responses and records each request.

use super::*;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokio::net::TcpListener;

/// One request the stub saw.
#[derive(Debug, Clone)]
struct Seen {
    /// The `{id}` the publish addressed.
    run_id: String,
    /// The bearer token presented.
    token: Option<String>,
    /// The publish-job-id header presented.
    publish_job_id: Option<String>,
}

/// The stub's state: the answers still to give, in order, and the requests seen.
#[derive(Clone, Default)]
struct Stub {
    answers: Arc<Mutex<VecDeque<(StatusCode, String)>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Stub {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("lock").clone()
    }
}

/// Serve the stub answering `answers` in order (the last one repeats), returning
/// its base URL and the stub.
async fn serve(answers: Vec<(StatusCode, &str)>) -> (String, Stub) {
    let stub = Stub {
        answers: Arc::new(Mutex::new(
            answers
                .into_iter()
                .map(|(status, body)| (status, body.to_string()))
                .collect(),
        )),
        seen: Arc::default(),
    };
    let app = Router::new()
        .route("/runs/{id}/publish", post(publish))
        .with_state(stub.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}/"), stub)
}

/// The stub publish route: record the request, then give the next answer.
async fn publish(
    State(stub): State<Stub>,
    AxumPath(run_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let publish_job_id = headers
        .get("x-tcab-publish-job-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    stub.seen.lock().expect("lock").push(Seen {
        run_id,
        token,
        publish_job_id,
    });
    let mut answers = stub.answers.lock().expect("lock");
    let (status, body) = if answers.len() > 1 {
        answers.pop_front().expect("an answer")
    } else {
        answers.front().cloned().expect("an answer")
    };
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Five attempts with no pause between them.
fn instant() -> Backoff {
    Backoff {
        attempts: 5,
        initial_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    }
}

/// A client for run `run-7` as publish job `pub-3` against `base`.
fn client(base: &str) -> ReposClient {
    ReposClient::new(base, "run-7", "pub-3", "pub-token").with_backoff(instant())
}

#[tokio::test]
async fn a_publish_answers_the_repository_url_with_the_job_credentials() {
    let (base, stub) = serve(vec![(
        StatusCode::OK,
        r#"{"url":"https://github.com/TheClockwyrks/tcab-pong-run-7"}"#,
    )])
    .await;

    let url = client(&base).publish().await.expect("publish");

    assert_eq!(
        url.as_deref(),
        Some("https://github.com/TheClockwyrks/tcab-pong-run-7")
    );
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    // The trailing slash of the base is trimmed, so the path is the route's.
    assert_eq!(seen[0].run_id, "run-7");
    assert_eq!(seen[0].token.as_deref(), Some("pub-token"));
    assert_eq!(seen[0].publish_job_id.as_deref(), Some("pub-3"));
}

#[tokio::test]
async fn a_null_url_is_a_run_that_releases_no_source() {
    let (base, _stub) = serve(vec![(StatusCode::OK, r#"{"url":null}"#)]).await;

    let url = client(&base).publish().await.expect("publish");

    assert_eq!(url, None);
}

#[tokio::test]
async fn a_conflict_is_diverged_with_the_service_message_and_not_retried() {
    let message = "`TheClockwyrks/tcab-pong-run-7` already holds a `main` whose tree differs \
                   from this run's scrubbed implementation; it is left as it is";
    let body = serde_json::json!({ "error": { "code": "conflict", "message": message } });
    let (base, stub) = serve(vec![(StatusCode::CONFLICT, &body.to_string())]).await;

    let err = client(&base).publish().await.expect_err("a 409 fails");

    assert!(
        matches!(&err, Error::Diverged(detail) if detail == message),
        "{err:?}"
    );
    assert_eq!(err.to_string(), format!("publishing the run: {message}"));
    assert_eq!(stub.seen().len(), 1, "a 409 is not retried");
}

#[tokio::test]
async fn an_upstream_failure_is_retried_until_the_publish_lands() {
    let failed = r#"{"error":{"code":"upstream","message":"pushing failed"}}"#;
    let (base, stub) = serve(vec![
        (StatusCode::BAD_GATEWAY, failed),
        (StatusCode::BAD_GATEWAY, failed),
        (StatusCode::OK, r#"{"url":"https://github.com/o/r"}"#),
    ])
    .await;

    let url = client(&base).publish().await.expect("publish");

    assert_eq!(url.as_deref(), Some("https://github.com/o/r"));
    assert_eq!(stub.seen().len(), 3);
}

#[tokio::test]
async fn an_upstream_failure_that_persists_is_bounded_by_the_attempts() {
    let failed = r#"{"error":{"code":"upstream","message":"pushing failed"}}"#;
    let (base, stub) = serve(vec![(StatusCode::BAD_GATEWAY, failed)]).await;

    let err = client(&base)
        .publish()
        .await
        .expect_err("every attempt fails");

    assert!(
        matches!(&err, Error::Repos { status: Some(502), detail } if detail == "pushing failed"),
        "{err:?}"
    );
    assert_eq!(stub.seen().len(), 5);
}

#[tokio::test]
async fn a_refusal_is_not_retried_and_carries_the_body() {
    for (status, body, detail) in [
        (
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"code":"unauthorized","message":"token rejected"}}"#,
            "token rejected",
        ),
        (
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":"not_found","message":"no bundle is stored for run `run-7`"}}"#,
            "no bundle is stored for run `run-7`",
        ),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "not an envelope",
            "not an envelope",
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"code":"unconfigured","message":"this service publishes nothing"}}"#,
            "this service publishes nothing",
        ),
    ] {
        let (base, stub) = serve(vec![(status, body)]).await;

        let err = client(&base).publish().await.expect_err("a refusal fails");

        let code = status.as_u16();
        assert!(
            matches!(&err, Error::Repos { status: Some(s), detail: d } if *s == code && d == detail),
            "{err:?}"
        );
        assert!(err.to_string().contains(&format!("HTTP {code}")), "{err}");
        assert_eq!(stub.seen().len(), 1, "a {code} is not retried");
    }
}

#[tokio::test]
async fn an_unreadable_answer_fails_without_retrying() {
    let (base, stub) = serve(vec![(StatusCode::OK, "<html>a proxy page</html>")]).await;

    let err = client(&base).publish().await;

    assert!(
        matches!(
            err,
            Err(Error::Repos {
                status: Some(200),
                ..
            })
        ),
        "{err:?}"
    );
    assert_eq!(stub.seen().len(), 1);
}

#[tokio::test]
async fn an_unreachable_service_is_transient() {
    // Bind and drop a listener so the port refuses connections.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    let err = client(&format!("http://{addr}"))
        .with_backoff(Backoff {
            attempts: 1,
            ..instant()
        })
        .publish()
        .await
        .expect_err("nothing answers");

    assert!(matches!(err, Error::Repos { status: None, .. }), "{err:?}");
    assert!(crate::retry::Transient::is_transient(&err));
}
