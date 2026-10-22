#!/usr/bin/env bash
# One-shot Kiln install for an Apple Silicon Mac (e.g. a Mac mini).
#
#   curl -fsSLO https://raw.githubusercontent.com/chhhee10/kiln/main/scripts/install-macos.sh
#   bash install-macos.sh
#
# Installs what is missing of Homebrew, Docker (OrbStack), Tailscale, git and
# gh, reusing an existing Docker Desktop or Tailscale CLI; downloads
# the latest Kiln release into ~/.kiln; writes ~/.kiln/kiln.toml; publishes
# HTTPS on the tailnet; installs and starts the launchd service. It pauses
# only for things you must do yourself (passwords and sign-ins).
# Safe to re-run: finished steps are skipped. Re-run it to upgrade.
#
# To install from a fork or a private copy (releases and worker image under
# another owner), set KILN_REPO=<owner>/<repo>. Private releases and images
# are fetched with your `gh` sign-in.
set -euo pipefail

KILN_HOME="${KILN_HOME:-$HOME/.kiln}"
REPO="${KILN_REPO:-chhhee10/kiln}"
IMAGE="ghcr.io/$(echo "${REPO%/*}" | tr '[:upper:]' '[:lower:]')/kiln-worker"
TS=/Applications/Tailscale.app/Contents/MacOS/Tailscale

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
wait_for() { # wait_for "<message>" <command...>
    local msg=$1; shift
    until "$@" >/dev/null 2>&1; do
        printf '    %s (waiting…)\r' "$msg"
        sleep 3
    done
    printf '    %s ✓                \n' "$msg"
}

[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || {
    echo "This script is for Apple Silicon Macs. See docs/GETTING_STARTED.md for other systems."
    exit 1
}

if ! pmset -g | grep -qE '^ +sleep +0' || ! pmset -g | grep -qE '^ +autorestart +1'; then
    say "Keep the Mac awake and restart after power loss (needs your password)"
    sudo pmset -a sleep 0 autorestart 1
fi

if ! command -v brew >/dev/null && [ ! -x /opt/homebrew/bin/brew ]; then
    say "Installing Homebrew"
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
fi
eval "$(/opt/homebrew/bin/brew shellenv)"

say "Installing git and gh"
command -v git >/dev/null && command -v gh >/dev/null || brew install git gh

# Docker: keep a working Docker (Docker Desktop, OrbStack, Colima); else OrbStack.
export PATH="$HOME/.orbstack/bin:/usr/local/bin:/Applications/Docker.app/Contents/Resources/bin:$PATH"
if ! docker info >/dev/null 2>&1; then
    if [ -d /Applications/Docker.app ] && [ ! -d /Applications/OrbStack.app ]; then
        say "Starting Docker Desktop"
        open -a Docker
    else
        [ -d /Applications/OrbStack.app ] || brew install --cask orbstack
        say "Starting OrbStack (finish its first-run window if one opens)"
        open -a OrbStack
    fi
    wait_for "Docker is up" docker info
fi
DOCKER_SOCK=$(docker context inspect --format '{{.Endpoints.docker.Host}}' 2>/dev/null | sed 's|^unix://||')

# Tailscale: use an installed CLI (Homebrew/standalone daemon) or the app.
if command -v tailscale >/dev/null; then
    TS=$(command -v tailscale)
else
    [ -d /Applications/Tailscale.app ] || brew install --cask tailscale
    say "Tailscale: sign in from the menu-bar icon if asked"
    open -a Tailscale
fi
wait_for "Connected to your tailnet" "$TS" ip -4
TS_IP=$("$TS" ip -4 | head -1)
TS_NAME=$("$TS" status --json | python3 -c 'import sys,json;print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))')

if ! gh auth status >/dev/null 2>&1; then
    say "GitHub: sign in so Kiln can push branches and open PRs"
    gh auth login --git-protocol https --web
fi

say "Downloading the latest Kiln release"
TAG=$(gh api "repos/$REPO/releases/latest" --jq .tag_name)
DIR="$KILN_HOME/kiln-$TAG-aarch64-apple-darwin"
mkdir -p "$KILN_HOME/bin"
chmod 700 "$KILN_HOME"
if [ ! -x "$DIR/kilnd" ]; then
    gh release download "$TAG" --repo "$REPO" --pattern "kiln-$TAG-aarch64-apple-darwin.tar.gz" --output - | tar xz -C "$KILN_HOME"
    xattr -dr com.apple.quarantine "$DIR" 2>/dev/null || true
fi
for b in kilnd kiln kiln-runner; do ln -sf "$DIR/$b" "$KILN_HOME/bin/$b"; done
export PATH="$KILN_HOME/bin:$PATH"
grep -q '.kiln/bin' ~/.zprofile 2>/dev/null || echo 'export PATH="$HOME/.kiln/bin:$PATH"; export KILN_DATA_DIR="$HOME/.kiln"' >> ~/.zprofile
export KILN_DATA_DIR="$KILN_HOME"

# HTTPS via `tailscale serve` when the tailnet has HTTPS certificates on;
# otherwise plain HTTP on the tailnet (WireGuard encrypts it anyway).
# KILN_HTTPS=0 skips HTTPS.
if [ "${KILN_HTTPS:-1}" != 0 ] && "$TS" serve --bg --https=443 "http://$TS_IP:7420" >/dev/null 2>&1; then
    PUBLIC_URL="https://$TS_NAME"
else
    PUBLIC_URL="http://$TS_NAME:7420"
    echo "    Using plain HTTP on the tailnet. For HTTPS, enable HTTPS certificates in"
    echo "    the Tailscale admin console (DNS page) and re-run this script."
fi
say "Kiln address: $PUBLIC_URL"

if [ ! -f "$KILN_HOME/kiln.toml" ]; then
    say "Writing $KILN_HOME/kiln.toml"
    cat > "$KILN_HOME/kiln.toml" <<EOF
# Kiln config. All options: config/kiln.example.toml in the repository.
data_dir = "$KILN_HOME"

[server]
bind = "tailscale"
public_url = "$PUBLIC_URL"

[runtime]
worker_image = "$IMAGE:${TAG#v}"   # kept current by re-running install-macos.sh
docker_socket = "$DOCKER_SOCK"

[github]
mode = "token"   # uses \`gh auth token\`; see docs/GETTING_STARTED.md for GitHub App mode
EOF
else
    # Upgrade: point an existing config at this release's image.
    sed -i '' -E "s|^worker_image = \"[^\"]*\"|worker_image = \"$IMAGE:${TAG#v}\"|" "$KILN_HOME/kiln.toml"
    sed -i '' -E "s|^public_url = \"[^\"]*\"|public_url = \"$PUBLIC_URL\"|" "$KILN_HOME/kiln.toml"
fi

VERSION=${TAG#v}
say "Pulling the worker image (a few minutes the first time)"
if ! docker pull "$IMAGE:$VERSION" >/dev/null 2>&1; then
    # Private image: log Docker in to ghcr.io with the gh sign-in.
    gh auth status 2>&1 | grep -q read:packages || gh auth refresh -h github.com -s read:packages
    gh auth token | docker login ghcr.io -u "$(gh api user --jq .login)" --password-stdin >/dev/null
    docker pull "$IMAGE:$VERSION" >/dev/null
fi

say "Installing the Kiln service"
kilnd install --force
PLIST=~/Library/LaunchAgents/dev.kiln.kilnd.plist
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"
wait_for "Kiln is running" curl -fsS "http://$TS_IP:7420/healthz"

say "Checking the setup"
kilnd doctor || true

cat <<EOF

Kiln is running.

  Open:        $PUBLIC_URL/app/   (from any device on your tailnet)
  Next:        create the admin account, then Settings → Add credential
               (model key or LiteLLM), then Admin → Add project.
  Terminal:    kiln --server $PUBLIC_URL
  Logs:        $KILN_HOME/logs/
  Apps:        https://github.com/$REPO/releases/latest

EOF
