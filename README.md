# microservice-publisher

The publisher: the per-publish Job that releases a reviewed run to Cloudflare
Pages and GitHub.

This repository is one component of
[The Test Cabinet](https://github.com/TheClockwyrks/TheTestCabinet). What it is
and how it fits with the others is stated by the documentation site, which
lives in the superrepo at `apps/docs/src/content/docs/`. Its Repositories page,
`apps/docs/src/content/docs/development/repositories.md`, draws the edges
between the repositories.

## The service

`crates/publisher` is `tcab-publisher`, the publisher: the one-shot binary of
the per-publish Job the dispatcher admits for each publish of a reviewed run.
It downloads the run's source tree from the artifact service, deploys the
run's playable build to Cloudflare Pages through `wrangler`, publishes the
implementation to a public GitHub repository through `gh` and `git`, streams
its progress to the backend through the backend client of the contracts
repository, reports the terminal result and exits. It holds no state of its
own and serves nothing.

What the publish must do is stated by the superrepo's
`apps/docs/src/content/docs/components/backend/results.md` (its Publish
section). Its configuration is entirely environment variables, documented in
`crates/publisher/src/config.rs`; the backend, the publish job, its token, the
run and the artifact service are required, and the release reads
`TCAB_GITHUB_ORG`, `TCAB_PAGES_PROJECT`, `GH_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`
and `CLOUDFLARE_API_TOKEN` when it runs. Without them the binary names the
first missing variable and exits nonzero:

```sh
cargo run --bin tcab-publisher
```

The image carries the tools the release shells out to, `git`, `gh` and
`wrangler` at the version the `Dockerfile` pins, and runs as the unprivileged
`node` user.

The dispatcher's configuration that names this image, the publisher Job's
secrets and the deploy sets pinning it live in the superrepo, under
`deployments/k8s/`. A deployment pins this repository's image at the commit
the superrepo's submodule pointer names.

## Getting started

Open the superrepo in its devcontainer, which carries every toolchain the
repositories need, and work from this repository's root.
A build here inherits the superrepo's Cargo configuration, which redirects
every dependency on another repository to the sibling checkout, so nothing is
fetched from a mirror.

One command runs every gate:

```sh
make gate
```

Each gate is a file under `ci/gates/` whose stem is the gate's id.
`uv run --quiet --project ci gate list` names them all, and
`azure-pipelines.yml` runs each one as a step of its own on every push, inside
the CI images the superrepo builds.

## The image

The `Dockerfile` at the root is the repository's image, built from the root
with `.dockerignore` as its allowlist. On every gated push to `master` and
`staging` the pipeline builds it natively for amd64 and arm64
(`scripts/ci/image.sh`) and fuses the two into the multi-arch
`testcabinet.azurecr.io/microservice-publisher:<commit>` a deployment pins
(`scripts/ci/manifest.sh`).
The `build-context` gate holds every `COPY` to the allowlist and the Rust pin
to `rust-toolchain.toml`. The kit wrote the `Dockerfile` once, and it is the
repository's to grow.
