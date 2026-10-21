# Kiln

A self-hosted, multi-tenant platform for orchestrating AI coding agents on an
always-on machine. Plan a feature with the **Planner**; every approved task
runs **Builder → QA → Reviewer** in isolated containers and comes out as a
pull request with QA evidence and a review verdict. A layered **company
brain** learns from every run, with per-user privacy.

> "Kiln" is a provisional name — see [`DECISIONS.md`](DECISIONS.md).

- **Install:** [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md)
- **Decisions:** [`DECISIONS.md`](DECISIONS.md) (assumptions and defaults)
- **Internals:** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (crates & contracts) · [`docs/SCHEDULER.md`](docs/SCHEDULER.md) (pipeline, recovery, daemon)

## Components

| Path | What |
|---|---|
| `crates/daemon` | `kilnd` — the one server process: HTTP/WS API, scheduler, memory, web app hosting |
| `crates/runner` | `kiln-runner` — runs inside each worker container and drives the harness |
| `crates/tui` | `kiln` — keyboard-first terminal client |
| `app/` | Leptos UI for desktop + Android (Tauri 2 shell in `app/src-tauri`); also served by `kilnd` at `/app` |
| `images/worker` | Base worker image (runner, Node, Claude Agent SDK, Codex, Playwright) |
| `crates/*` | types, store, auth, vault, events, scheduler, sandbox, harness, gitops, memory, planner, notifier, api-client |

## Quick start (development)

Requirements: Rust ≥ 1.95, git, Docker (or OrbStack/Podman) for real
containers. `trunk` + `wasm32-unknown-unknown` for the web UI.

```sh
# 1. Build the UI (optional; the daemon serves app/dist in --dev)
(cd app && trunk build --release)

# 2. Run the daemon with demo data: local repos, mock agents, local PRs
cargo build -p kiln-daemon -p kiln-runner
KILN_DATA_DIR=~/.kiln-dev KILN_NO_KEYCHAIN=1 target/debug/kilnd serve --dev --demo-seed
#   → binds the Tailscale IP if present, else 127.0.0.1:7420
#   → login: admin@company.dev / kiln-demo-password

# 3. Clients
open http://127.0.0.1:7420/app/            # desktop UI in a browser
cargo run -p kiln-tui -- --server http://127.0.0.1:7420
cargo run -p kiln-tui -- --demo            # TUI without a daemon
# web UI without a daemon: open /app/?demo=1 (add &surface=android for the phone layout)
```

Useful commands: `kilnd doctor` (checks runtime, keychain, Tailscale,
disk, image, GitHub auth, socket-path length), `kilnd install` (launchd /
systemd unit), `kilnd admin reset-password <email>`.

## Install

**[`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md)** walks through a real
install (release binaries or source, Docker, GitHub, model keys, the first
project, running as a service, the Mac mini checklist and the apps).

## Status

Verified on real infrastructure:

- **Real agents end to end.** Codex (`gpt-6.1-sol`) and the Claude Agent SDK
  (`claude-haiku-4-5`), both through a LiteLLM gateway, planned, built,
  tested and reviewed tasks in Docker and opened real GitHub pull requests
  (token mode).
- **Containers.** Full pipeline in Docker, including Postgres QA services
  with health checks and per-project extras images.
- **Desktop app.** Tauri build packaged as AppImage, `.deb` and `.rpm`.
- **Memory backends.** The `full` features (LanceDB, fastembed, DataFusion)
  pass their tests (`PROTOC=... KILN_TEST_FASTEMBED=1 cargo test -p kiln-memory --features full`).

Not yet verified: the GitHub App mode against real GitHub, and native Android
push and speech on a device.

In `--dev`, the daemon falls back to the process runtime (no isolation) when
Docker or the worker image is missing.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
