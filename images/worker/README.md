# Kiln worker image

One base image runs every worker role (Planner, Builder, QA, Reviewer,
Distiller); per-project extras are layered on top (DECISIONS.md #2).

| Path in image                  | What                                                             |
|--------------------------------|------------------------------------------------------------------|
| `/usr/local/bin/kiln-runner`   | the runner (`ENTRYPOINT kiln-runner run`); also `mcp`, `mock-harness` |
| `/opt/kiln/claude-entry.mjs`   | Claude Agent SDK entry (harness `claude_sdk`), SDK in `/opt/kiln/node_modules` |
| `codex` (global npm)           | OpenAI Codex CLI (harness `codex`)                               |
| `playwright` + Chromium        | browsers in `/ms-playwright` (`PLAYWRIGHT_BROWSERS_PATH`)        |
| user `kiln` (uid 1000)         | `HOME=/home/kiln`; `~/.claude/projects`, `~/.codex/sessions` are bind-mounted to the run folder |

Pinned versions are build args: `CLAUDE_AGENT_SDK_VERSION`, `CODEX_VERSION`,
`PLAYWRIGHT_VERSION`, `RUST_IMAGE`, `NODE_IMAGE`.

## Build

From the repository root (the runner is compiled in a builder stage):

```sh
docker build -f images/worker/Dockerfile -t kiln/worker:dev .
```

`Dockerfile.dockerignore` keeps `target/`, `node_modules/` and design assets
out of the context.

## Runtime contract

The daemon (via `kiln-sandbox`) starts one container per run with:

- `/workspace` — task worktree (RW for the Builder, RO otherwise), plus the
  repo's common git dir at the same absolute path;
- `/run/kiln` — the run folder (socket `kiln.sock`, `spool.jsonl`, `out/`, `transcripts/`);
- env `KILN_RUN_ID`, `KILN_RUN_TOKEN`, `KILN_SOCKET=/run/kiln/kiln.sock` and
  the owner's credentials (`ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN`,
  `OPENAI_API_KEY`, `LITELLM_BASE_URL` + `LITELLM_API_KEY`, …);
- all capabilities dropped, `no-new-privileges`, pids/memory/cpu limits, an
  init process, and no auto-remove (the daemon removes it after ingest).

Workers write structured results to `/run/kiln/out/` (`qa_report.json`,
`verdict.json`, `plan.json`, `reply.md`, `summary.md`) and evidence to
`/run/kiln/out/artifacts/`. The runner always writes `out/outcome.json`.

## Smoke test without a model

```sh
docker run --rm --entrypoint kiln-runner kiln/worker:dev --help
docker run --rm --entrypoint node kiln/worker:dev --check /opt/kiln/claude-entry.mjs
```

## Per-project extras

`extras.Dockerfile.tmpl` shows how `qa.yaml: extras` (`apt`, `npm`, `cargo`,
`pip`) is layered on the base image; the daemon renders and caches it.
