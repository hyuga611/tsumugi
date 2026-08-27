#!/bin/sh
# tsumugi installer (macOS / Linux).
#
#   curl -fsSL https://raw.githubusercontent.com/hyuga611/tsumugi/main/install.sh | sh
#
# Downloads the latest tsg for your machine and runs `tsg --install`, which
# symlinks ~/.local/bin/tsg, installs the shell integration, and wires the
# Claude Code / Codex hooks if they are there. No sudo: it only writes under
# your home directory.
#
# ⚠️ macOS and Linux are NOT verified. Building and the test suite are checked
# in CI, but the window (winit / wgpu), the IME and `--install` have parts
# written against Windows APIs, and nobody has run them yet. Please say either
# way: https://github.com/hyuga611/tsumugi/issues
#
# Options are environment variables, set them before the line above:
#   TSUMUGI_DIR=/opt/bin      # where to put it (default: ~/.local/bin)
#   TSUMUGI_VERSION=v0.3.0    # which release (default: latest)
#   TSUMUGI_NO_REGISTER=1     # just drop the binary, no shell integration
#   TSUMUGI_FORCE=1           # reinstall even if already on that version

set -eu

REPO='hyuga611/tsumugi'
DIR="${TSUMUGI_DIR:-$HOME/.local/bin}"
VERSION="${TSUMUGI_VERSION:-latest}"

# Which asset. Keep the names in step with .github/workflows/release.yml.
case "$(uname -s)" in
    Darwin)
        case "$(uname -m)" in
            arm64|aarch64) ASSET='tsg-macos-arm64' ;;
            *)
                echo "Intel Macs have no binary yet. Build from source:" >&2
                echo "  git clone https://github.com/$REPO && cd tsumugi && cargo build --release" >&2
                exit 1
                ;;
        esac
        ;;
    Linux)
        case "$(uname -m)" in
            x86_64) ASSET='tsg-linux-x86_64' ;;
            *)
                echo "$(uname -m) has no binary yet. Build from source:" >&2
                echo "  git clone https://github.com/$REPO && cd tsumugi && cargo build --release" >&2
                exit 1
                ;;
        esac
        ;;
    *)
        echo "Windows? Use PowerShell instead:" >&2
        echo "  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" >&2
        exit 1
        ;;
esac

# One of these is always around on a machine that just ran curl.
fetch() {
    if command -v curl > /dev/null 2>&1; then
        curl -fsSL "$1"
    else
        wget -qO- "$1"
    fi
}

if [ "$VERSION" = latest ]; then
    API="https://api.github.com/repos/$REPO/releases/latest"
else
    API="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
fi

echo "Fetching tsumugi ($VERSION)..."
RELEASE="$(fetch "$API")"

TAG="$(printf '%s' "$RELEASE" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
if [ -z "$TAG" ]; then
    echo "Could not read that release. Is $VERSION a real tag?" >&2
    exit 1
fi

# `tsg update` says which version it is running. Downloading the same 20 MB
# again buys nothing. A first install never sets it.
if [ -n "${TSUMUGI_HAVE:-}" ] && [ -z "${TSUMUGI_FORCE:-}" ] && [ "$TAG" = "v$TSUMUGI_HAVE" ]; then
    echo "Already on $TAG."
    exit 0
fi

URL="$(printf '%s' "$RELEASE" \
    | tr ',' '\n' \
    | sed -n "s|.*\"browser_download_url\": *\"\\([^\"]*/$ASSET\\)\".*|\\1|p" \
    | head -n 1)"
if [ -z "$URL" ]; then
    echo "$TAG has no $ASSET. Build from source:" >&2
    echo "  git clone https://github.com/$REPO && cd tsumugi && cargo build --release" >&2
    exit 1
fi

mkdir -p "$DIR"
EXE="$DIR/tsg"

# A running tsumugi holds the file open on some systems, and replacing a
# running binary in place is asking for a half-written file either way.
# Move it aside first; the copy you have open keeps running.
MOVED=''
if [ -e "$EXE" ]; then
    MOVED="$EXE.old-$$"
    mv "$EXE" "$MOVED"
fi
# Leftovers from an earlier install, now that nothing holds them.
for old in "$DIR"/tsg.old-*; do
    [ -e "$old" ] || continue
    [ "$old" = "$MOVED" ] && continue
    rm -f "$old"
done

if ! fetch "$URL" > "$EXE.part"; then
    rm -f "$EXE.part"
    [ -n "$MOVED" ] && mv "$MOVED" "$EXE"
    echo "Could not download $URL" >&2
    exit 1
fi
chmod +x "$EXE.part"
mv "$EXE.part" "$EXE"
echo "Installed: $EXE ($TAG)"

# Check it actually starts. Quarantine (macOS) and hardening show up here.
if ! "$EXE" --version > /dev/null 2>&1; then
    rm -f "$EXE"
    [ -n "$MOVED" ] && mv "$MOVED" "$EXE"
    cat >&2 <<EOF
Could not start: $EXE
On macOS, Gatekeeper blocks unsigned downloads. To let this one through:
  xattr -d com.apple.quarantine $EXE
Put it somewhere else with:  TSUMUGI_DIR=/opt/bin
EOF
    exit 1
fi
echo "Verified it starts ($TAG)"

[ -n "$MOVED" ] && rm -f "$MOVED"

if [ -z "${TSUMUGI_NO_REGISTER:-}" ]; then
    "$EXE" --install || true
fi

echo ''
case ":$PATH:" in
    *":$DIR:"*) ;;
    *) echo "Add $DIR to your PATH to type \`tsg\` from anywhere." ;;
esac
echo 'Done. Open a new shell and type `tsg`.'
echo 'Shell integration and agent hooks went in too. `tsg --uninstall` takes it all out.'
echo ''
echo 'macOS and Linux are not verified yet. Please say how it went:'
echo "  https://github.com/$REPO/issues"
