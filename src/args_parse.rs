use std::path::PathBuf;
use clap::{ArgAction, ArgGroup, Parser};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Sync directories by tracking and comparing directory trees",
    after_help = r#"EXAMPLES:
  filesync -t "$HOME/Downloads"
  filesync -t "$HOME/Downloads" -p firefox_pictures -p chrome
  filesync -d "$HOME/Downloads" "$HOME/Pictures"
  filesync -s "$HOME/Downloads" "$HOME/Pictures" --dry-run
"#
)]
#[command(
    group(
        ArgGroup::new("command")
            .required(true)
            .multiple(false) // exactly ONE of these must be present
            .args(["track", "diff", "sync"])
    )
)]
pub struct ProgramArgs {
    /// Write a tracking file to PATH (requires a DIR positional argument)
    /// return tracking-file path
    #[arg(short = 't', long = "track", value_name = "DIR")]
    pub track: Option<PathBuf>,

    /// Compare master vs slave directories
    #[arg(short = 'd', long = "diff", value_names = ["DIR_MASTER", "DIR_SLAVE"], num_args = 2)]
    pub diff: Option<Vec<PathBuf>>,

    /// Sync slave directory to match master directory
    #[arg(short = 's', long = "sync", value_names = ["DIR_MASTER", "DIR_SLAVE"], num_args = 2)]
    pub sync: Option<Vec<PathBuf>>,


    //optionals:

    /// Only include paths that start with PREFIX (repeatable). Allowed in any mode.
    #[arg(short, long, value_name = "PREFIX", action = ArgAction::Append)]
    pub prefix: Option<Vec<String>>,

    /// Print actions only (valid with --sync)
    #[arg(long, requires = "sync")]
    pub dry_run: bool,

}

