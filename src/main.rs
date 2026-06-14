mod app;
mod cli;
mod core;
mod message;
mod store;
mod theme;
mod ui;

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use crate::app::RunOptions;
use crate::cli::Cli;
use crate::store::Store;
use crate::ui::browser::HomeIntent;
use crate::ui::dashboard::Outcome;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // One friendly line on stderr instead of anyhow's raw `Error: {:?}`
            // debug dump. `{err:#}` flattens the whole context chain, e.g.
            // "bukagu: applying to /backup: copying a -> b: Permission denied".
            eprintln!(
                "{}",
                theme::ansi_fg(theme::ERROR, &format!("bukagu: {err:#}"))
            );
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Cli::parse();

    // Anchor to the project root before anything touches `.bukagu/`: if launched
    // from inside the store dir, step up to its parent so the store and the
    // onboarding browser don't land at `.bukagu/.bukagu`.
    store::normalize_cwd()?;

    let opts = RunOptions {
        dry_run: args.dry_run,
        yes: args.yes,
        delete: args.delete,
    };

    // `--reset` ignores any saved store and re-runs first-time setup from scratch.
    let mut store = if args.reset {
        None
    } else {
        Store::load().context("could not read the store — run `bukagu --reset` to re-onboard")?
    };

    // Unattended fast path: a returning `--yes` run syncs straight away with no
    // home screen, so scripts/CI keep working. (Without a store there's nothing to
    // sync yet, so `--yes` falls through to the interactive home to set one up.)
    if args.yes
        && let Some(store) = store.as_mut()
    {
        let config = store.config.clone();
        let summary = run_sync(Some(store), config, opts).await?;
        theme::print_banner();
        println!("{}", theme::ansi_fg(theme::GOLD, &summary));
        return Ok(());
    }

    // The home screen (actions + info) is the hub for every interactive run:
    // pick/edit the source and destinations, or "Sync now". It loops so the
    // actions are always there to return to after a sync.
    let mut last_run: Option<String> = None;
    loop {
        let initial = store.as_ref().map(|s| s.config.clone());
        let mappings = store
            .as_ref()
            .map(|s| s.mappings.clone())
            .unwrap_or_default();
        let (config, intent) = ui::browser::run_home(initial, mappings, last_run.take())?;

        // Persist any edits the user made in the home (even before a sync, and even
        // if they're about to quit), preserving the store's created_at/last_sync.
        if let Some(config) = &config {
            match &mut store {
                Some(s) if s.config != *config => {
                    s.config = config.clone();
                    s.save().context("saving your edited folders")?;
                }
                Some(_) => {}
                None => {
                    let s = Store::new(config.clone());
                    s.save().context("writing the store")?;
                    store = Some(s);
                }
            }
        }

        match intent {
            HomeIntent::Quit => break,
            HomeIntent::Sync => {
                // confirm() only fires with a source + ≥1 destination, so this holds.
                let Some(config) = config else {
                    last_run = Some("Add a source and a destination before syncing.".into());
                    continue;
                };
                last_run = Some(run_sync(store.as_mut(), config, opts).await?);
            }
            HomeIntent::Mappings => {
                // open_mappings() also requires a source + ≥1 destination.
                let Some(config) = config else {
                    last_run = Some("Add a source and a destination before mapping files.".into());
                    continue;
                };
                last_run = run_mappings(store.as_mut(), &config, opts)?;
            }
        }
    }

    theme::print_banner();
    println!(
        "{}",
        theme::ansi_fg(theme::AMBER, "Closed bukagu — see you next time.")
    );
    Ok(())
}

/// Run one sync via the dashboard and fold the outcome into a one-line summary to
/// show back on the home screen. Stamps `last_sync` on a real (non-dry) apply.
async fn run_sync(
    store: Option<&mut Store>,
    config: store::Config,
    opts: RunOptions,
) -> Result<String> {
    let outcome = ui::dashboard::run(config, opts).await?;
    Ok(match outcome {
        Outcome::Applied(stats) => {
            if let Some(store) = store {
                store.mark_synced();
                store.save().context("stamping last_sync")?;
            }
            format!(
                "Sync complete — {} copied, {} updated, {} deleted ({} bytes).",
                stats.copies, stats.overwrites, stats.deletes, stats.bytes
            )
        }
        Outcome::Preview(stats) => format!(
            "Dry run — {} change(s) previewed, nothing written.",
            stats.copies + stats.overwrites + stats.deletes + stats.create_dirs
        ),
        Outcome::UpToDate => "Everything is already in sync — nothing to do.".into(),
        Outcome::Cancelled => "Sync cancelled — no changes made.".into(),
    })
}

/// Open the v2 file-mapping screen, persist the edited mappings back to the store,
/// and stamp `last_sync` on a real (non-dry-run) mapping sync. Returns a one-line
/// summary for the home's "Last run" line, or `None` if no sync ran.
fn run_mappings(
    store: Option<&mut Store>,
    config: &store::Config,
    opts: RunOptions,
) -> Result<Option<String>> {
    let existing = store
        .as_ref()
        .map(|s| s.mappings.clone())
        .unwrap_or_default();
    let session = ui::mappings::run(config, existing, opts)?;
    if let Some(store) = store {
        store.mappings = session.mappings;
        if session.synced {
            store.mark_synced();
        }
        store.save().context("saving your file mappings")?;
    }
    Ok((!session.summary.is_empty()).then_some(session.summary))
}
