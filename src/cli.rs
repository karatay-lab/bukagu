//! Command-line arguments (clap derive).
//!
//! v1 is store-driven: the source and destinations live in `./.bukagu/`, set up
//! during first-run onboarding — so the CLI only carries run-time flags.

use clap::Parser;

/// bukagu — mirror one read-only source folder into many destination folders.
#[derive(Debug, Parser)]
#[command(name = "bukagu", version, about)]
pub struct Cli {
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
