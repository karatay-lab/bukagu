# bukagu

A Swiss Army knife CLI/TUI for developers, written in Rust. It ships with **folder
syncing** (mirror one master folder into many backups, on demand, from a colorful
terminal dashboard) and **encrypted off-site backups** of that master folder.

## Install

bukagu ships as a single static binary with no runtime dependencies, so it runs
on any Linux distro regardless of glibc version.

### Quick install (Linux, x86_64 / aarch64)

```bash
curl -fsSL https://raw.githubusercontent.com/karatay-lab/bukagu/main/install.sh | sh
```

Installs to `~/.local/bin` by default. Override with `BUKAGU_INSTALL_DIR=/usr/local/bin`
(may need `sudo`) or pin a version with `BUKAGU_VERSION=v0.1.0`.

### Debian / Ubuntu (.deb)

Download the `.deb` for your architecture from the
[latest release](https://github.com/karatay-lab/bukagu/releases/latest), then:

```bash
sudo apt install ./bukagu-*.deb     # or: sudo dpkg -i bukagu-*.deb
```

### Homebrew (Linux & macOS)

```bash
brew install karatay-lab/tap/bukagu
```

### From crates.io (needs a Rust toolchain)

```bash
cargo install bukagu
```

### Prebuilt binaries

Grab a `.tar.gz` for your platform from the
[releases page](https://github.com/karatay-lab/bukagu/releases), extract, and put
`bukagu` somewhere on your `PATH`. Each archive ships a `.sha256` checksum.

## Uninstall

Remove the binary using **only** the line that matches how you installed it (the others will
just report "not found"):

| Installed with | Remove with |
| --- | --- |
| Quick install / prebuilt binary | `rm ~/.local/bin/bukagu` (or your `$BUKAGU_INSTALL_DIR` / PATH location) |
| Debian / Ubuntu (`.deb`) | `sudo apt remove bukagu` |
| Homebrew | `brew uninstall bukagu` |
| crates.io | `cargo uninstall bukagu` |

bukagu keeps its state outside the binary, so removing the program leaves it behind. Delete
whatever you no longer want (each is safe to skip if it isn't there):

```bash
rm -rf .bukagu                  # per-project config (the source/destination/mapping store)
rm -rf ~/.config/bukagu         # saved API credentials (token + URL)
rm -rf ~/bukagu-backups         # encrypted backup archives — see the caution below
```

> **Keep your backups in mind.** `~/bukagu-backups` holds your encrypted archives. Only your
> private age identity can decrypt them — deleting this folder is permanent and irreversible.
> Move it somewhere safe rather than deleting it if you might still need to restore.

> **Safety first.** The source folder is **read-only to bukagu** — it only ever writes
> into destinations (or, for backups, into `~/bukagu-backups`). This is enforced in code
> (canonicalized path checks before any scan *and* before any write), not just by convention.
> bukagu refuses to run if a destination — or a backup/restore folder — equals, sits inside,
> or contains the source.

## What it does

- **One-way mirror.** A single **source** folder (your master files) is mirrored into a
  list of **destination** folders. Each destination is made to match the source,
  matched by relative path.
- **On demand.** You run it when you want a sync. There is no background watcher.
- **Content-aware.** Files are compared by a **blake3** content hash, with a size
  pre-check first — so identical files are never needlessly rehashed or recopied, and a
  size difference settles a comparison without hashing at all.
- **Interactive.** Every run opens a **home** screen with an actions pane pinned on top —
  pick/edit the source and destinations, or **Sync now** (scan → review the color-coded diff →
  apply), as many times as you like in one session.

The choice of folders is saved to `./.bukagu/bukagu-store.json` (relative to the
directory you launch from), so subsequent runs go straight to the dashboard.

## Build from source

Requires a recent stable Rust toolchain (Rust 2024 edition, ≥ 1.85).

```bash
# Build a release binary at target/release/bukagu
cargo build --release

# …or just run it from the repo
cargo run --release
```

## Usage

Run `bukagu` from the directory where you want its `.bukagu/` state to live.

### The home screen

Every interactive run opens the **home**: an **actions** pane pinned on top and an **info**
pane below it. The actions are always there, so you can edit your folders or sync as often as
you like without restarting:

- **Select source folder** — choose the read-only master folder.
- **Select destination folder** — add/remove the folders it mirrors into.
- **Sync now** — mirror the source into every destination (opens the sync dashboard, then
  returns here with the result shown under *Last run*).
- **Map files** — open the **file-mapping** screen: map individual source files to specific
  destination files, each written with a "managed by bukagu" banner. See *File mappings* below.
- **Backup now** — make an **encrypted backup** of the source into `~/bukagu-backups` (needs only a
  source — no destination). See *Encrypted backups* below.

On a first run the home starts empty; on later runs it opens pre-filled from your saved store.
Any edit you make is saved back to the store immediately. Activating *Select source/destination*
opens a **full-screen folder browser**; navigate with the arrows (`→`/`l`/`Enter` open a folder,
`←`/`Backspace` go up) and press **`Space`** to pick the highlighted folder in place — you don't
have to open it first — or `Esc` to go back. The **source** is a single pick (it returns straight
away); **destinations** are multi-select — `Space` selects the highlighted folder, `Space` again
deselects it, and `Esc` returns when you're done. Press `r` at any depth to jump back to the
project root (where bukagu was launched).

**Home view:**

| Keys | Action |
| --- | --- |
| `Tab` | Switch focus between the actions and info panes |
| `↑` / `↓` | Move within the focused pane |
| `Enter` | Actions pane: open the folder browser / run **Sync now** · Info pane: reveal the destination list / expand a folder's stats |
| `Backspace` / `←` | Info pane: collapse the open folder, then the list |
| `s` / `a` | Jump straight to choosing a **source** / adding a **destination** |
| `c` | **Sync now** (mirror source → destinations) |
| `m` | **Map files** (open the file-mapping screen) |
| `b` | **Backup now** (encrypted backup of the source) |
| `Ctrl+Q` | Toggle a full-screen overlay listing every shortcut |
| `q` / `Esc` | Leave the home (any edits are already saved) |

**Folder browser (modal):**

| Keys | Action |
| --- | --- |
| `↑` / `↓` (or `j` / `k`) | Move the selection |
| `→` / `l` / `Enter` | Open (descend into) the highlighted folder |
| `←` / `Backspace` / `h` | Go up to the parent |
| `r` | Jump back to the launch folder (where bukagu was opened) |
| `Space` | Source: **pick** and return · Destination: **select / deselect** (toggle) |
| `Esc` / `q` | Finish / cancel and return to the main view |

The browser always opens at the **project root** (the folder you launched bukagu in, where
`.bukagu/` lives) — `r` jumps back there any time, and the header shows it as `Root Project`.

Pick a source, then add one or more destinations, then choose **Sync now** (or press `c`).
In the destination picker, the **source** — and any folder inside or containing it — is shown
**grayed out and disabled** (it can't be a destination, since the source is never written to).
Choosing a source that overlaps an already-added destination drops that destination automatically
from the list. The info pane is an **accordion**: the source's
stats — folder size, file count, and mapped-file count (`0` until a sync runs) — are always
shown at the top, and the `Destination Folders` header expands (on `Enter`) into the destination
list — `Enter` on a destination reveals *its* stats inline, one open at a time. After a sync,
the result appears as a **Last run** line at the top of the info pane.

### The sync dashboard

Choosing **Sync now** opens the dashboard: bukagu scans the source and every destination, shows
a per-destination color-coded diff, and applies on confirmation, then returns you to the home:

| Keys | Action |
| --- | --- |
| `↑` / `↓` (or `j` / `k`) | Scroll the review list |
| `PgUp` / `PgDn` | Page through the review list |
| `Enter` / `y` | Apply the planned changes |
| `q` / `Esc` | Cancel (ignored *during* applying) |

At the Done/Error screen, `Enter` or `q` returns to the home. Passing `-y`/`--yes` skips both
the home and the Review step, so a returning `bukagu --yes` syncs straight away (handy for
scripts).

### File mappings

Where the folder mirror copies *whole folders* by relative path, **file mappings** let you wire up
individual files: one **source file → one or more destination files**. Choose **Map files** (or
press `m`) on the home to open the mapping screen — a **two-pane page**: **Sources** on the left,
**Destinations** on the right, and a destination-folder info accordion below.

1. **Pick a source** (`Space` on the left): the chosen file gets a sticky highlight.
2. **Pick targets** (right pane): the destinations are an **accordion by folder** — `Space` opens a
   folder, `Space` on a file selects it (a distinct colour + `✓`; multi-select across folders, `Space`
   again deselects). Targets are existing destination files, and a target's **extension must match the
   source's** (you map files of the same type).
3. **Save** (`Enter`): the source → selected targets become a mapping. Press `a` to flip the right pane
   between **available** (unmapped) and **assigned** files; in the assigned view `Space`/`d` unmaps one.
   Press `r` to reload both folders if you add/remove files while bukagu is open.
4. **Review** (`s`): a summary lists every `source → target` pair marked `create` / `update` / `ok`,
   with rolled-up counts and any blocking issues.
5. **Sync** (`Enter`): each target is overwritten with its source's contents, preceded by a banner:

   ```text
   # Managed by bukagu — do not edit this file; it is overwritten on every sync. Edit the source instead: /path/to/master/app.py

   …your source file's contents…
   ```

   The comment marker matches the **target's extension** — `//` for `.js`/`.ts`/`.rs`/…, `#` for
   `.py`/`.sh`/`.yaml`/…, `--` for `.sql`/`.lua`, `/* … */` for CSS, `<!-- … -->` for HTML/Markdown.
   A leading `#!` shebang or byte-order mark is kept on top, and re-syncing **replaces** the banner
   rather than stacking it. Files with an unknown extension (or binary content) are copied verbatim
   **without** a banner. When the sync finishes you'll see `Last sync: OK · <timestamp>`.

Two rules are enforced: bukagu never writes a target inside the read-only source, and **a
destination file can be the target of at most one source** (no double-writes). Both the editor and
the pre-sync summary flag any violation. Mappings are saved in the store alongside your folders.

| Keys | Action |
| --- | --- |
| `Tab` | Cycle focus: Sources → Destinations → info accordion |
| `↑` / `↓` | Move within the focused pane |
| `Space` | Pick the source / open a folder / select a target (assigned view: unmap) |
| `Enter` | Save the selected source → targets as a mapping |
| `a` | Toggle the destinations pane between available and assigned files |
| `d` | (assigned view) unmap the highlighted file |
| `r` | Reload — re-read the source and destination folders |
| `s` | Review the summary, then sync |
| `q` / `Esc` | Leave the mapping screen (asks you to confirm) |

### Encrypted backups

bukagu can also keep **encrypted, off-site backups of the source folder itself** — not just mirror it
into local destinations. Each backup is one timestamped, `age`-encrypted archive
(`tar` → gzip → encrypt) written under `~/bukagu-backups/<project>/`.

The encryption is **asymmetric**, and that's the point: bukagu fetches only a **public recipient key**
from your own web service and encrypts *to* it. The machine running bukagu can make backups but can
**never decrypt them** — only the matching **private identity**, which you keep on your website, can.
A lost or compromised laptop therefore can't read your past backups.

**One-time setup.** Log in to your website, copy your API token, and save it:

```bash
bukagu auth login --url https://your-api.example.com   # paste the token at the hidden prompt
bukagu auth status                                      # confirm (never prints the token)
```

The token and URL are stored in `~/.config/bukagu/credentials.json` (`chmod 600`), overridable with the
`BUKAGU_API_TOKEN` / `BUKAGU_API_URL` environment variables. A project-local `.env` is auto-loaded at
startup too (a real exported variable still wins; `.env` is gitignored — see `.env.example`). Precedence:
**exported env → `.env` → the `0600` config file**. Credentials are never written into the repo's
`.bukagu/` store. If your API serves the key under a route other than `/recipient`, set
`BUKAGU_API_RECIPIENT_PATH`.

**Make a backup** — from the home screen press **`b`** (*Backup now*), or from the CLI:

```bash
bukagu backup            # encrypt the source → ~/bukagu-backups/<project>/<timestamp>.tar.gz.age
bukagu backup --dry-run  # preview (fetch the key, count files) without writing
```

bukagu keeps the newest **10** archives per project by default and prunes older ones.

**Restore** (on any machine) needs your **private age identity**, obtained from your website:

```bash
bukagu restore --identity @age-key.txt                 # newest backup → ./bukagu-restore-<timestamp>
bukagu restore --identity @age-key.txt \
  --archive ~/bukagu-backups/myproj/2026….tar.gz.age --into /tmp/restored
```

Pass the identity as a file (`@path`), via stdin (`-`), or at the hidden prompt — not as a flag value,
so it doesn't land in your shell history. Restore **never** writes into the read-only source: it refuses
a target that equals, sits inside, or contains the source — even with `--force` (which only lets you
restore into an existing non-empty directory).

**The API contract.** bukagu expects your service to expose one endpoint:

```text
GET {api_url}/recipient
Authorization: Bearer <token>
→ 200, body: an age recipient public key, e.g. "age1qz…"
```

bukagu calls it over **HTTPS only** (rustls), never follows redirects carrying the auth header, and
rejects any response that isn't a valid `age1…` recipient — so the private identity never leaves your
site and a tampered response can't redirect your backups to someone else's key.

### Flags

The bare `bukagu` opens the home screen. bukagu also has three **subcommands** — `bukagu auth`,
`bukagu backup`, and `bukagu restore` (see *Encrypted backups* above). The flags below apply to the
default (no-subcommand) sync run.

| Flag | Effect |
| --- | --- |
| `--dry-run` | Preview the plan; write nothing to any destination. |
| `-y`, `--yes` | Skip the interactive Review and apply immediately. |
| `--delete` | Delete files in a destination that no longer exist in the source. Off by default — extras are left untouched. |
| `--reset` | Ignore the saved store and re-run onboarding. |

```bash
bukagu --dry-run          # see what would change
bukagu --yes              # sync without the confirmation step
bukagu --yes --delete     # mirror exactly, removing destination-only files
bukagu --reset            # pick new source/destinations
```

## How comparison works

For each path that exists in both the source and a destination:

1. If the file **sizes differ**, it's an overwrite — no hashing needed.
2. If the sizes **match**, both files are hashed with blake3 and overwritten only if
   the hashes differ.

Paths only in the source are copied (files) or created (directories). Paths only in a
destination are left alone unless `--delete` is given.

## Symlinks

**Symlinks are skipped entirely** — a symlink to a file or a directory is never copied,
hashed, deleted, or archived, and symlinked directories are not descended into. This
applies wherever bukagu walks a tree: the folder mirror, the encrypted backup, and the
file-mapping screen (which only ever lists real files to map). It keeps behavior
predictable: bukagu never follows a link out of the source tree and never recreates one in
a destination. (Richer symlink modes may come in a later version.)

## State file

bukagu stores your configuration at `./.bukagu/bukagu-store.json`:

```json
{
  "version": 3,
  "source": "/path/to/master",
  "destinations": ["/path/to/backup-a", "/path/to/backup-b"],
  "mappings": [
    { "source_rel": "app.py", "targets": ["/path/to/backup-a/app.py"] }
  ],
  "backup": { "last_backup": "2026-06-15T09:00:00Z" },
  "created_at": "2026-06-13T10:00:00Z",
  "last_sync": "2026-06-13T11:30:00Z"
}
```

`version` records the on-disk schema (stamped whenever bukagu writes the store). `last_sync` is
stamped after each successful (non-dry-run) sync. `mappings` holds the explicit file mappings
(`source_rel` is relative to the source; `targets` are absolute destination files). `backup` holds the
backup settings — an optional `root` (defaults to `~/bukagu-backups`), `retention` (how many archives to
keep, default 10), and `last_backup` (stamped after each successful backup). Older stores load
unchanged: a missing `mappings` or `backup` key is treated as empty/default. (API credentials are
**not** stored here — they live in `~/.config/bukagu/credentials.json`.)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
