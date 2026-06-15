//! Command-line arguments (clap derive).
//!
//! v1/v2 are store-driven: the source, destinations, and mappings live in
//! `./.bukagu/`, set up interactively — so the bare invocation only carries
//! run-time flags. v3 adds subcommands (`auth`, …); with **no** subcommand,
//! bukagu behaves exactly as before (interactive home, or `--yes` fast sync).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// bukagu — mirror a read-only source into destinations, map files, and back the
/// source up encrypted.
#[derive(Debug, Parser)]
#[command(name = "bukagu", version, about)]
pub struct Cli {
    /// Subcommand to run. With none, bukagu opens the interactive home (or, with
    /// `--yes` on a returning run, syncs immediately).
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Preview the sync without writing anything to the destinations.
    #[arg(long)]
    pub dry_run: bool,

    /// Apply without the interactive Review confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Delete files in destinations that no longer exist in the source.
    #[arg(long)]
    pub delete: bool,

    /// Ignore the saved store and re-run first-time onboarding.
    #[arg(long)]
    pub reset: bool,
}

/// bukagu subcommands. The bare `bukagu` (no subcommand) keeps the v1/v2 home +
/// sync behavior.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage the API credentials used to fetch the backup encryption key.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },

    /// Back up the source folder, encrypted, into ~/bukagu-backups/<project>.
    Backup {
        /// Preview: guard, fetch the key, and report — but write no archive.
        #[arg(long)]
        dry_run: bool,

        /// Encrypt to this age recipient (age1…) directly instead of fetching it
        /// from the API — for offline development/testing. Hidden from --help.
        #[arg(long, hide = true)]
        recipient: Option<String>,
    },

    /// Restore a backup by decrypting it with your private age identity.
    Restore {
        /// Your age identity (AGE-SECRET-KEY-1…), or `@<path>` to read it from a
        /// key file, or `-` to read it from stdin. Prompted for (hidden) if
        /// omitted. Obtain it from your website. Prefer `@<path>` over passing the
        /// secret on the command line.
        #[arg(long)]
        identity: Option<String>,

        /// Archive to restore. Defaults to the newest in ~/bukagu-backups/<project>.
        #[arg(long)]
        archive: Option<PathBuf>,

        /// Directory to restore into. Defaults to ./bukagu-restore-<timestamp>.
        #[arg(long)]
        into: Option<PathBuf>,

        /// Allow restoring into an existing, non-empty directory (never the source).
        #[arg(long)]
        force: bool,
    },
}

/// `bukagu auth …` actions.
#[derive(Debug, Subcommand)]
pub enum AuthAction {
    /// Save the API token + URL (copied from your website) to ~/.config/bukagu.
    ///
    /// The token is read from a hidden prompt (or stdin), never from a flag, so
    /// it doesn't leak into shell history or the process list.
    Login {
        /// API base URL (https://…). Prompted for if omitted.
        #[arg(long)]
        url: Option<String>,

        /// Read the token from stdin instead of the hidden prompt (e.g. piping
        /// it in CI). Implied when stdin is not a terminal.
        #[arg(long)]
        token_stdin: bool,
    },

    /// Show whether API credentials are configured (without revealing the token).
    Status,
}
