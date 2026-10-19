# syntax=docker/dockerfile:1.7
#
# Kiln base worker image (DECISIONS.md #2): kiln-runner + Node + Claude Agent SDK
# + Codex CLI + Playwright/Chromium. Build from the repository root:
#
#   docker build -f images/worker/Dockerfile -t kiln/worker:dev .
#
# Per-project extras are layered on top with extras.Dockerfile.tmpl.

ARG RUST_IMAGE=rust:1.95-bookworm
ARG NODE_IMAGE=node:22.23.3-bookworm-slim

# ---------------------------------------------------------------- runner build
FROM ${RUST_IMAGE} AS runner-build
WORKDIR /src
# Only the manifests + sources cargo needs to resolve the workspace
# (see Dockerfile.dockerignore for what is excluded from the context).
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target-docker \
    CARGO_TARGET_DIR=/src/target-docker cargo build --release -p kiln-runner \
 && install -m 0755 /src/target-docker/release/kiln-runner /usr/local/bin/kiln-runner

# ---------------------------------------------------------------- worker image
FROM ${NODE_IMAGE}

ARG CLAUDE_AGENT_SDK_VERSION=0.3.303
ARG CODEX_VERSION=0.163.0
ARG PLAYWRIGHT_VERSION=1.64.1

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    HOME=/home/kiln \
    PLAYWRIGHT_BROWSERS_PATH=/ms-playwright \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false \
    DISABLE_AUTOUPDATER=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    KILN_RUN_DIR=/run/kiln \
    KILN_WORKSPACE=/workspace

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ca-certificates curl jq build-essential python3 python3-venv python3-pip ripgrep \
      openssh-client unzip xz-utils procps less \
 && rm -rf /var/lib/apt/lists/*

# Codex CLI (global) and Playwright with Chromium + its system deps.
RUN npm install -g "@openai/codex@${CODEX_VERSION}" "playwright@${PLAYWRIGHT_VERSION}" \
 && playwright install --with-deps chromium \
 && chmod -R a+rX "${PLAYWRIGHT_BROWSERS_PATH}" \
 && rm -rf /var/lib/apt/lists/* /root/.npm

# Claude Agent SDK lives next to the entry script: ESM imports resolve from the
# script's directory, not from the global prefix.
WORKDIR /opt/kiln
COPY images/worker/claude-entry.mjs /opt/kiln/claude-entry.mjs
RUN printf '{"name":"kiln-claude-entry","private":true,"type":"module"}\n' > package.json \
 && npm install --omit=dev --no-audit "@anthropic-ai/claude-agent-sdk@${CLAUDE_AGENT_SDK_VERSION}" \
 && rm -rf /root/.npm \
 && node --check /opt/kiln/claude-entry.mjs

COPY --from=runner-build /usr/local/bin/kiln-runner /usr/local/bin/kiln-runner

# Non-root user `kiln` (uid 1000). The node image ships a `node` user with
# uid 1000; replace it. HOME and the harness transcript dirs (bind-mounted to
# the run folder) must exist and stay writable even when the daemon runs the
# container as the host uid (`ContainerSpec.user`).
RUN userdel -r node 2>/dev/null || true \
 && useradd --create-home --home-dir /home/kiln --uid 1000 --shell /bin/bash kiln \
 && mkdir -p /home/kiln/.claude/projects /home/kiln/.codex/sessions /run/kiln /workspace \
 && chown -R kiln:kiln /home/kiln /run/kiln /workspace \
 && chmod 1777 /home/kiln /home/kiln/.claude /home/kiln/.codex \
 && git config --system --add safe.directory '*' \
 && git config --system init.defaultBranch main

USER kiln
WORKDIR /workspace
ENTRYPOINT ["kiln-runner", "run"]
