//! Releasing a downloaded run: the Cloudflare Pages deploy and the repository
//! publish.
//!
//! Given a run directory untarred from the artifact service
//! (`run-record.json` + `implementation/` + `events.jsonl`), this loads the
//! record, finds the build and drives the two release steps in the order
//! `components/backend/results.md` ("Publish") gives them: the Pages deploy of
//! the playable build ([`PagesPublisher`], `wrangler`) first, then the repository
//! publish, which the repos service performs when the publisher calls its
//! publish route ([`ReposClient`]). It releases only: the link-attach and the
//! publish-flip are the backend's job once it receives the terminal result, and
//! the run record was already stored by the driver when the run finished.
//!
//! Secret scrubbing is inside both steps: the Pages deploy redacts any leaked
//! provider key from the built output before it deploys it, and the repos
//! service scrubs the source it pushes.

use std::path::{Path, PathBuf};

use test_cabinet_contracts::layout::find_build_output;
use test_cabinet_contracts::run_record::RunRecord;

use crate::pages::PagesApi;
use crate::release::{CommandRunner, PagesPublisher, PublishConfig, SystemCommandRunner};
use crate::repos::ReposClient;

/// The links a successful release produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedLinks {
    /// The public source-repo URL, or `None` for a run that releases no code (an
    /// asset-generation run creates no repository).
    pub source_repo: Option<String>,
    /// The deployed playable-build URL, or `None` when the run produced no build.
    pub playable_build: Option<String>,
}

/// A failure releasing a downloaded run.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The run record could not be read or parsed from the downloaded tree.
    #[error("reading the run record at `{path}`: {detail}")]
    Record {
        /// The `run-record.json` path that could not be loaded.
        path: String,
        /// Why it failed (an I/O or JSON-parse error).
        detail: String,
    },
    /// A release step (the Pages deploy or the repository publish) failed.
    #[error("releasing the run: {0}")]
    Publish(#[source] crate::Error),
}

/// Read and parse a run record from `run_dir/run-record.json`.
pub fn load_record(run_dir: &Path) -> Result<RunRecord, ReleaseError> {
    let path = run_dir.join("run-record.json");
    let bytes = std::fs::read(&path).map_err(|err| ReleaseError::Record {
        path: path.display().to_string(),
        detail: err.to_string(),
    })?;
    serde_json::from_slice(&bytes).map_err(|err| ReleaseError::Record {
        path: path.display().to_string(),
        detail: err.to_string(),
    })
}

/// Whether `record`'s run releases its source to a public repository: a state
/// that publishes artifacts (a harness-error or hung run is a statistic only; see
/// `RunState::publishes_artifacts`) of a type that writes code (an
/// asset-generation run's output is its recorded operations; see
/// `TestType::releases_source_repo`). The repository publish is skipped for any
/// other run, which carries no source link.
pub fn releases_source_repo(record: &RunRecord) -> bool {
    record.status.state.publishes_artifacts() && record.subject.test_type.releases_source_repo()
}

/// A downloaded run made ready for its two release steps: its record loaded, its
/// build found and the [`PagesPublisher`] that deploys it built. The publisher's
/// sequence runs [`PreparedRelease::deploy`] and then
/// [`PreparedRelease::publish_repository`] as two steps, so that it can stop
/// between them (see [`crate::sequence`]).
pub struct PreparedRelease<R: CommandRunner> {
    record: RunRecord,
    build_dir: Option<PathBuf>,
    pages: PagesPublisher<R>,
}

impl<R: CommandRunner> PreparedRelease<R> {
    /// Prepare the run downloaded and untarred under `run_dir`, using `runner` to
    /// drive `wrangler` and `pages_api` to look the build's deployment up.
    ///
    /// A [`PublishConfig::from_env`] resolves the Pages project from the
    /// `TCAB_PAGES_PROJECT` the Job forwards. It is required, so a publish Job
    /// that was not handed its target fails here (as a [`ReleaseError::Publish`])
    /// rather than deploying to a default project.
    pub fn new(run_dir: &Path, runner: R, pages_api: PagesApi) -> Result<Self, ReleaseError> {
        let config = PublishConfig::from_env().map_err(ReleaseError::Publish)?;
        Self::with_publisher(run_dir, PagesPublisher::new(config, runner, pages_api))
    }

    /// Prepare the run downloaded under `run_dir`, deploying through `pages`.
    pub fn with_publisher(run_dir: &Path, pages: PagesPublisher<R>) -> Result<Self, ReleaseError> {
        let record = load_record(run_dir)?;
        // The build directory found within the collected implementation tree.
        let build_dir = find_build_output(&run_dir.join("implementation"));
        Ok(Self {
            record,
            build_dir,
            pages,
        })
    }

    /// The Pages deploy: deploy (or reuse the deployment of) the run's playable
    /// build, returning its URL, or `None` when the run has no build to deploy.
    pub async fn deploy(&self) -> Result<Option<String>, ReleaseError> {
        self.pages
            .release_playable_build(&self.record, self.build_dir.as_deref())
            .await
            .map_err(ReleaseError::Publish)
    }

    /// The repository publish: have the repos service publish the run's
    /// implementation to its public repository through `repos`, returning the
    /// repository URL, or `None` for a run that releases no source, for which
    /// the service is not called (see [`releases_source_repo`]).
    pub async fn publish_repository(
        &self,
        repos: &ReposClient,
    ) -> Result<Option<String>, ReleaseError> {
        if !releases_source_repo(&self.record) {
            return Ok(None);
        }
        repos.publish().await.map_err(ReleaseError::Publish)
    }
}

impl PreparedRelease<SystemCommandRunner> {
    /// Prepare a downloaded run with the real [`SystemCommandRunner`] (`wrangler`
    /// as a subprocess) and the Cloudflare API the Job's
    /// `CLOUDFLARE_ACCOUNT_ID`/`CLOUDFLARE_API_TOKEN` authenticate to (see
    /// [`PagesApi::from_env`]).
    pub fn from_env(run_dir: &Path) -> Result<Self, ReleaseError> {
        let pages_api = PagesApi::from_env().map_err(ReleaseError::Publish)?;
        Self::new(run_dir, SystemCommandRunner, pages_api)
    }
}

/// Release a run that has already been downloaded and untarred under `run_dir`:
/// the Pages deploy through `pages`, then the repository publish through
/// `repos`, the order `components/backend/results.md` ("Publish") gives the
/// publisher's steps.
///
/// The publisher's sequence runs the two steps itself (see
/// [`crate::sequence`]); this runs them back to back.
pub async fn release<R: CommandRunner>(
    run_dir: &Path,
    pages: PagesPublisher<R>,
    repos: &ReposClient,
) -> Result<ReleasedLinks, ReleaseError> {
    let prepared = PreparedRelease::with_publisher(run_dir, pages)?;
    let playable_build = prepared.deploy().await?;
    let source_repo = prepared.publish_repository(repos).await?;
    Ok(ReleasedLinks {
        source_repo,
        playable_build,
    })
}

#[cfg(test)]
#[path = "publish.test.rs"]
mod tests;
