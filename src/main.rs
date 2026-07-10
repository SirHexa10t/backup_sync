use std::process::ExitCode;

use clap::Parser;
use filesync::Cli;

fn main() -> ExitCode {
    ExitCode::from(filesync::run(Cli::parse()))
}
