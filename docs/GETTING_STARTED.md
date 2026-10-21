# Getting started

This guide takes a fresh machine to Kiln opening its first pull request.
It works the same on a Mac (Apple Silicon) and on Linux (x86-64 or arm64).
A Mac mini that stays on is the intended home, but any always-on box works.

## 1. What you need

| | |
|---|---|
| **Docker** | Docker Desktop, OrbStack, Podman or Colima. Workers run in containers. |
| **git** | On `PATH`. |
| **A model key** | An Anthropic key, an OpenAI key, a Claude subscription token or a LiteLLM proxy. Each user adds their own. |
| **GitHub access** | A token (`gh auth login` is enough) or a GitHub App. |
| **Tailscale** (recommended) | Lets your laptop and phone reach the daemon privately. Not needed if you only use it on the same machine. |

Hardware: about 4 GB of RAM per concurrent worker container, plus 6 GB kept
for the system. A 32 GB machine runs about 8 containers.

## 2. Install

### From a release (recommended)

Download the archive for your platform from
[Releases](https://github.com/chhhee10/kiln/releases) and unpack it:

```sh
tar xzf kiln-v0.1.1-aarch64-apple-darwin.tar.gz   # or x86_64/aarch64-unknown-linux-gnu
sudo cp kiln-v0.1.1-*/kilnd kiln-v0.1.1-*/kiln kiln-v0.1.1-*/kiln-runner /usr/local/bin/
mkdir -p ~/.kiln && cp -r kiln-v0.1.1-*/web ~/.kiln/web
```

The worker image (`ghcr.io/chhhee10/kiln-worker`, amd64 and arm64) is pulled
automatically the first time a task runs. To pull it ahead of time:
`docker pull ghcr.io/chhhee10/kiln-worker:0.1.1`.

### From source

```sh
git clone https://github.com/chhhee10/kiln && cd kiln
rustup target add wasm32-unknown-unknown && cargo install trunk
(cd app && trunk build --release)
cargo build --release -p kiln-daemon -p kiln-tui -p kiln-runner
mkdir -p ~/.kiln && cp -r app/dist ~/.kiln/web
docker build -f images/worker/Dockerfile -t ghcr.io/chhhee10/kiln-worker:0.1.1 .
```

Binaries land in `target/release/` (`kilnd`, `kiln`, `kiln-runner`).

## 3. Configure

Kiln keeps everything (database, repo mirrors, worktrees, run logs, memory) in
one data directory. Keep its path short: run sockets live inside it and Unix
socket paths are limited to about 100 bytes. `~/.kiln` is a good choice.

```sh
export KILN_DATA_DIR=~/.kiln
cp config/kiln.example.toml ~/.kiln/kiln.toml   # from the repo or the release archive
```

Edit `~/.kiln/kiln.toml`. The example file documents every key; the ones
that matter on day one:

```toml
data_dir = "~/.kiln"
web_dir = "~/.kiln/web"

[server]
bind = "tailscale"            # or "127.0.0.1" for this machine only
# public_url = "https://kiln-host.your-tailnet.ts.net"   # see step 8

[github]
mode = "token"                # uses $GITHUB_TOKEN, else `gh auth token`
```

**Docker socket.** Kiln finds Docker Desktop, OrbStack and Podman sockets on
its own. For Colima or anything else, set `runtime.docker_socket` (or
`DOCKER_HOST`).

**GitHub.** Pick one:

- `mode = "token"`: a fine-grained personal access token with *Contents:
  read and write* and *Pull requests: read and write* on your repos, in
  `GITHUB_TOKEN`; or just run `gh auth login` as the user that runs `kilnd`.
- `mode = "app"`: create a GitHub App (Settings → Developer settings →
  GitHub Apps) with repository permissions **Contents: read and write**,
  **Pull requests: read and write** and **Metadata: read**. No webhooks are
  needed (Kiln polls). Install it on your repos, download a private key and
  set:

  ```toml
  [github]
  mode = "app"
  app_id = 123456
  private_key_path = "~/.kiln/github-app.pem"
  ```

Now check the machine:

```sh
kilnd doctor
```

It checks the config, data directory, socket path length, disk, master key,
Tailscale, Docker and the worker image, git, GitHub auth and the web app.
Fix anything marked `fail` before going on.

## 4. Start and create the admin

```sh
kilnd serve
```

Open the app in a browser at `http://<bind address>:7420/app/` (with
`bind = "127.0.0.1"`: <http://127.0.0.1:7420/app/>). The first visit shows
**Setup**:

- the admin's name, email and password (10+ characters);
- the **allowed domain**: anyone with an email on it can create their own
  account from the sign-in page. Admins can create accounts for anyone else
  (Admin → Create account);
- optionally, a first project.

The terminal client works too: `kiln --server http://127.0.0.1:7420`.

## 5. Add your model key

Each user adds their own credentials under **Settings → Add credential**.
Keys are encrypted in the vault (master key in the OS keychain, else
`<data_dir>/master.key`) and are never shown again.

| Type | Used by |
|---|---|
| Anthropic API key | Claude Agent SDK |
| Claude subscription (OAuth token) | Claude Agent SDK (check your provider's terms for automated use) |
| OpenAI API key | Codex |
| LiteLLM proxy (base URL + key) | Both: Claude via its `/v1/messages`, Codex via `/v1/responses` |

Then pick a harness and model per role under **Settings → Worker defaults**
(Planner, Builder, QA, Reviewer, Distiller). Model names are free text, so
any model your provider or proxy serves works.

Codex reports tokens but not cost; Kiln prices those runs from LiteLLM's
`/model/info`, or from `[prices]` in `kiln.toml`.

## 6. Add a project

**Admin → Add project**: a name, a short key (`WEB`, used in task IDs like
`WEB-12`), the repository URL (`https://…` or `git@github.com:…`) and its
default branch.

The repository needs a `qa.yaml` at its root telling QA how to boot and
test it:

```yaml
version: 1
resources: { cpus: 2, memory: 2g }
extras:                       # added on top of the worker image, built once
  apt: [postgresql-client]
services:                     # sidecars, reachable by name (here: db:5432)
  db:
    image: postgres:16-alpine
    env: { POSTGRES_PASSWORD: kiln }
    ports: [5432]
    healthcheck: "pg_isready -U postgres -h 127.0.0.1"
boot:
  - "npm ci"
  - "npm start &"
checks:
  unit: "npm test"
  e2e: "npx playwright test"
artifacts: ["playwright-report/**"]
loop_cap: 3                   # Builder ↔ QA rounds before a human is asked
```

QA waits for every service's `healthcheck` to pass (up to 2 minutes), runs
`boot`, then the `checks`, and attaches the `artifacts` to the pull request.

## 7. Ship something

1. **Planner**: describe the feature. The Planner reads the repo, asks
   questions and proposes tasks.
2. **Approve** the plan. Each task runs Builder → QA → Reviewer in its own
   containers; dependent tasks stack their branches.
3. **Runs** shows live logs, QA evidence and the review. When a task is
   ready, its pull request is open on GitHub with the evidence linked.
4. Merge it on GitHub; Kiln notices and marks the task merged. After every
   finished task the Distiller records what it learned in the company brain.

## 8. Run it as a service

```sh
kilnd install          # launchd agent on macOS, systemd user unit on Linux
```

It prints the next steps. The service only receives `KILN_DATA_DIR` and a
standard `PATH`, so put settings in `kiln.toml` rather than the environment,
and run `gh auth login` as the service's user if you use token mode.

- **macOS**: `launchctl load ~/Library/LaunchAgents/dev.kiln.kilnd.plist`.
  Logs: `<data_dir>/logs/`.
- **Linux**: `systemctl --user enable --now kilnd` and
  `loginctl enable-linger $USER` so it runs while you are logged out.

### Mac mini checklist

1. `sudo pmset -a sleep 0 autorestart 1` so it never sleeps and restarts after
   a power cut.
2. Create a dedicated non-admin macOS user for Kiln and do everything below as
   that user.
3. Install OrbStack (or Docker Desktop) and Tailscale; `tailscale up`.
4. Steps 2 to 5 above, with `bind = "tailscale"`.
5. HTTPS for your devices:
   `tailscale serve --bg --https=443 http://$(tailscale ip -4):7420`, then set
   `server.public_url = "https://<host>.<tailnet>.ts.net"` and restart.
   Pairing QR codes and PR links use this URL.
6. `kilnd install`, load it, and confirm `kilnd doctor` is clean.

## 9. Clients

- **Browser**: `<public_url>/app/`.
- **Terminal**: `kiln --server <public_url>` (`kiln login` once).
- **Desktop app**: download from
  [Releases](https://github.com/chhhee10/kiln/releases): `.dmg` for Apple
  Silicon Macs, AppImage, `.deb` or `.rpm` for Linux. The builds aren't
  code-signed yet: on macOS, right-click the app → **Open** the first time
  (or `xattr -dr com.apple.quarantine /Applications/Kiln.app`). Sign in with
  your server's URL. To build it yourself: `cargo tauri build` in `app/`.
- **For teammates**: copy the app files into `<data_dir>/downloads/<version>/`
  (e.g. `~/.kiln/downloads/0.1.1/`) and they appear at `<public_url>/download`.
- **Android**: download `kiln-android-*-arm64.apk` from Releases and install
  it (allow installs from your browser or file manager). Pair the phone from
  **Settings → Devices → Pair a phone** by scanning the QR, or sign in with
  the server URL. Push notifications need a UnifiedPush distributor such as
  the [ntfy](https://ntfy.sh) app (optionally your own server:
  `[push] ntfy_url`). Building it yourself:
  [`app/src-tauri/ANDROID.md`](../app/src-tauri/ANDROID.md).

## Troubleshooting

- **`kilnd` refuses to start: no Tailscale address.** Run `tailscale up`, or
  set `server.bind = "127.0.0.1"`. Kiln never listens on `0.0.0.0`.
- **Socket path too long.** Move `data_dir` somewhere shorter (`~/.kiln`).
- **Image not found.** `docker pull ghcr.io/chhhee10/kiln-worker:0.1.1`, or
  set `runtime.worker_image` to an image you built.
- **A run fails right away.** Open it in Runs: the error includes the last
  lines of the container's output. Missing credentials show up here.
- **Forgot the admin password.** On the host: `kilnd admin reset-password <email>`.
- **Trying it without Docker or keys.** `kilnd serve --dev --demo-seed` runs a
  demo company with mock agents (see the README).
