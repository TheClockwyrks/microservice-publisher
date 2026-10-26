//! Tests for the Pages half of a publish: the deploy configuration, wrangler URL
//! capture, the deploy with its lookup and retries, and the build scrub, driven
//! through a mock command runner and a stub Cloudflare API so no real `wrangler`
//! or network is touched.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use super::*;
use crate::pages::tests::{StubPagesApi, deployment};
use test_cabinet_contracts::metrics::{Cost, RunMetrics, TokenCounts};
use test_cabinet_contracts::run_record::{
    HarnessSlug, RunEnvironment, RunLinks, RunState, RunStatus, RunSubject, RunTooling,
};
use test_cabinet_contracts::validation::ValidationSummary;

fn sample_record() -> RunRecord {
    RunRecord {
        id: "d483a2f9-7bda-4018-a27f-586ccdf31a9e".to_string(),
        started_at: "2026-06-15T01:36:06Z".to_string(),
        finished_at: "2026-06-15T01:40:25Z".to_string(),
        subject: RunSubject {
            test_case_slug: "pong".to_string(),
            test_case_version: "v1.0.0".to_string(),
            test_type: test_cabinet_contracts::test_case::TestType::EndToEnd,
            variant: "base".to_string(),
            harness_slug: HarnessSlug::Codex,
            harness_version: Some("0.139.0".to_string()),
            orchestrator_slug: "one-shot".to_string(),
            engine_slug: "none".to_string(),
            engine_version: None,
            // Dots are not repo-name-safe; the slug must reduce them to hyphens.
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
            auth_mode: test_cabinet_contracts::run_record::AuthMode::ApiKey,
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
            voxel: None,
            ui: None,
            material: None,
            particle: None,
            audio: None,
            adversarial: None,
            performance: None,
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

/// A publish configuration matching the production Pages project.
/// [`PublishConfig`] has no `Default` and no compiled-in fallback: every
/// environment sets its Pages project explicitly (see
/// [`PublishConfig::from_env`]), so the tests spell it out here.
fn sample_config() -> PublishConfig {
    PublishConfig {
        pages_project: "test-cabinet-runs".to_string(),
    }
}

static ENV_GUARD: Mutex<()> = Mutex::new(());

#[test]
fn from_env_requires_the_pages_project() {
    let _guard = ENV_GUARD
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    // Unset, there is no compiled-in fallback: resolution fails loudly rather
    // than deploying into a default project.
    unsafe {
        std::env::remove_var("TCAB_PAGES_PROJECT");
    }
    assert!(matches!(PublishConfig::from_env(), Err(Error::Publish(_))));

    unsafe {
        std::env::set_var("TCAB_PAGES_PROJECT", "acme-runs");
    }
    let resolved = PublishConfig::from_env().expect("the variable is set");
    assert_eq!(resolved.pages_project, "acme-runs");

    // A blank value is treated as unset.
    unsafe {
        std::env::set_var("TCAB_PAGES_PROJECT", "  ");
    }
    assert!(matches!(PublishConfig::from_env(), Err(Error::Publish(_))));

    unsafe {
        std::env::remove_var("TCAB_PAGES_PROJECT");
    }
}

#[test]
fn wrangler_url_is_captured_not_constructed() {
    // The deployment URL is whatever wrangler reports — a sanitized/truncated
    // branch-alias host, not `https://<run-id>.<project>.pages.dev`.
    let stdout = "Uploading... (3/3)\n\
         ✨ Deployment complete! Take a peek over at https://abc123.test-cabinet-runs.pages.dev\n";
    assert_eq!(
        parse_wrangler_url(stdout).as_deref(),
        Some("https://abc123.test-cabinet-runs.pages.dev")
    );
    // Trailing sentence punctuation is trimmed.
    assert_eq!(
        parse_wrangler_url("Deployed to https://x.pages.dev.").as_deref(),
        Some("https://x.pages.dev")
    );
    // A line without a pages.dev URL yields nothing.
    assert_eq!(parse_wrangler_url("nothing here"), None);
}

/// A [`CommandRunner`] that records every invocation and answers `wrangler`
/// with a deployment URL, after failing the first scripted runs.
struct MockRunner {
    /// How many leading `wrangler` runs fail before one succeeds.
    wrangler_failures: Mutex<u32>,
    calls: Mutex<Vec<String>>,
}

impl MockRunner {
    fn new() -> Self {
        Self {
            wrangler_failures: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Make the first `n` `wrangler` runs fail before one succeeds.
    fn failing_first_deploys(mut self, n: u32) -> Self {
        self.wrangler_failures = Mutex::new(n);
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl CommandRunner for MockRunner {
    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
        self.calls
            .lock()
            .expect("lock")
            .push(format!("{program} {}", args.join(" ")));
        let fails = program == "wrangler" && {
            let mut remaining = self.wrangler_failures.lock().expect("lock");
            (*remaining > 0).then(|| *remaining -= 1).is_some()
        };
        let stdout = if fails {
            "✘ [ERROR] A request to the Cloudflare API failed.\n"
        } else {
            "✨ Deployment complete! https://abc123.test-cabinet-runs.pages.dev\n"
        };
        Ok(CommandOutput {
            success: !fails,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }
}

/// Five attempts with no pause between them, so the retry paths run without
/// sleeping.
fn instant_backoff() -> Backoff {
    Backoff {
        attempts: 5,
        initial_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    }
}

/// The empty build directory under `dir` a test deploys.
fn build_dir_in(dir: &Path) -> PathBuf {
    let build_dir = dir.join("dist");
    std::fs::create_dir_all(&build_dir).expect("build dir");
    build_dir
}

/// A publisher deploying through `runner` with lookups in `stub`, retrying
/// without pauses: the retry *count* is kept so the retry paths stay exercised.
fn publisher(runner: MockRunner, stub: &StubPagesApi) -> PagesPublisher<MockRunner> {
    PagesPublisher::new(sample_config(), runner, stub.api()).with_backoff(instant_backoff())
}

/// Release `build_dir`'s playable build through `runner`, looking its
/// deployment up in `stub`.
async fn release_build_against(
    runner: MockRunner,
    stub: &StubPagesApi,
    build_dir: &Path,
) -> (Result<Option<String>>, Vec<String>) {
    let publisher = publisher(runner, stub);
    let record = sample_record();
    let result = publisher
        .release_playable_build(&record, Some(build_dir))
        .await;
    (result, publisher.runner().calls())
}

#[tokio::test]
async fn release_playable_build_deploys_with_the_digest_and_captures_the_wrangler_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = MockRunner::new();
    let build_dir = build_dir_in(dir.path());
    std::fs::write(build_dir.join("index.html"), b"<!doctype html>").expect("write build");
    let digest = build_digest(&build_dir).expect("digest");
    let record = sample_record();
    // The project holds deployments of this run's branch with another build and
    // of this build on another branch; neither is reused.
    let stub = StubPagesApi::with(vec![
        deployment(
            "https://other.test-cabinet-runs.pages.dev",
            &record.id,
            &"0".repeat(64),
        ),
        deployment(
            "https://elsewhere.test-cabinet-runs.pages.dev",
            "another-run",
            &digest,
        ),
    ])
    .await;

    let (playable_build, calls) = release_build_against(runner, &stub, &build_dir).await;

    // The playable build URL is the one wrangler reported, not a constructed host.
    assert_eq!(
        playable_build.expect("release build").as_deref(),
        Some("https://abc123.test-cabinet-runs.pages.dev")
    );
    // The build was looked up, then deployed to Cloudflare Pages under the run-id
    // branch alias, carrying the scrubbed build's digest as its commit hash.
    assert_eq!(stub.requests().len(), 1);
    let deploys: Vec<_> = calls
        .iter()
        .filter(|c| c.contains("wrangler pages deploy"))
        .collect();
    assert_eq!(deploys.len(), 1, "{calls:?}");
    assert!(
        deploys[0].contains(&format!("--branch={}", record.id))
            && deploys[0].contains(&format!("--commit-hash={digest}"))
            && deploys[0].contains("--project-name test-cabinet-runs"),
        "{calls:?}"
    );
}

#[tokio::test]
async fn release_playable_build_reuses_the_deployment_of_the_same_build() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = MockRunner::new();
    let build_dir = build_dir_in(dir.path());
    std::fs::write(build_dir.join("index.html"), b"<!doctype html>").expect("write build");
    let digest = build_digest(&build_dir).expect("digest");
    let record = sample_record();
    let stub = StubPagesApi::with(vec![deployment(
        "https://3f6fa159.test-cabinet-runs.pages.dev",
        &record.id,
        &digest,
    )])
    .await;

    let (playable_build, calls) = release_build_against(runner, &stub, &build_dir).await;

    // The deployment an earlier attempt made is reported, and nothing is deployed.
    assert_eq!(
        playable_build.expect("release build").as_deref(),
        Some("https://3f6fa159.test-cabinet-runs.pages.dev")
    );
    assert!(!calls.iter().any(|c| c.contains("wrangler")), "{calls:?}");
}

#[tokio::test]
async fn release_playable_build_digests_the_scrubbed_build() {
    // A leaked key is scrubbed before the digest is taken, so the digest the
    // lookup searches for is that of the tree actually deployed.
    let key = "sk-proj-DATA4digestSCRUBtest0123456789abcdefABCDEF";
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = MockRunner::new();
    let build_dir = build_dir_in(dir.path());
    std::fs::write(build_dir.join("app.js"), format!("const k = \"{key}\";")).expect("write");
    let stub = StubPagesApi::with(Vec::new()).await;

    let (playable_build, calls) = release_build_against(runner, &stub, &build_dir).await;
    playable_build.expect("release build");

    let scrubbed = std::fs::read_to_string(build_dir.join("app.js")).expect("read");
    assert!(!scrubbed.contains(key), "{scrubbed}");
    let digest = build_digest(&build_dir).expect("digest");
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&format!("--commit-hash={digest}"))),
        "{calls:?}"
    );
}

#[tokio::test]
async fn release_playable_build_fails_when_the_lookup_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = MockRunner::new();
    let build_dir = build_dir_in(dir.path());
    let stub = StubPagesApi::serve(Vec::new(), axum::http::StatusCode::BAD_GATEWAY).await;

    let (playable_build, calls) = release_build_against(runner, &stub, &build_dir).await;

    assert!(
        matches!(
            playable_build,
            Err(Error::PagesApi {
                status: Some(502),
                ..
            })
        ),
        "{playable_build:?}"
    );
    assert!(!calls.iter().any(|c| c.contains("wrangler")), "{calls:?}");
    // A 502 is transient: the lookup was made once per attempt.
    assert_eq!(stub.requests().len(), 5);
}

#[tokio::test]
async fn release_playable_build_fails_at_once_when_the_lookup_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = MockRunner::new();
    let build_dir = build_dir_in(dir.path());
    let stub = StubPagesApi::serve(Vec::new(), axum::http::StatusCode::FORBIDDEN).await;

    let (playable_build, calls) = release_build_against(runner, &stub, &build_dir).await;

    assert!(
        matches!(
            playable_build,
            Err(Error::PagesApi {
                status: Some(403),
                ..
            })
        ),
        "{playable_build:?}"
    );
    assert_eq!(stub.requests().len(), 1, "a 403 is not retried");
    assert!(!calls.iter().any(|c| c.contains("wrangler")), "{calls:?}");
}

#[tokio::test]
async fn a_failed_wrangler_run_is_retried_after_looking_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = MockRunner::new().failing_first_deploys(1);
    let build_dir = build_dir_in(dir.path());
    let stub = StubPagesApi::with(Vec::new()).await;

    let (playable_build, calls) = release_build_against(runner, &stub, &build_dir).await;

    assert_eq!(
        playable_build.expect("the second deploy lands").as_deref(),
        Some("https://abc123.test-cabinet-runs.pages.dev")
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("wrangler ")).count(),
        2,
        "{calls:?}"
    );
    // Each attempt looked the deployment up before deploying.
    assert_eq!(stub.requests().len(), 2);
}

#[tokio::test]
async fn a_wrangler_run_that_keeps_failing_is_bounded_by_the_attempts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = MockRunner::new().failing_first_deploys(99);
    let build_dir = build_dir_in(dir.path());
    let stub = StubPagesApi::with(Vec::new()).await;

    let (playable_build, calls) = release_build_against(runner, &stub, &build_dir).await;

    let err = playable_build.expect_err("every deploy fails");
    assert!(matches!(err, Error::Command(_)), "{err}");
    assert!(err.to_string().contains("Cloudflare API failed"), "{err}");
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("wrangler ")).count(),
        5,
        "{calls:?}"
    );
}

#[tokio::test]
async fn release_playable_build_of_an_artifactless_failure_deploys_nothing() {
    // Even if a partial tree happened to build, a harness-error or hung run deploys
    // no playable build — it is a per-model statistic only.
    for state in [RunState::HarnessError, RunState::Hung] {
        let dir = tempfile::tempdir().expect("tempdir");
        let build_dir = build_dir_in(dir.path());
        let stub = StubPagesApi::with(Vec::new()).await;
        let publisher = publisher(MockRunner::new(), &stub);
        let mut record = sample_record();
        record.status.state = state;

        let playable_build = publisher
            .release_playable_build(&record, Some(&build_dir))
            .await
            .expect("release build");

        assert_eq!(playable_build, None, "{state:?} deployed a build");
        let calls = publisher.runner().calls();
        assert!(
            !calls.iter().any(|c| c.contains("wrangler")),
            "{state:?} release must not deploy a build: {calls:?}"
        );
        assert!(stub.requests().is_empty());
    }
}

#[tokio::test]
async fn release_playable_build_without_a_build_dir_skips_the_deploy() {
    let stub = StubPagesApi::with(Vec::new()).await;
    let publisher = publisher(MockRunner::new(), &stub);
    let record = sample_record();

    let playable_build = publisher
        .release_playable_build(&record, None)
        .await
        .expect("release build");

    assert!(playable_build.is_none());
    let calls = publisher.runner().calls();
    assert!(!calls.iter().any(|c| c.contains("wrangler")));
}

#[test]
fn failure_details_prefers_stdout_when_stderr_is_empty() {
    // `wrangler` writes its diagnostics to stdout, leaving stderr empty — the
    // exact case that previously rendered the error as `… failed: ` with nothing
    // after it.
    let output = CommandOutput {
        success: false,
        stdout: "  Authentication error [code: 10000]\n".to_string(),
        stderr: String::new(),
    };
    assert_eq!(
        output.failure_details(),
        "Authentication error [code: 10000]"
    );
}

#[test]
fn failure_details_prefers_stderr_when_stdout_is_empty() {
    let output = CommandOutput {
        success: false,
        stdout: String::new(),
        stderr: "  ✘ [ERROR] Project not found\n".to_string(),
    };
    assert_eq!(output.failure_details(), "✘ [ERROR] Project not found");
}

#[test]
fn failure_details_combines_both_streams_when_present() {
    let output = CommandOutput {
        success: false,
        stdout: "deploy output\n".to_string(),
        stderr: "a warning\n".to_string(),
    };
    assert_eq!(output.failure_details(), "a warning\ndeploy output");
}

#[test]
fn failure_details_reports_when_no_output_was_captured() {
    let output = CommandOutput {
        success: false,
        stdout: "   \n".to_string(),
        stderr: String::new(),
    };
    assert_eq!(output.failure_details(), "(no output captured)");
}

#[test]
fn scrub_build_dir_redacts_keys_in_nested_text_files_only() {
    let key = "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789AbCdEf01";
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("assets");
    std::fs::create_dir_all(&nested).expect("nested dir");

    // A bundled JS chunk that captured the key, a clean HTML file, and a binary
    // asset that merely contains the key bytes (and must be left untouched).
    let leaky = nested.join("app.js");
    std::fs::write(&leaky, format!("const K=\"{key}\";export default K;")).expect("js");
    let clean = dir.path().join("index.html");
    std::fs::write(&clean, "<!doctype html><title>play</title>").expect("html");
    let binary = nested.join("logo.png");
    let binary_bytes = [0x89, b'P', b'N', b'G', 0x00, 0xFF, 0xFE];
    std::fs::write(&binary, binary_bytes).expect("png");

    scrub_build_dir(dir.path()).expect("scrub");

    let scrubbed = std::fs::read_to_string(&leaky).expect("read js");
    assert!(!scrubbed.contains(key), "the key must be gone");
    assert!(scrubbed.contains(test_cabinet_redact::PLACEHOLDER));
    // Untouched files keep their exact bytes.
    assert_eq!(
        std::fs::read_to_string(&clean).expect("read html"),
        "<!doctype html><title>play</title>"
    );
    assert_eq!(std::fs::read(&binary).expect("read png"), binary_bytes);
}
