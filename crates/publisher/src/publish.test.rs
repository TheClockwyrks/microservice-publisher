//! Tests for the release orchestration: the publisher loads the record from the
//! downloaded tree, finds the build, deploys it through a stub [`CommandRunner`]
//! and a stub Cloudflare API, then calls a stub repos service's publish route,
//! and returns the produced links; a run that releases no source skips the
//! repos call, and a missing record fails cleanly. The deploy itself is covered
//! by [`crate::release`]'s tests and the repos client by [`crate::repos`]'s.

use super::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use test_cabinet_contracts::metrics::{Cost, RunMetrics, TokenCounts};
use test_cabinet_contracts::run_record::{
    AuthMode, HarnessSlug, RunEnvironment, RunLinks, RunState, RunStatus, RunSubject, RunTooling,
};
use test_cabinet_contracts::test_case::TestType;
use test_cabinet_contracts::validation::ValidationSummary;
use tokio::net::TcpListener;

use crate::pages::tests::StubPagesApi;
use crate::release::{CommandOutput, CommandRunner, PublishConfig};
use crate::retry::Backoff;

/// The order the release reached the outside world in: `wrangler` runs and
/// repos publishes, as one log the runner and the repos stub both append to.
type Log = Arc<Mutex<Vec<String>>>;

/// A [`CommandRunner`] standing in for `wrangler`: it records every call in the
/// shared log and prints a deployment URL.
#[derive(Clone, Default)]
struct StubRunner {
    log: Log,
}

#[async_trait::async_trait]
impl CommandRunner for StubRunner {
    async fn run(&self, program: &str, args: &[&str]) -> crate::Result<CommandOutput> {
        self.log
            .lock()
            .expect("lock")
            .push(format!("{program} {}", args.join(" ")));
        Ok(CommandOutput {
            success: true,
            stdout: "✨ Deployment complete! https://abc123.test-cabinet-runs.pages.dev\n"
                .to_string(),
            stderr: String::new(),
        })
    }
}

/// Serve a stub repos service answering every publish with `status` and `body`,
/// recording each in `log`, and return a client of it.
async fn stub_repos(log: Log, status: StatusCode, body: &'static str) -> ReposClient {
    let app = Router::new().route(
        "/runs/{id}/publish",
        post(
            move |axum::extract::Path(id): axum::extract::Path<String>| {
                let log = log.clone();
                async move {
                    log.lock()
                        .expect("lock")
                        .push(format!("repos publish {id}"));
                    (status, [("content-type", "application/json")], body)
                }
            },
        ),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    ReposClient::new(
        format!("http://{addr}"),
        "d483a2f9-7bda-4018-a27f-586ccdf31a9e",
        "pub-1",
        "pub-token",
    )
    .with_backoff(Backoff {
        attempts: 2,
        initial_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    })
}

/// A Pages publisher deploying through a runner that logs to `log`.
fn pages(log: &Log, stub: &StubPagesApi) -> PagesPublisher<StubRunner> {
    PagesPublisher::new(
        PublishConfig {
            pages_project: "test-cabinet-runs".to_string(),
        },
        StubRunner { log: log.clone() },
        stub.api(),
    )
}

/// The repository URL the repos stub answers.
const REPO_URL: &str = "https://github.com/TheClockwyrks/tcab-pong-codex-gpt-5-4-mini-d483a2f9";

/// The repos stub's success answer.
const REPO_ANSWER: &str =
    r#"{"url":"https://github.com/TheClockwyrks/tcab-pong-codex-gpt-5-4-mini-d483a2f9"}"#;

/// A minimal code-releasing run record for the fixture tree.
fn sample_record() -> RunRecord {
    RunRecord {
        id: "d483a2f9-7bda-4018-a27f-586ccdf31a9e".to_string(),
        started_at: "2026-06-15T01:36:06Z".to_string(),
        finished_at: "2026-06-15T01:40:25Z".to_string(),
        subject: RunSubject {
            test_case_slug: "pong".to_string(),
            test_case_version: "v1.0.0".to_string(),
            test_type: TestType::EndToEnd,
            variant: "base".to_string(),
            harness_slug: HarnessSlug::Codex,
            harness_version: Some("0.139.0".to_string()),
            orchestrator_slug: "one-shot".to_string(),
            engine_slug: "none".to_string(),
            engine_version: None,
            model_id: "gpt-5.4-mini".to_string(),
            gg_capability_set: None,
            gg_summary: None,
        },
        tooling: RunTooling {
            test_cabinet_commit: Some("0d60bc1deadbeef".to_string()),
        },
        environment: RunEnvironment {
            os: "Debian GNU/Linux 12 (bookworm)".to_string(),
            container_image: "test-cabinet/codex:1a7b".to_string(),
            node_version: Some("v22.11.0".to_string()),
            auth_mode: AuthMode::ApiKey,
        },
        metrics: RunMetrics {
            run_time_seconds: 258.4,
            tokens: TokenCounts {
                uncached_input: Some(41403),
                cached_input: Some(940416),
                output: Some(28733),
                reasoning: Some(7974),
            },
            cost: Cost {
                comparable: Some(0.2667),
                actual: Some(0.2667),
            },
            ..RunMetrics::default()
        },
        validation: ValidationSummary {
            debug_scripts: Vec::new(),
            loaded: true,
            requirements: Vec::new(),
            detail: None,
            install: None,
            build: None,
            checks: vec![],
            proofs: vec![],
            asset: None,
            adversarial: None,
            performance: None,
            voxel: None,
            ui: None,
            material: None,
            particle: None,
            audio: None,
        },
        links: RunLinks::default(),
        status: RunStatus {
            state: RunState::Completed,
            detail: None,
        },
        game_jam_readme: None,
        tool_calls: Default::default(),
        game_jam_prior_entries: Vec::new(),
        seed_commit: None,
        code_analysis: None,
        toolchain: None,
        showcase: None,
    }
}

/// Lay down a run directory under a temp dir: `run-record.json`, an
/// `implementation/` tree with a `dist/` build, and an `events.jsonl`. Returns the
/// temp dir (kept alive by the caller) and the run directory within it.
fn fixture_run_dir(record: &RunRecord) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let run_dir = temp.path().join(&record.id);
    let impl_dir = run_dir.join("implementation");
    std::fs::create_dir_all(impl_dir.join("dist")).expect("mkdir build");
    std::fs::write(
        run_dir.join("run-record.json"),
        serde_json::to_vec(record).expect("serialize record"),
    )
    .expect("write record");
    std::fs::write(impl_dir.join("dist/index.html"), b"<!doctype html>").expect("write build");
    std::fs::write(run_dir.join("events.jsonl"), b"").expect("write events");
    (temp, run_dir)
}

#[tokio::test]
async fn deploys_the_build_then_publishes_the_repository_and_returns_both_links() {
    let record = sample_record();
    let (_temp, run_dir) = fixture_run_dir(&record);
    let log = Log::default();
    let pages_api = StubPagesApi::with(Vec::new()).await;
    let repos = stub_repos(log.clone(), StatusCode::OK, REPO_ANSWER).await;

    let links = release(&run_dir, pages(&log, &pages_api), &repos)
        .await
        .expect("release should succeed");

    assert_eq!(links.source_repo.as_deref(), Some(REPO_URL));
    assert_eq!(
        links.playable_build.as_deref(),
        Some("https://abc123.test-cabinet-runs.pages.dev")
    );
    // The two steps ran in the order `components/backend/results.md` gives them:
    // the build was looked up and deployed before the repository was published.
    let log = log.lock().expect("lock").clone();
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log[0].starts_with("wrangler pages deploy"), "{log:?}");
    assert_eq!(log[1], format!("repos publish {}", record.id));
    assert_eq!(
        pages_api.requests().len(),
        1,
        "the deployment was looked up"
    );
}

#[tokio::test]
async fn an_asset_generation_run_deploys_but_publishes_no_repository() {
    let mut record = sample_record();
    record.subject.test_type = TestType::AssetGeneration;
    let (_temp, run_dir) = fixture_run_dir(&record);
    let log = Log::default();
    let pages_api = StubPagesApi::with(Vec::new()).await;
    let repos = stub_repos(log.clone(), StatusCode::OK, REPO_ANSWER).await;

    let links = release(&run_dir, pages(&log, &pages_api), &repos)
        .await
        .expect("release should succeed");

    assert_eq!(links.source_repo, None);
    assert!(links.playable_build.is_some());
    let log = log.lock().expect("lock").clone();
    assert!(
        !log.iter().any(|line| line.starts_with("repos ")),
        "the repos service was called: {log:?}"
    );
}

#[tokio::test]
async fn a_statistic_only_run_releases_nothing() {
    let mut record = sample_record();
    record.status.state = RunState::HarnessError;
    let (_temp, run_dir) = fixture_run_dir(&record);
    let log = Log::default();
    let pages_api = StubPagesApi::with(Vec::new()).await;
    let repos = stub_repos(log.clone(), StatusCode::OK, REPO_ANSWER).await;

    let links = release(&run_dir, pages(&log, &pages_api), &repos)
        .await
        .expect("release should succeed");

    assert_eq!(links.source_repo, None);
    assert_eq!(links.playable_build, None);
    assert!(log.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn a_null_url_is_reported_as_no_repository() {
    let record = sample_record();
    let (_temp, run_dir) = fixture_run_dir(&record);
    let log = Log::default();
    let pages_api = StubPagesApi::with(Vec::new()).await;
    let repos = stub_repos(log.clone(), StatusCode::OK, r#"{"url":null}"#).await;

    let links = release(&run_dir, pages(&log, &pages_api), &repos)
        .await
        .expect("release should succeed");

    assert_eq!(links.source_repo, None);
}

#[tokio::test]
async fn a_diverged_repository_fails_the_release_with_the_service_message() {
    let record = sample_record();
    let (_temp, run_dir) = fixture_run_dir(&record);
    let log = Log::default();
    let pages_api = StubPagesApi::with(Vec::new()).await;
    let repos = stub_repos(
        log.clone(),
        StatusCode::CONFLICT,
        r#"{"error":{"code":"conflict","message":"`TheClockwyrks/tcab-x` already holds a `main` whose tree differs from this run's scrubbed implementation; it is left as it is"}}"#,
    )
    .await;

    let err = release(&run_dir, pages(&log, &pages_api), &repos)
        .await
        .expect_err("a diverged repository fails");

    assert!(
        matches!(err, ReleaseError::Publish(crate::Error::Diverged(_))),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "releasing the run: publishing the run: `TheClockwyrks/tcab-x` already holds a \
         `main` whose tree differs from this run's scrubbed implementation; it is left as it is"
    );
}

#[tokio::test]
async fn a_missing_record_fails_cleanly() {
    let temp = tempfile::tempdir().expect("tempdir");
    let run_dir = temp.path().join("no-such-run");
    std::fs::create_dir_all(&run_dir).expect("mkdir");
    let log = Log::default();
    let pages_api = StubPagesApi::with(Vec::new()).await;
    let repos = stub_repos(log.clone(), StatusCode::OK, REPO_ANSWER).await;

    let err = release(&run_dir, pages(&log, &pages_api), &repos)
        .await
        .expect_err("a missing record must fail");

    assert!(
        matches!(err, ReleaseError::Record { .. }),
        "expected a Record error, got {err:?}"
    );
    assert!(log.lock().expect("lock").is_empty());
}
