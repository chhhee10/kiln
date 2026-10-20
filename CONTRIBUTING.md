# Contributing to Kiln

Thanks for helping out! Kiln is a Rust workspace plus a Leptos web UI.

## Setup

- Rust 1.95+ (`rustup`), `git`, and Docker (or Podman/OrbStack) for real containers.
- For the web UI: `rustup target add wasm32-unknown-unknown` and `cargo install trunk`.
- The memory crate's `full` feature (LanceDB) needs `protoc`.

## Before you open a PR

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Optional, slower suites:

- `KILN_TEST_DOCKER=1 cargo test -p kiln-sandbox` runs against a real Docker daemon.
- `KILN_TEST_FASTEMBED=1 cargo test -p kiln-memory --features full` downloads ONNX models.

## Where things live

`docs/ARCHITECTURE.md` describes every crate and its public API;
`docs/SCHEDULER.md` covers the task pipeline and recovery; `DECISIONS.md`
records design choices. If you change a public API listed in
`ARCHITECTURE.md`, update it in the same PR.

## Guidelines

- Keep PRs focused; add tests for behaviour changes.
- Never log secrets: credentials go through `kiln-vault` and its redactor.
- Every task state change is a compare-and-swap recorded before its side effect.
- Contributions are dual-licensed under MIT or Apache-2.0, like the project.
