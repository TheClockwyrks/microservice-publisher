//! The Cloudflare Pages half of a publish: deploying a run's playable build.
//!
//! See `components/backend/results.md`, "Publish". The `tcab-publisher` Job
//! deploys the run's already-built static output to the Cloudflare Pages project
//! under the run's branch alias, after scrubbing it, and reports the URL
//! `wrangler` gives it rather than constructing one, or reuses the deployment an
//! earlier attempt made of the same build (see [`crate::pages`]). The run's public
//! repository is the repos service's to create and push (see [`crate::repos`]),
//! so this module drives `wrangler` alone and the Job holds only the Cloudflare
//! credential.
//!
//! Every step is idempotent, so a publish repeated after a failure at any point
//! reaches the state its first run reached.

use std::borrow::Cow;
use std::path::Path;

use test_cabinet_contracts::run_record::RunRecord;
use test_cabinet_redact::SecretScrubber;

use crate::error::{Error, Result};
use crate::pages::{PagesApi, build_digest};
use crate::retry::{Backoff, retry};

/// Where the per-run builds are deployed.
///
/// Each run is deployed to one Cloudflare Pages project under its own branch
/// alias (`--branch=<run-id>`); the URL is read from `wrangler`'s output or the
/// deployment the lookup finds, never constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishConfig {
    /// The Cloudflare Pages project the per-run builds deploy to
    /// (`TCAB_PAGES_PROJECT`).
    pub pages_project: String,
}

impl PublishConfig {
    /// Resolve the publish configuration from the environment.
    ///
    /// `TCAB_PAGES_PROJECT` (the Cloudflare Pages project the per-run builds
    /// deploy to) is **required** and has no compiled-in fallback: every
    /// deployment sets it explicitly in its dispatcher overlay, so a misconfigured
    /// environment fails loudly here instead of silently deploying into another
    /// environment's project. A blank (or whitespace-only) value is treated as
    /// unset.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            pages_project: required_var("TCAB_PAGES_PROJECT")?,
        })
    }
}

/// Read a required publish-configuration variable, treating a blank (or
/// whitespace-only) value as unset and reporting a clear [`Error::Publish`] when it
/// is missing, so a half-configured deployment fails loudly rather than deploying
/// into a surprising Pages project.
fn required_var(key: &str) -> Result<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::Publish(format!(
                "required publish configuration variable `{key}` is unset or empty"
            ))
        })
}

/// The captured result of running an external command.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Whether the process exited successfully.
    pub success: bool,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl CommandOutput {
    /// Human-readable diagnostics for a failed command, drawn from *both*
    /// streams.
    ///
    /// `wrangler` prints most of its diagnostics to stdout, so a stderr-only
    /// message renders empty exactly when a deploy fails and the operator most
    /// needs to see why. Surface whatever each stream carried, labelling them only
    /// when both are non-empty, and say so explicitly when a failing command
    /// produced nothing at all.
    fn failure_details(&self) -> String {
        let stdout = self.stdout.trim();
        let stderr = self.stderr.trim();
        match (stderr.is_empty(), stdout.is_empty()) {
            (true, true) => "(no output captured)".to_string(),
            (false, true) => stderr.to_string(),
            (true, false) => stdout.to_string(),
            (false, false) => format!("{stderr}\n{stdout}"),
        }
    }
}

/// Runs the external command the Pages deploy needs (`wrangler`).
///
/// Abstracting process execution behind a trait keeps the deploy testable
/// without touching the network or Cloudflare.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args` in the inherited environment and capture its
    /// output.
    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput>;
}

/// A [`CommandRunner`] that executes commands as real subprocesses.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCommandRunner;

#[async_trait::async_trait]
impl CommandRunner for SystemCommandRunner {
    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .map_err(|err| Error::Publish(format!("failed to run `{program}`: {err}")))?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Deploys runs' playable builds to Cloudflare Pages.
///
/// `wrangler` is driven through a [`CommandRunner`] and the deployment is looked
/// up in a [`PagesApi`]; both seams are swappable so the deploy can be tested
/// without touching the network.
pub struct PagesPublisher<R: CommandRunner> {
    config: PublishConfig,
    runner: R,
    /// How the Pages deploy retries a transient failure (see [`crate::retry`]).
    retry: Backoff,
    /// The Cloudflare API the deploy looks its deployment up in (see
    /// [`deploy_pages_build`]).
    pages_api: PagesApi,
}

impl<R: CommandRunner> PagesPublisher<R> {
    /// Construct a publisher deploying to `config`'s project through `runner`,
    /// looking each build's deployment up in `pages_api` first so a repeated
    /// deploy reuses the deployment it already made.
    pub fn new(config: PublishConfig, runner: R, pages_api: PagesApi) -> Self {
        Self {
            config,
            runner,
            retry: Backoff::default(),
            pages_api,
        }
    }

    /// Retry the Pages deploy with `backoff` instead of [`Backoff::default`].
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.retry = backoff;
        self
    }

    /// The deploy configuration this publisher uses.
    pub fn config(&self) -> &PublishConfig {
        &self.config
    }

    /// The command runner this publisher drives `wrangler` through.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// Deploy `record`'s playable build from `build_dir`, returning the URL it is
    /// served at, or `None` when there is no build to deploy or the run's state
    /// releases nothing.
    pub async fn release_playable_build(
        &self,
        record: &RunRecord,
        build_dir: Option<&Path>,
    ) -> Result<Option<String>> {
        // A harness-error run releases nothing (it is a per-model statistic only), so
        // no build is deployed even if a partial tree happened to build; see
        // `RunState::publishes_artifacts`.
        if !record.status.state.publishes_artifacts() {
            return Ok(None);
        }
        let Some(build_dir) = build_dir else {
            return Ok(None);
        };
        // Deploy the already-built static output to Cloudflare Pages under a
        // per-run branch alias, redacting any leaked API key from the built tree
        // first (a key the model wrote into a source file can have been carried
        // through the build into an emitted asset). The branch alias is the run
        // id. A deployment of the same scrubbed build on that branch is reused,
        // so the run keeps one deployment however often the step repeats; see
        // [`deploy_pages_build`].
        let url = deploy_pages_build(
            &self.runner,
            &self.pages_api,
            build_dir,
            &self.config.pages_project,
            &record.id,
            &self.retry,
        )
        .await?;
        Ok(Some(url))
    }
}

/// Deploy an already-built static `build_dir` to a Cloudflare Pages `project`
/// under `branch`, or find the deployment an earlier attempt made of the same
/// build, returning the URL it is served at.
///
/// This is the playable-build step of a publish (branch = the run id). It makes
/// one deployment per run, however often it is repeated (see
/// `components/backend/results.md`, "Repeating a step"). The steps, in order:
///
/// 1. **Redact.** `scrub_build_dir` rewrites any leaked provider API key out of
///    every text file under `build_dir` in place, so a key the model left in a
///    source file cannot ship to the public site through a built asset.
/// 2. **Digest.** [`build_digest`] hashes the scrubbed tree. The digest is the
///    deployment's commit hash, so a deployment records which build it serves.
/// 3. **Look up.** [`PagesApi::find_deployment`] searches the project for a
///    successful deployment on `branch` carrying the digest. One that exists is
///    reused: its URL is returned and nothing is deployed.
/// 4. **Deploy.** Otherwise `wrangler pages deploy <build_dir> --project-name
///    <project> --branch=<branch> --commit-hash=<digest>` is run through `runner`
///    (a [`SystemCommandRunner`] in production, a mock in tests). A non-zero exit
///    is surfaced with whatever wrangler wrote to *either* stream, since it prints
///    most diagnostics to stdout.
/// 5. **Read the URL back.** The deployed URL is parsed out of wrangler's output
///    ([`parse_wrangler_url`]) rather than constructed: Cloudflare sanitizes and
///    truncates long branch-alias subdomains, so `https://<branch>.<project>.pages.dev`
///    is *not* a reliable address. The URL wrangler reports is the deployment's
///    own (`https://<short id>.<project>.pages.dev`), the same URL the lookup
///    returns for it.
///
/// Steps 3 to 5 are retried together with `backoff` on a transient failure (see
/// [`crate::retry`]): a lookup that got no answer or a `408`/`429`/`5xx`, or a
/// failed `wrangler` run. Each retry looks again first, so a deploy that landed
/// before its run failed is found rather than repeated.
pub async fn deploy_pages_build(
    runner: &impl CommandRunner,
    pages_api: &PagesApi,
    build_dir: &Path,
    pages_project: &str,
    branch: &str,
    backoff: &Backoff,
) -> Result<String> {
    scrub_build_dir(build_dir)?;
    let digest = build_digest(build_dir)?;
    let dir = build_dir
        .to_str()
        .ok_or_else(|| Error::Publish("build directory path is not valid UTF-8".to_string()))?;
    retry(backoff, "the Pages deploy", || {
        find_or_deploy(runner, pages_api, dir, pages_project, branch, &digest)
    })
    .await
}

/// One attempt of [`deploy_pages_build`]'s lookup and deploy of the scrubbed
/// build at `dir`, whose digest is `digest`.
async fn find_or_deploy(
    runner: &impl CommandRunner,
    pages_api: &PagesApi,
    dir: &str,
    pages_project: &str,
    branch: &str,
    digest: &str,
) -> Result<String> {
    if let Some(url) = pages_api
        .find_deployment(pages_project, branch, digest)
        .await?
    {
        tracing::info!(%url, %branch, %digest, "reusing the build's existing Pages deployment");
        return Ok(url);
    }
    let branch_arg = format!("--branch={branch}");
    let commit_hash_arg = format!("--commit-hash={digest}");
    let output = runner
        .run(
            "wrangler",
            &[
                "pages",
                "deploy",
                dir,
                "--project-name",
                pages_project,
                &branch_arg,
                &commit_hash_arg,
            ],
        )
        .await?;
    if !output.success {
        return Err(Error::Command(format!(
            "`wrangler pages deploy` failed: {}",
            output.failure_details()
        )));
    }
    parse_wrangler_url(&output.stdout)
        .or_else(|| parse_wrangler_url(&output.stderr))
        .ok_or_else(|| {
            Error::Publish(
                "could not find a deployment URL in `wrangler pages deploy` output".to_string(),
            )
        })
}

/// Extract the deployment URL `wrangler pages deploy` reports from its output.
///
/// Wrangler prints a line containing the deployment URL — for example
/// `✨ Deployment complete! Take a peek over at https://abc123.test-cabinet-runs.pages.dev`.
/// The host is sanitized/truncated by Cloudflare, so the reported URL is the only
/// reliable source; this scans for the first `https://…pages.dev` token. Returns
/// the URL with surrounding punctuation trimmed, or `None` when none is found.
pub fn parse_wrangler_url(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|token| {
        // A token may carry leading framing (an emoji, a quote) before the URL;
        // start at the scheme and trim trailing sentence punctuation.
        let start = token.find("https://")?;
        let url = token[start..].trim_end_matches(['.', ',', ')', '"', '\'']);
        if url.contains("pages.dev") {
            Some(url.to_string())
        } else {
            None
        }
    })
}

/// Redact any leaked provider API key from every text file under an
/// already-built static `build_dir` before it is deployed to the public
/// Cloudflare Pages site.
///
/// This is the Cloudflare-egress half of the publish-time secret scrub; the repos
/// service scrubs the source it pushes (see `components/repos/overview.md`,
/// Publishing). The scrubber matches the exact key values from the publisher's
/// environment as well as any `sk-…`-shaped token. A built tree has no
/// `.gitignore` staging to lean on, so it is walked in full; binary, symlinked,
/// and unreadable entries are skipped (a key is pasted as text). Rewritten files
/// are logged.
fn scrub_build_dir(build_dir: &Path) -> Result<()> {
    let scrubber = SecretScrubber::from_host_env();
    let mut redacted = Vec::new();
    scrub_dir_recursive(build_dir, &scrubber, &mut redacted)?;
    if !redacted.is_empty() {
        tracing::warn!(
            files = ?redacted,
            "redacted leaked API key(s) from a run's playable build before deploying it publicly"
        );
    }
    Ok(())
}

/// Recursively scrub every readable UTF-8 file under `dir` in place, appending
/// the path of each file actually rewritten to `redacted`. Directories are
/// descended; symlinks are not followed (so a link cannot redirect the walk out
/// of the build tree), and non-UTF-8 or unreadable files are left untouched.
fn scrub_dir_recursive(
    dir: &Path,
    scrubber: &SecretScrubber,
    redacted: &mut Vec<String>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Error::Publish(format!("reading build directory {}: {e}", dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| Error::Publish(format!("reading a build directory entry: {e}")))?;
        let path = entry.path();
        // `file_type` does not follow symlinks, so a symlink is neither a file
        // nor a directory here and falls through untouched.
        let file_type = entry
            .file_type()
            .map_err(|e| Error::Publish(format!("inspecting {}: {e}", path.display())))?;
        if file_type.is_dir() {
            scrub_dir_recursive(&path, scrubber, redacted)?;
        } else if file_type.is_file() {
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Cow::Owned(scrubbed) = scrubber.scrub(&contents) {
                std::fs::write(&path, scrubbed).map_err(|e| {
                    Error::Publish(format!(
                        "rewriting {} to redact a secret: {e}",
                        path.display()
                    ))
                })?;
                redacted.push(path.display().to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "release.test.rs"]
mod tests;
