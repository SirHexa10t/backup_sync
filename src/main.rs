use std::process::ExitCode;

use clap::Parser;
use filesync::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Interactive runs launched without root ask for it RIGHT HERE — a sudo prompt at the very
    // start, never mid-run; `--unelevated` is the explicit opt-out. May not return (it re-runs
    // this binary under sudo and exits with that run's code). Binary-only by design: library
    // embedders calling `filesync::run` are never re-executed.
    filesync::runtime::elevation::sudo_prompt_at_start(cli.command.common().unelevated);
    ExitCode::from(filesync::run(cli))
}
