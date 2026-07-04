//! Command-line interface.
//!
//! Two subcommands: `diff` (preview — read-only) and `sync` (do the job). They share `--from`,
//! `--to`, `--eager-checksum`, and `--report`; `sync` adds the write-side toggles. Source and
//! destination are named (`--from`/`--to`), never positional, so they can't be silently swapped.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "filesync",
    version,
    about = "Cheaply and reliably mirror one directory onto another.",
    long_about = "Mirror a SOURCE directory onto a DEST directory (e.g. backups to an external \
                  drive). Runs unattended (no prompts), is resumable, and never modifies the source."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Report what a sync would do (new / changed / moved / deleted). Changes nothing.
    Diff(DiffArgs),
    /// Make DEST mirror SOURCE: copy new/changed, rename moves, delete extras. Resumable.
    Sync(SyncArgs),
}

/// Options shared by both subcommands.
#[derive(Args, Debug)]
pub struct Common {
    /// Source directory — treated strictly read-only; never modified.
    #[arg(long, value_name = "DIR")]
    pub from: PathBuf,

    /// Destination directory.
    #[arg(long, value_name = "DIR")]
    pub to: PathBuf,

    /// Compare by file content (blake3) instead of the fast size+mtime check — slower, but never
    /// misses a same-size, same-mtime change.
    #[arg(long)]
    pub eager_checksum: bool,

    /// Report file path. Default: ./filesync-report-<source>-<YYYY-mm-DD_HHMM>.txt
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Worker threads for parallelizable work (content hashing). 1 = fully sequential (default);
    /// higher speeds up hashing on fast storage but puts more concurrent load on the device.
    #[arg(long, default_value_t = 1, value_name = "N")]
    pub jobs: usize,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    #[command(flatten)]
    pub common: Common,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[command(flatten)]
    pub common: Common,

    /// Skip re-reading each copied file to confirm it landed correctly.
    #[arg(long)]
    pub no_verify: bool,

    /// fsync every file individually (durable-as-you-go, but far slower on many small files).
    /// Default: one filesystem sync at the end.
    #[arg(long)]
    pub fsync_each: bool,

    /// Move files that would be deleted or overwritten here, instead of erasing them.
    #[arg(long, value_name = "DIR")]
    pub backup_dir: Option<PathBuf>,
}

impl Command {
    /// The options shared by both subcommands.
    pub fn common(&self) -> &Common {
        match self {
            Command::Diff(a) => &a.common,
            Command::Sync(a) => &a.common,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sync_with_all_flags() {
        let cli = Cli::try_parse_from([
            "filesync", "sync", "--from", "/a", "--to", "/b", "--eager-checksum", "--no-verify",
            "--fsync-each", "--backup-dir", "/trash",
        ])
        .unwrap();
        match cli.command {
            Command::Sync(a) => {
                assert_eq!(a.common.from, PathBuf::from("/a"));
                assert_eq!(a.common.to, PathBuf::from("/b"));
                assert!(a.common.eager_checksum && a.no_verify && a.fsync_each);
                assert_eq!(a.backup_dir, Some(PathBuf::from("/trash")));
            }
            _ => panic!("expected sync"),
        }
    }

    #[test]
    fn parses_diff_minimal() {
        let cli = Cli::try_parse_from(["filesync", "diff", "--from", "/a", "--to", "/b"]).unwrap();
        assert!(matches!(cli.command, Command::Diff(_)));
        assert!(!cli.command.common().eager_checksum);
    }

    #[test]
    fn from_and_to_are_required() {
        assert!(Cli::try_parse_from(["filesync", "sync", "--from", "/a"]).is_err());
        assert!(Cli::try_parse_from(["filesync", "diff"]).is_err());
    }

    #[test]
    fn jobs_defaults_to_one_and_parses() {
        let c = Cli::try_parse_from(["filesync", "sync", "--from", "/a", "--to", "/b"]).unwrap();
        assert_eq!(c.command.common().jobs, 1);
        let c2 =
            Cli::try_parse_from(["filesync", "diff", "--from", "/a", "--to", "/b", "--jobs", "4"])
                .unwrap();
        assert_eq!(c2.command.common().jobs, 4);
    }

    #[test]
    fn sync_only_flags_are_rejected_on_diff() {
        // The subcommand split means write-side flags don't exist on the read-only preview.
        assert!(Cli::try_parse_from(
            ["filesync", "diff", "--from", "/a", "--to", "/b", "--no-verify"]
        )
        .is_err());
    }
}
