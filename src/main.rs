mod app;
mod backup;
mod cli;
mod core;
mod credentials;
mod message;
mod store;
mod theme;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::app::RunOptions;
use crate::backup::key::{HttpKeyProvider, KeyProvider, StaticKeyProvider, parse_recipient};
use crate::backup::{BackupEvent, BackupReport};
use crate::cli::{AuthAction, Cli, Command};
use crate::credentials::Credentials;
use crate::store::Store;
use crate::ui::backup::BackupOutcome;
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
    // Load a project-local `.env` (if present) into the process environment before
    // anything resolves credentials. dotenvy does NOT override variables that are
    // already set, so a real exported `BUKAGU_API_TOKEN`/`_URL` still wins over the
    // file. A missing `.env` is fine; a malformed one is surfaced as a warning only.
    if let Err(e) = dotenvy::dotenv()
        && !e.not_found()
    {
        eprintln!(
            "{}",
            theme::ansi_fg(theme::AMBER, &format!("bukagu: ignoring .env: {e}"))
        );
    }

    let args = Cli::parse();

    // Subcommands (v3) run to completion and return before the interactive home.
    if let Some(command) = args.command {
        return match command {
            Command::Auth { action } => run_auth(action),
            Command::Backup { dry_run, recipient } => run_backup_cmd(dry_run, recipient),
            Command::Restore {
                identity,
                archive,
                into,
                force,
            } => run_restore_cmd(identity, archive, into, force),
        };
    }

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
                    let mut s = Store::new(config.clone());
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
            HomeIntent::Backup => {
                // A backup only reads the source, so open_backup() requires just a
                // source (no destination) — config is Some whenever a source is set.
                let Some(config) = config else {
                    last_run = Some("Add a source before backing up.".into());
                    continue;
                };
                last_run = Some(run_backup_home(store.as_mut(), &config).await?);
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

/// `bukagu auth …`: configure or inspect the API credentials used to fetch the
/// backup encryption key (v3).
fn run_auth(action: AuthAction) -> Result<()> {
    match action {
        AuthAction::Status => auth_status(),
        AuthAction::Login { url, token_stdin } => auth_login(url, token_stdin),
    }
}

/// Report whether usable API credentials exist, and where they come from — never
/// printing the token itself.
fn auth_status() -> Result<()> {
    let path = credentials::config_path()?;
    match Credentials::load() {
        Ok(creds) => {
            theme::print_banner();
            println!(
                "{}",
                theme::ansi_fg(theme::GOLD, "API credentials are configured.")
            );
            println!(
                "  API URL:   {}  ({})",
                creds.api_url,
                source_of(credentials::URL_ENV)
            );
            println!("  API token: set  ({})", source_of(credentials::TOKEN_ENV));
            println!("  Config:    {}", path.display());
        }
        Err(err) => {
            println!(
                "{}",
                theme::ansi_fg(theme::AMBER, "No usable API credentials yet.")
            );
            println!("  {err:#}");
            println!("  Config file: {}", path.display());
        }
    }
    Ok(())
}

/// Whether a value currently comes from the environment override or the file.
fn source_of(env_name: &str) -> &'static str {
    if std::env::var_os(env_name).is_some() {
        "from the environment"
    } else {
        "from the config file"
    }
}

/// Save the API token (read from a hidden prompt or stdin) and base URL.
fn auth_login(url: Option<String>, token_stdin: bool) -> Result<()> {
    // Validate the URL before asking for a token, so a typo fails fast.
    let url = match url {
        Some(u) => u,
        None => prompt_line("API base URL (https://…): ")?,
    };
    let url = credentials::validate_url(&url)?;

    let token = read_token(token_stdin)?;
    let path = Credentials::save(&token, &url)?;

    theme::print_banner();
    println!(
        "{}",
        theme::ansi_fg(
            theme::GOLD,
            &format!(
                "Saved API credentials to {} (permissions 0600).",
                path.display()
            )
        )
    );
    println!("Next: `bukagu auth status` to confirm, then `bukagu backup`.");
    Ok(())
}

/// Print `prompt`, then read one trimmed line from stdin (for non-secret input).
fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading input")?;
    Ok(line.trim().to_string())
}

/// Read the API token without echoing it: from a hidden terminal prompt, or from
/// stdin when piped (or when `--token-stdin` is set). Never taken as a CLI arg.
fn read_token(token_stdin: bool) -> Result<String> {
    use std::io::IsTerminal;
    let token = if token_stdin || !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the token from stdin")?;
        line
    } else {
        rpassword::prompt_password("Paste your API token (input hidden): ")
            .context("reading the token")?
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("no token provided");
    }
    Ok(token)
}

/// `bukagu restore`: decrypt an archive with the user's private age identity and
/// unpack it. The store (when present) supplies the source for the guard and the
/// default archive location; restoring on another machine without a store works
/// too (with explicit `--archive`).
fn run_restore_cmd(
    identity: Option<String>,
    archive: Option<PathBuf>,
    into: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let identity_text = read_identity(identity)?;
    let id = backup::restore::parse_identity(&identity_text)?;

    store::normalize_cwd().ok();
    let store = Store::load().ok().flatten();
    let source = store.as_ref().map(|s| s.config.source.clone());

    let archive_path = match archive {
        Some(path) => path,
        None => {
            let store = store
                .as_ref()
                .context("no --archive given and no store here — pass --archive <file>")?;
            let project_root = std::env::current_dir().context("reading the current directory")?;
            let backup_dir = backup::resolve_backup_dir(&store.backup, &project_root)?;
            backup::restore::newest_archive(&backup_dir)?
                .with_context(|| format!("no backups found in {}", backup_dir.display()))?
        }
    };

    let target =
        into.unwrap_or_else(|| PathBuf::from(format!("bukagu-restore-{}", store::now_compact())));

    backup::restore::restore(&id, &archive_path, &target, source.as_deref(), force)?;

    theme::print_banner();
    println!(
        "{}",
        theme::ansi_fg(
            theme::GOLD,
            &format!("Restored {} → {}", archive_path.display(), target.display())
        )
    );
    Ok(())
}

/// Read the age identity for restore without echoing it: a literal / `@path` / `-`
/// from the flag, else a hidden prompt (or stdin when piped). `parse_identity`
/// later interprets `@path` and picks the secret-key line.
fn read_identity(arg: Option<String>) -> Result<String> {
    use std::io::{IsTerminal, Read};
    match arg.as_deref() {
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("reading the identity from stdin")?;
            Ok(s)
        }
        Some(text) => Ok(text.to_string()),
        None => {
            if std::io::stdin().is_terminal() {
                rpassword::prompt_password("Paste your age identity (AGE-SECRET-KEY-1…, hidden): ")
                    .context("reading the identity")
            } else {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .context("reading the identity from stdin")?;
                Ok(s)
            }
        }
    }
}

/// `bukagu backup`: write one encrypted archive of the source into
/// `~/bukagu-backups/<project>`, then prune to the retention count. `--dry-run`
/// guards and fetches the key but writes nothing.
fn run_backup_cmd(dry_run: bool, recipient_override: Option<String>) -> Result<()> {
    store::normalize_cwd()?;
    let mut store = Store::load()
        .context("could not read the store — run `bukagu --reset` to re-onboard")?
        .context("no source configured yet — run `bukagu` and pick a source folder first")?;

    let source = store.config.source.clone();
    if !source.exists() {
        bail!("the source folder {} no longer exists", source.display());
    }

    let project_root = std::env::current_dir().context("reading the current directory")?;
    let backup_dir = backup::resolve_backup_dir(&store.backup, &project_root)?;
    let retention = store.backup.effective_retention();

    // Where the recipient comes from: the hidden offline override, or the API.
    let provider: Box<dyn KeyProvider> = match recipient_override {
        Some(text) => Box::new(StaticKeyProvider::new(parse_recipient(&text)?)),
        None => Box::new(HttpKeyProvider::new(Credentials::load()?)),
    };

    let report = backup::run_backup(
        &source,
        &backup_dir,
        provider.as_ref(),
        retention,
        dry_run,
        &mut |event| print_backup_progress(event),
    )?;

    if !dry_run {
        store.mark_backed_up();
        store.save().context("stamping last_backup")?;
    }
    print_backup_report(&report);
    Ok(())
}

/// "Backup now" from the home screen: resolve the backup dir + retention from the
/// store's backup settings, run the TUI backup screen, stamp `last_backup`, and
/// return a one-line summary for the home's "Last run" line.
async fn run_backup_home(store: Option<&mut Store>, config: &store::Config) -> Result<String> {
    let source = config.source.clone();
    if !source.exists() {
        return Ok(format!(
            "Backup skipped — source {} no longer exists.",
            source.display()
        ));
    }

    let project_root = std::env::current_dir().context("reading the current directory")?;
    let settings = store.as_ref().map(|s| s.backup.clone()).unwrap_or_default();
    let backup_dir = backup::resolve_backup_dir(&settings, &project_root)?;
    let retention = settings.effective_retention();

    let outcome = ui::backup::run(source, backup_dir, retention).await?;
    Ok(match outcome {
        BackupOutcome::Done(report) => {
            if let Some(store) = store {
                store.mark_backed_up();
                store.save().context("stamping last_backup")?;
            }
            format!(
                "Backup complete — {} file(s) → {} ({}).",
                report.files,
                report.archive_path.display(),
                ui::widgets::human_bytes(report.bytes)
            )
        }
        BackupOutcome::Failed(msg) => format!("Backup failed — {msg}"),
    })
}

/// Overwrite a single status line with the current backup phase.
fn print_backup_progress(event: BackupEvent) {
    use std::io::Write;
    let line = match event {
        BackupEvent::FetchingKey => "Fetching the encryption key…".to_string(),
        BackupEvent::Archiving => "Archiving and encrypting…".to_string(),
        BackupEvent::Progress { bytes } => {
            format!(
                "Archiving and encrypting… {}",
                ui::widgets::human_bytes(bytes)
            )
        }
        BackupEvent::Pruning => "Pruning old backups…".to_string(),
    };
    // `\r` returns to column 0; `\x1b[K` clears any leftover from a longer line.
    print!("\r\x1b[K{line}");
    std::io::stdout().flush().ok();
}

/// Print the final one-line backup summary (on a fresh line after progress).
fn print_backup_report(report: &BackupReport) {
    println!();
    theme::print_banner();
    if report.dry_run {
        println!(
            "{}",
            theme::ansi_fg(
                theme::AMBER,
                &format!(
                    "Dry run — would back up {} file(s) (~{}) to {}. Nothing written.",
                    report.files,
                    ui::widgets::human_bytes(report.bytes),
                    report.archive_path.display()
                )
            )
        );
    } else {
        let mut msg = format!(
            "Backup complete — {} file(s) → {} ({} encrypted).",
            report.files,
            report.archive_path.display(),
            ui::widgets::human_bytes(report.bytes)
        );
        if report.pruned > 0 {
            msg.push_str(&format!(" Pruned {} old archive(s).", report.pruned));
        }
        println!("{}", theme::ansi_fg(theme::GOLD, &msg));
    }
}
