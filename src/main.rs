use std::process::ExitCode;

use clap::Parser;
use filesync::Cli;

fn main() -> ExitCode {
    filesync::run(Cli::parse())
}
