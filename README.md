# bukagu

A Swiss Army knife CLI/TUI for developers, written in Rust. The first tool it ships
with is **folder syncing**: mirror one master folder into many backups, on demand,
from a colorful terminal dashboard.

> **Safety first.** The source folder is **read-only to bukagu** — it only ever writes
> into destinations. This is enforced in code (canonicalized path checks before any
> scan *and* before any write), not just by convention. bukagu refuses to run if a
> destination equals, sits inside, or contains the source.

## What it does (v1)

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

## Install / build

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

### Flags

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

In v1, **symlinks are skipped entirely** — a symlink to a file or a directory is never
copied, hashed, or deleted, and symlinked directories are not descended into. This keeps
a mirror predictable: it never follows a link out of the source tree and never recreates
one in a destination. (Richer symlink modes may come in a later version.)

## State file

bukagu stores your configuration at `./.bukagu/bukagu-store.json`:

```json
{
  "version": 1,
  "source": "/path/to/master",
  "destinations": ["/path/to/backup-a", "/path/to/backup-b"],
  "created_at": "2026-06-13T10:00:00Z",
  "last_sync": "2026-06-13T11:30:00Z"
}
```

`last_sync` is stamped after each successful (non-dry-run) sync.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
