//! A copy of the `kiln-runner` binary for the daemon's integration tests.
//!
//! The process runtime needs a runner executable on the host; `cargo test`
//! builds examples of the package under test, so the tests find this one at
//! `target/<profile>/examples/kiln-runner-dev` without a nested cargo build.
//! (Its name starts with `kiln-runner`, so the mock harness re-invokes it.)

use clap::Parser;

fn main() {
    let code = kiln_runner::main_with(kiln_runner::Cli::parse());
    std::process::exit(code);
}
