# WhatsApp sidecar image for IronClaw.
#
# Built from pinned local checkouts passed as BuildKit named contexts:
#
#   docker build \
#     --build-context bridge=/path/to/whatsapp-rust-bridge \
#     --build-context baileys=/path/to/Baileys \
#     -t ironclaw-whatsapp-sidecar .
#
# `build.sh` next to this file computes the checkouts' commits on the host,
# checks them against the pins below and runs exactly that command.
#
# No prebuilt whatsapp-rust-bridge WASM and no prebuilt Baileys `lib/` is ever
# downloaded: both are compiled here from the checkouts. Every build stage
# extracts its sources with `git archive <pinned commit>`, so uncommitted or
# untracked files in a checkout never reach the build, and the stage fails if
# the checkout's HEAD is not the pinned commit.
#
# The file deliberately has no `# syntax=` line: the BuildKit frontend built
# into Docker is used, so the build pulls no frontend image.

# Base images, pinned by digest. BASE_REGISTRY may point at a mirror of the
# Docker official images (for example public.ecr.aws/docker/library); the
# digests make the mirror unable to substitute different content.
ARG BASE_REGISTRY=docker.io/library
ARG RUST_IMAGE=rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546
ARG NODE_BUILD_IMAGE=node:24.15-trixie@sha256:8202a46483627d14c75c8078d8c1b1d8ec14b792390c7001adb4f698724c4ca9
ARG NODE_RUNTIME_IMAGE=node:24.15-trixie-slim@sha256:291be77873bc04731968cacf82f0fcef17cee8cf200c6b6951e2bcab41560eb7

# Pinned upstream commits. build.sh passes the same values; the defaults make
# a bare `docker build` check them too.
ARG BRIDGE_COMMIT=4b185ecf26b2f0077606ba8b181cc6ae0d562ef9
ARG BAILEYS_COMMIT=0af2386292907f7d9742d8d41f830d8c48208fa1
ARG WHATSAPP_RUST_COMMIT=88a0fe074fdc09d73be95cdd23743fba22ef172e

# ---------------------------------------------------------------------------
# Node toolchain, reused by the Rust stage for the bridge's JS build steps.
# ---------------------------------------------------------------------------
FROM ${BASE_REGISTRY}/${NODE_BUILD_IMAGE} AS node-build

# ---------------------------------------------------------------------------
# Stage 1: whatsapp-rust-bridge (Rust -> WASM -> JS package tarball)
# ---------------------------------------------------------------------------
FROM ${BASE_REGISTRY}/${RUST_IMAGE} AS bridge-tools

# Build tools, versions pinned. wasm-pack and wasm-bindgen-cli are compiled
# from crates.io sources with their own lockfiles. The wasm-bindgen-cli
# version must equal the `wasm-bindgen` version in the bridge's Cargo.lock
# (checked below); wasm-pack then uses it from PATH instead of downloading.
ARG RUST_NIGHTLY=nightly-2026-01-30
ARG WASM_PACK_VERSION=0.14.0
ARG WASM_BINDGEN_VERSION=0.2.121
ARG BINARYEN_DEB_VERSION=120-4

# binaryen (wasm-opt) comes from Debian trixie. The bridge's CI uses binaryen
# 129; every flag its build script passes exists in 120.
RUN apt-get update \
 && apt-get install -y --no-install-recommends "binaryen=${BINARYEN_DEB_VERSION}" \
 && rm -rf /var/lib/apt/lists/*

# Node (for the bridge's build script and npm) from the pinned node image.
COPY --from=node-build /usr/local/bin/node /usr/local/bin/node
COPY --from=node-build /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && node --version && npm --version

# The bridge's rust-toolchain.toml selects this nightly; install it up front
# (with the components that file lists) so nothing is fetched implicitly.
RUN rustup toolchain install "${RUST_NIGHTLY}" --profile minimal \
      --target wasm32-unknown-unknown --component rustfmt,clippy

RUN cargo install --locked "wasm-pack@${WASM_PACK_VERSION}" \
 && cargo install --locked "wasm-bindgen-cli@${WASM_BINDGEN_VERSION}" \
 && rm -rf /usr/local/cargo/registry /usr/local/cargo/git \
 && wasm-pack --version && wasm-bindgen --version && wasm-opt --version

# bun is only a build tool here (it bundles the bridge's TS entry point and
# inlines the WASM through a bun macro). docker/tools/bun pins it and its
# platform packages in a lockfile; npm ci installs exactly those and checks
# each one's integrity hash. bun's install script (which links the platform
# binary) runs, from that pinned code.
COPY docker/tools/bun/ /opt/build-tools/bun/
RUN cd /opt/build-tools/bun \
 && npm ci --no-audit --no-fund \
 && ln -s /opt/build-tools/bun/node_modules/.bin/bun /usr/local/bin/bun \
 && bun --version

FROM bridge-tools AS bridge-build
ARG BRIDGE_COMMIT
ARG WHATSAPP_RUST_COMMIT
ARG WASM_BINDGEN_VERSION

# Verify the checkout and extract exactly the pinned commit.
RUN --mount=type=bind,from=bridge,target=/ctx \
    set -eu; \
    head="$(git -c safe.directory='*' -C /ctx rev-parse HEAD)"; \
    if [ "$head" != "$BRIDGE_COMMIT" ]; then \
      echo "whatsapp-rust-bridge checkout is at $head, expected $BRIDGE_COMMIT" >&2; exit 1; \
    fi; \
    mkdir -p /build/bridge; \
    git -c safe.directory='*' -C /ctx archive --format=tar "$BRIDGE_COMMIT" | tar -x -C /build/bridge; \
    echo "$BRIDGE_COMMIT" > /build/bridge/.source-commit
WORKDIR /build/bridge

# Every whatsapp-rust crate in the lockfile must come from the pinned commit,
# and the lockfile's wasm-bindgen must match the installed CLI.
RUN set -eu; \
    sources="$(grep -o 'git+https://github.com/jlucaso1/whatsapp-rust.git[^"]*' Cargo.lock | sort -u)"; \
    expected="git+https://github.com/jlucaso1/whatsapp-rust.git?branch=main#${WHATSAPP_RUST_COMMIT}"; \
    if [ "$sources" != "$expected" ]; then \
      echo "unexpected whatsapp-rust sources in Cargo.lock:" >&2; echo "$sources" >&2; exit 1; \
    fi; \
    locked="$(awk '/^name = "wasm-bindgen"$/ {getline; gsub(/version = |"/, ""); print; exit}' Cargo.lock)"; \
    if [ "$locked" != "$WASM_BINDGEN_VERSION" ]; then \
      echo "Cargo.lock pins wasm-bindgen $locked but wasm-bindgen-cli is $WASM_BINDGEN_VERSION" >&2; exit 1; \
    fi

# Fetch every crate (including whatsapp-rust at the locked commit) once, then
# build offline.
RUN cargo fetch --locked
ENV CARGO_NET_OFFLINE=true

# build:wasm - SIMD and non-SIMD builds, each through wasm-pack + wasm-opt.
RUN node scripts/build-wasm.mjs

# TypeScript tools for the declaration build, at the versions bun.lock pins,
# with their dependencies locked in docker/tools/bridge-ts. Installed outside
# the source tree so the bridge's own devDependencies (which include a
# prebuilt Baileys from npm) are never installed.
COPY docker/tools/bridge-ts/ /build/bridge-tools/
RUN cd /build/bridge-tools \
 && npm ci --no-audit --no-fund --ignore-scripts \
 && ln -s /build/bridge-tools/node_modules /build/bridge/node_modules

# build:ts and postbuild, as the bridge's package.json defines them.
RUN rm -rf dist \
 && bun build ts/index.ts --outfile dist/index.js \
 && node_modules/.bin/tsc -p tsconfig.json --outDir dist \
 && rm node_modules

# Pack exactly the files the package publishes.
RUN mkdir -p /out \
 && npm pack --ignore-scripts --pack-destination /out \
 && ls -l /out \
 && tar -tzf /out/whatsapp-rust-bridge-*.tgz

# ---------------------------------------------------------------------------
# Stage 2: Baileys, built against the locally built bridge
# ---------------------------------------------------------------------------
FROM ${BASE_REGISTRY}/${NODE_BUILD_IMAGE} AS baileys-build
ARG BAILEYS_COMMIT
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0 \
    YARN_ENABLE_TELEMETRY=0 \
    YARN_ENABLE_GLOBAL_CACHE=1

RUN --mount=type=bind,from=baileys,target=/ctx \
    set -eu; \
    head="$(git -c safe.directory='*' -C /ctx rev-parse HEAD)"; \
    if [ "$head" != "$BAILEYS_COMMIT" ]; then \
      echo "Baileys checkout is at $head, expected $BAILEYS_COMMIT" >&2; exit 1; \
    fi; \
    mkdir -p /build/baileys; \
    git -c safe.directory='*' -C /ctx archive --format=tar "$BAILEYS_COMMIT" | tar -x -C /build/baileys; \
    echo "$BAILEYS_COMMIT" > /build/baileys/.source-commit

COPY --from=bridge-build /out/ /build/bridge-pkg/
COPY docker/ /build/docker/
WORKDIR /build/baileys

# Yarn 4.9.2, as Baileys' packageManager field names it.
RUN corepack enable && corepack prepare yarn@4.9.2 --activate && yarn --version

# Point whatsapp-rust-bridge at the local tarball (npm has 0.5.4; the local
# checkout is 0.5.5) and check the bridge API Baileys imports is present.
RUN node /build/docker/use-local-bridge.mjs /build/baileys /build/bridge-pkg \
 && cp yarn.lock /build/yarn.lock.orig

# Full install (dev dependencies are needed for tsc, tsc-esm-fix and jest).
# Baileys' preinstall script only checks for Node >= 20.
RUN yarn install --mode=skip-build

# The only lockfile change allowed is the whatsapp-rust-bridge entry.
RUN node /build/docker/check-lockfile.mjs /build/yarn.lock.orig yarn.lock

RUN yarn build

# Baileys' own unit tests, now running on the 0.5.5 bridge. They cover the
# bridge functions Baileys uses (hkdf, md5, app-state key expansion, LT hash).
RUN yarn test

# Production-only dependency tree for the runtime image.
RUN set -eu; \
    mkdir -p /build/baileys-prod; \
    cp -r package.json yarn.lock .yarnrc.yml engine-requirements.js lib WAProto LICENSE /build/baileys-prod/; \
    cd /build/baileys-prod; \
    YARN_ENABLE_SCRIPTS=false yarn workspaces focus --production; \
    rm -rf .yarn; \
    if find node_modules -name '*.node' | grep -q .; then \
      echo "native addon found in production dependencies:" >&2; find node_modules -name '*.node' >&2; exit 1; \
    fi

# ---------------------------------------------------------------------------
# Stage 3: the sidecar, compiled and tested
# ---------------------------------------------------------------------------
FROM baileys-build AS app
WORKDIR /build/sidecar
COPY package.json tsconfig.json ./
COPY src/ src/
COPY test/ test/
# Compile against the built Baileys, with the TypeScript and @types/node that
# Baileys' lockfile pins. The sidecar has no registry dependencies of its own.
RUN mkdir -p node_modules \
 && ln -s /build/baileys node_modules/baileys \
 && ln -s /build/baileys/node_modules/@types node_modules/@types \
 && /build/baileys/node_modules/.bin/tsc -p tsconfig.json
# The image build fails if any test fails: once against the full Baileys tree
# and once against the production-only tree the runtime image ships. node
# --test passes when its pattern matches no files, so that is checked first.
RUN test -n "$(find dist/test -name '*.test.js')" || { echo "no compiled test files" >&2; exit 1; } \
 && node --test --test-reporter=spec 'dist/test/**/*.test.js'
RUN ln -sfn /build/baileys-prod node_modules/baileys \
 && node --test --test-reporter=dot 'dist/test/**/*.test.js'

# ---------------------------------------------------------------------------
# Stage 4: runtime
# ---------------------------------------------------------------------------
FROM ${BASE_REGISTRY}/${NODE_RUNTIME_IMAGE} AS runtime
ARG BRIDGE_COMMIT
ARG BAILEYS_COMMIT
ARG WHATSAPP_RUST_COMMIT

# No package managers in the runtime image.
RUN rm -rf /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack \
      /usr/local/bin/npm /usr/local/bin/npx /usr/local/bin/corepack \
      /usr/local/bin/yarn /usr/local/bin/yarnpkg /opt/yarn-* \
 && groupadd --system --gid 10001 sidecar \
 && useradd --system --uid 10001 --gid sidecar --home-dir /nonexistent --shell /usr/sbin/nologin sidecar \
 && mkdir -p /data \
 && chown sidecar:sidecar /data \
 && chmod 0700 /data

WORKDIR /app
COPY --from=app /build/sidecar/package.json /app/package.json
COPY --from=app /build/sidecar/dist/src/ /app/dist/src/
COPY --from=baileys-build /build/baileys-prod/ /app/node_modules/baileys/

LABEL org.opencontainers.image.title="ironclaw-whatsapp-sidecar" \
      org.opencontainers.image.description="WhatsApp linked-device sidecar for IronClaw (Baileys + whatsapp-rust-bridge)" \
      io.ironclaw.whatsapp-sidecar.baileys-commit="${BAILEYS_COMMIT}" \
      io.ironclaw.whatsapp-sidecar.bridge-commit="${BRIDGE_COMMIT}" \
      io.ironclaw.whatsapp-sidecar.whatsapp-rust-commit="${WHATSAPP_RUST_COMMIT}"

ENV NODE_ENV=production \
    SIDECAR_DATA_DIR=/data \
    SIDECAR_LISTEN=0.0.0.0:8080
USER sidecar:sidecar
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD ["node", "/app/dist/src/healthcheck.js"]
ENTRYPOINT ["node", "/app/dist/src/main.js"]
