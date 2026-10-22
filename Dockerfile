# syntax=docker/dockerfile:1
# The image of microservice-publisher. Its pipeline builds it on every gated push to master
# and staging, natively once per architecture (scripts/ci/image.sh), and fuses
# the two into the multi-arch `testcabinet.azurecr.io/microservice-publisher:<commit>` a
# deployment pins (scripts/ci/manifest.sh).
#
# The build context is the repository root, held to what .dockerignore admits,
# and the `build-context` gate holds every COPY below to that allowlist and the
# Rust pin to rust-toolchain.toml's channel. The kit writes this file once, as
# a scaffold, and it then belongs to the repository: an update never merges
# into it.
#
# A manifest names a dependency on another repository by its public source,
# which a build cannot reach while the mirrors are private. The pipeline hands
# the build the git configuration scripts/ci/git-sources.sh wrote, which
# rewrites each public source to Azure and carries the job's token, as the
# secret `gitconfig`; it is mounted for the one RUN that fetches and never
# enters a layer. A build without it fetches from the public sources as named.

# The wrangler the Pages deploy runs, pinned. Bump deliberately (Cloudflare
# ships frequent releases); pinning keeps the publish path reproducible across
# image builds. Declared before the first FROM so the runtime stage's own
# `ARG WRANGLER_VERSION` inherits this default.
ARG WRANGLER_VERSION=4.40.3

FROM docker.io/library/rust:1.97.1-bookworm AS build
WORKDIR /src
COPY . .
# The commit the image is built from, which the build context, carrying no
# `.git`, cannot tell; the pipeline passes it.
ARG TCAB_BUILD_COMMIT
# The cargo registry, the git checkouts and target/ are cache mounts, so a
# source change compiles only what changed. BuildKit replays the mtimes a file
# had in the context when the COPY is served from cache, so the sources are
# touched first and cargo never takes an artifact of another build as fresh.
RUN --mount=type=secret,id=gitconfig,target=/root/.gitconfig,required=false \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    find /src -path /src/target -prune -o -type f -exec touch {} + \
    && TCAB_BUILD_COMMIT="${TCAB_BUILD_COMMIT}" cargo build --release --locked -p tcab-publisher \
    && mkdir -p /out \
    && cp target/release/tcab-publisher /out/

# The dispatcher creates one publisher Job per publish (a lane beside the run
# path); each publisher pod runs THIS image, resolves its publish job from the
# environment the dispatcher set, downloads the reviewed run's source tree from
# the artifact service, performs the GitHub-repo and Cloudflare Pages release
# (the publisher's own BackendPublisher, crates/publisher/src/release.rs) while
# streaming progress to the backend, reports the terminal result, and exits.
# Its configuration comes from the environment, documented in
# crates/publisher/src/config.rs.
#
# Unlike the driver image (which renders builds to screenshot them and therefore
# carries Playwright and a Playwright-managed Chromium), the publisher renders
# NOTHING: it only releases. So it carries no browser, and instead the three
# tools the release path shells out to:
#   - `git`, to commit the model's working tree into each run's seeded repository
#     and push it to the run's public repo (release.rs `commit_implementation`
#     sets a per-repo `user.name`/`user.email`, so no global git identity is
#     needed here; the push authenticates through `gh`'s credential helper, see
#     below, so no git credential helper is configured).
#   - `gh`, the GitHub CLI, for the idempotent `gh repo view` gate, `gh repo
#     create --public` (the empty repo), and, via `gh auth git-credential`, the
#     credential helper the implementation push authenticates through
#     (release.rs `release_code`/`push_implementation`; create and push are kept
#     separate so the push can retry through GitHub's post-create permission
#     lag). Installed from GitHub's official apt repository so it is a current,
#     supported build rather than Debian's older packaged one.
#   - `wrangler`, Cloudflare's CLI, for `wrangler pages deploy <dir>
#     --project-name <p> --branch=<run>` (release.rs `release_playable_build`).
#     It is an npm package and the base is Node, so it is installed globally and
#     PINNED above; the release invokes the bare `wrangler` on PATH.
# `gh`/`wrangler` authenticate from the Job's env (GH_TOKEN /
# CLOUDFLARE_API_TOKEN, wired by the deployment overlay), and the git push
# borrows `gh`'s token via its credential helper; the binary itself never reads
# the GitHub token.
FROM docker.io/library/node:24-bookworm-slim

ARG WRANGLER_VERSION

# ca-certificates covers the publisher's outbound HTTPS (the backend and
# artifact service over TLS, GitHub and Cloudflare APIs, telemetry export); git
# commits each run's working tree before push; curl, gnupg and the GitHub apt
# key pull `gh` from GitHub's official repository. Everything is removed from
# the layer that no longer needs it.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
       ca-certificates \
       git \
       curl \
       gnupg \
  && mkdir -p -m 755 /etc/apt/keyrings \
  && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
       | tee /etc/apt/keyrings/githubcli-archive-keyring.gpg > /dev/null \
  && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
  && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
       > /etc/apt/sources.list.d/github-cli.list \
  && apt-get update \
  && apt-get install -y --no-install-recommends gh \
  && npm install -g --no-audit --no-fund "wrangler@${WRANGLER_VERSION}" \
  && apt-get purge -y --auto-remove curl gnupg \
  && rm -rf /var/lib/apt/lists/* /root/.npm

COPY --from=build /out/tcab-publisher /usr/local/bin/tcab-publisher

# Run as an unprivileged user: the Kubernetes runtime needs only API access (its
# ServiceAccount token and the per-publish-job token), never host privileges.
# The Node base already ships a non-root `node` user (uid 1000), reused rather
# than minting another at the same uid. Its home is writable for the downloaded
# source-tree scratch (TCAB_WORK_DIR) and the CLIs' caches and configuration.
USER node
WORKDIR /home/node

ENTRYPOINT ["tcab-publisher"]
