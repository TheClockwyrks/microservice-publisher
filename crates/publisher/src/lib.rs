//! The Test Cabinet publisher: a one-shot per-publish-Job binary.
//!
//! The publisher is the publish-path counterpart of the driver
//! (`test-cabinet-driver`): where the driver executes one *run* and streams it
//! back, the publisher releases one *already-reviewed run* — a Cloudflare Pages
//! deploy of its playable build and, through the repos service, its own public
//! GitHub repository — and streams the publish progress + terminal outcome back to
//! the backend, then exits. The dispatcher creates one publisher Job per claimed
//! publish job; everything it needs arrives through `TCAB_*` env (see [`config`]).
//!
//! The Pages deploy is
//! [`release_playable_build`](release::PagesPublisher::release_playable_build) (see
//! [`release`], and [`pages`] for the deployment lookup), and the repository
//! publish is the repos service's `POST /runs/{id}/publish`, which
//! [`ReposClient`](repos::ReposClient) calls (see [`repos`]); they run in that
//! order (see [`publish`]). The inputs are downloaded from the artifact service
//! (see [`download`]), and the outcome is reported over the publish-job token (see
//! [`client`]). Each of those steps retries a transient failure with backoff (see
//! [`retry`]); [`sequence`] runs them in order and stops them on `SIGTERM`.
//!
//! There is no app-level auth on the publisher itself — it is a client, not a
//! server. Its calls to the backend, the artifact service and the repos service
//! authenticate with the per-publish-job token the dispatcher passed in (see
//! [`client`]). Its one credential is the Cloudflare one the Job's `envFrom`
//! injects: it reads `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` itself to
//! look the build's Pages deployment up before `wrangler`, which reads them too,
//! deploys it (see [`pages`]). The GitHub credential is the repos service's own.

pub mod client;
pub mod config;
pub mod download;
pub mod error;
pub mod pages;
pub mod publish;
pub mod release;
pub mod repos;
pub mod retry;
pub mod sequence;

pub use error::{Error, Result};
