# Kiln

A self-hosted, multi-tenant platform for orchestrating AI coding agents on an
always-on machine. Plan a feature with the **Planner**; every approved task
runs **Builder → QA → Reviewer** in isolated containers and comes out as a
pull request with QA evidence and a review verdict. A layered **company
brain** learns from every run, with per-user privacy.

> "Kiln" is a provisional name — see [`DECISIONS.md`](DECISIONS.md).

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

## Production install (Mac mini)

1. Host prep: `sudo pmset -a sleep 0 autorestart 1`; create a dedicated non-admin user; install Docker/OrbStack and Tailscale.
2. Build the worker image: `docker build -f images/worker/Dockerfile -t kiln-worker:0.1.0 .`
3. `cargo build --release -p kiln-daemon -p kiln-tui` (add `--features kiln-memory/full` for LanceDB/fastembed/DataFusion; needs `protoc`).
4. Copy `config/kiln.example.toml` to `<data_dir>/kiln.toml`; keep `data_dir` short (Unix socket paths are limited to ~100 bytes), choose `[github] mode = "app"` with your GitHub App id + key.
5. `kilnd install`, load the service, then `tailscale serve` HTTPS in front of it.
6. First run: open the app (or TUI) → create the admin, allowed email domain, register projects (each with a `qa.yaml`; see the example in `crates/daemon/src/seed.rs`).

## Status

Built end-to-end and tested with mock agents (no model credits spent),
including the full pipeline in real Docker containers with a Postgres QA
service. Not yet verified: real Claude/Codex sessions, the Tauri shell builds (needs
WebKitGTK / Android SDK), native Android push & speech, GitHub App against real GitHub.
The `full` memory features (LanceDB, fastembed, DataFusion) are built and
tested (`PROTOC=... KILN_TEST_FASTEMBED=1 cargo test -p kiln-memory --features full`).

In `--dev`, the daemon falls back to the process runtime (no isolation) when
Docker or the worker image is missing.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
