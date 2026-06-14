#!/bin/sh
# bukagu installer — downloads a prebuilt static binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/karatay-lab/bukagu/main/install.sh | sh
#
# Env overrides:
#   BUKAGU_VERSION      tag to install (default: latest release, e.g. v0.1.0)
#   BUKAGU_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
set -eu

REPO="karatay-lab/bukagu"
BIN="bukagu"
INSTALL_DIR="${BUKAGU_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- detect platform -------------------------------------------------------
os="$(uname -s)"
[ "$os" = "Linux" ] || err "this installer supports Linux only (got $os). On macOS use Homebrew or 'cargo install bukagu'."

case "$(uname -m)" in
  x86_64 | amd64)  target="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
  *) err "unsupported architecture: $(uname -m)" ;;
esac

# --- pick a downloader -----------------------------------------------------
if have curl; then
  dl() { curl -fsSL "$1" -o "$2"; }
  fetch() { curl -fsSL "$1"; }
elif have wget; then
  dl() { wget -qO "$2" "$1"; }
  fetch() { wget -qO - "$1"; }
else
  err "need curl or wget"
fi

# --- resolve version -------------------------------------------------------
tag="${BUKAGU_VERSION:-}"
if [ -z "$tag" ]; then
  tag="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | head -n1 | cut -d'"' -f4)"
  [ -n "$tag" ] || err "could not determine latest release; set BUKAGU_VERSION"
fi

archive="${BIN}-${tag}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$archive"

# --- download, verify, extract --------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

printf 'Downloading %s %s (%s)...\n' "$BIN" "$tag" "$target"
dl "$url" "$tmp/$archive" || err "download failed: $url"

if dl "$url.sha256" "$tmp/$archive.sha256" 2>/dev/null && have sha256sum; then
  ( cd "$tmp" && want="$(awk '{print $1}' "$archive.sha256")" \
    && echo "$want  $archive" | sha256sum -c - >/dev/null ) || err "checksum mismatch"
fi

tar -xzf "$tmp/$archive" -C "$tmp"
binpath="$(find "$tmp" -type f -name "$BIN" | head -n1)"
[ -n "$binpath" ] || err "binary '$BIN' not found in archive"

mkdir -p "$INSTALL_DIR"
install -m 755 "$binpath" "$INSTALL_DIR/$BIN"
printf 'Installed %s to %s\n' "$BIN" "$INSTALL_DIR/$BIN"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf '\nAdd it to your PATH:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR" ;;
esac
