//! Test/dev copy of the `kiln-runner` binary, so the scheduler's integration
//! tests can drive real runs under `ProcessRuntime` without a separate build.

use clap::Parser;

fn main() {
    let code = kiln_runner::main_with(kiln_runner::Cli::parse());
    std::process::exit(code);
}
