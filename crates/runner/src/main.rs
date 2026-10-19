use clap::Parser;

fn main() {
    let code = kiln_runner::main_with(kiln_runner::Cli::parse());
    std::process::exit(code);
}
