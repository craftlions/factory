# Rust appliance with a React UI and an APT repository

This repo builds a statically linked Rust/Trillium appliance and a Vite/React/TypeScript UI, packages them as a `.deb`, creates signed APT metadata, and deploys the repository to GitHub Pages whenever a `v*` tag is pushed.

## Development

Install [mise](https://mise.jdx.dev/), then run:

```bash
mise install
mise run ui:install
mise run dev
```

Open `http://127.0.0.1:5173` (Vite's default port). Vite refreshes the UI on edits
and proxies `/api` to Rust on port 8081. Use port 5173 for both the UI and API
during development. `mise watch`, powered by watchexec, rebuilds and restarts
Rust when `src/`, `Cargo.toml`, or `Cargo.lock` changes. Frontend edits do not
restart Rust. `mise run server` starts Rust once without watching.
The UI loads `/api/overview`, `/api/samples`, and `/api/sessions`, then
subscribes to `/api/events` (server-sent events) for a new sample every five
seconds.

Node and Aube are pinned in `mise.toml`; frontend dependency versions are pinned
in `ui/package.json` and `ui/aube-lock.yaml`. Aube uses its current upstream
`aubepkg/aube` repository explicitly because older mise registry entries still
reference its previous release identity.

`cargo test` runs the unit tests. `cargo test -- --ignored` also runs the pi
adapter test, which needs pi installed and sends no prompt.

On a stop signal the server ends its event streams first, so an open browser
tab cannot keep a stopping process alive and holding the port during
`mise run dev` restarts.

```bash
mise run check
mise run build
FACTORY_HOST=127.0.0.1 FACTORY_PORT=8080 FACTORY_UI_DIR=ui/dist ./target/release/craftlions-factory
```

Stop `mise run dev` before running the last command, which serves the production
UI at `http://127.0.0.1:8080` directly from Rust.
Trillium serves assets and falls back to the UI for extensionless HTML navigation;
unknown API routes and missing assets return 404.

On Debian, the single systemd service serves port 80, reads UI files from
`/usr/share/craftlions-factory/ui`, and keeps its SQLite database in
`/var/lib/craftlions-factory` (a systemd `StateDirectory`). `FACTORY_HOST`,
`FACTORY_PORT`, `FACTORY_UI_DIR`, and `FACTORY_DATA_DIR` override those
defaults; the dev tasks use `./data`, which is gitignored. Node and Aube are build tools only;
the appliance does not need a JavaScript server. CI builds both parts, installs
the package, and checks its HTTP endpoints before publishing.

## Dashboard

Rust samples the host every five seconds with `sysinfo`: CPU, one-minute load,
memory, the disk holding the data directory, the service's own CPU and resident
memory, and the number and size of files in the data directory. Samples are
stored in SQLite through `sqlx` with embedded migrations from `migrations/` and
pruned after seven days.

Sessions are the unit of work. A session is one harness process started and
controlled by the factory. The factory only offers isolated sessions: running
a harness directly on the host was removed, because harnesses execute shell
commands without asking. The microvm runner is not implemented yet, so
`POST /api/sessions` currently refuses every request, and sessions recorded
earlier without isolation can be read but not restarted. Any session still
open at startup is marked `interrupted`. Restarting one starts the harness
again in the same workspace and continues the conversation from the harness's
own session file.

Session ids are six random characters from `6789bcdfghjkmnpqrtwx`: lowercase,
no look-alike characters, no vowels. Rows from before that keep their number.
After launch the harness reports its own session id and session file, which
are stored with the session and used to read the transcript and to restart.

Each session owns `sessions/<id>/` under the data directory: `workspace/` is
the harness's working directory and is kept after the session ends, and
`harness/` holds the session file the harness writes itself.

`src/harness/mod.rs` defines a harness-agnostic chat model (`ChatItem`,
`ChatEvent`) and two traits. `Harness` launches and controls a process.
`TranscriptReader` parses that harness's own session file back into chat
items, which is how ended sessions are shown. There is no transcript in
SQLite. While a session runs, finished messages are kept in memory so a
client that connects late gets a consistent snapshot plus the live stream.

The pi adapter (`src/harness/pi.rs`) spawns `pi --mode rpc` and speaks its
JSON-lines protocol over stdio. pi runs as the factory's user and uses that
user's `~/.pi` configuration and credentials. `FACTORY_PI_BIN` overrides the
binary, which defaults to `pi` on `PATH`. Providers, models and per-model
reasoning levels are asked from pi on demand, never hardcoded. Extension dialogs are declined automatically because
the chat has no dialog UI yet.

The UI has a vertical navigation on the left, which collapses to a row on
narrow screens, and these pages routed with the History API in
`ui/src/router.tsx`:

| Path | Page |
| --- | --- |
| `/` | Overview: session counts |
| `/sessions` | Fifty most recent sessions |
| `/sessions/new` | Questionnaire that creates a session |
| `/sessions/<id>` | Chat with a running session, or the transcript of an ended one |
| `/host` | Host, service, and data directory stats with one hour of history |

| Endpoint | Purpose |
| --- | --- |
| `GET /api/health` | Liveness, `{"status":"ok"}` |
| `GET /api/overview` | Host facts, latest sample, session counts |
| `GET /api/samples?window=SECONDS` | Samples from the last window, default one hour |
| `GET /api/sessions` | Fifty most recent sessions |
| `POST /api/sessions` | Create a session; 501 for combinations not implemented yet, 422 for no isolation |
| `GET /api/sessions/<id>` | One session |
| `GET /api/sessions/<id>/events` | SSE stream of `chat` events: one `snapshot`, then live updates while it runs |
| `POST /api/sessions/<id>/prompt` | Send a message; it steers the agent if it is mid-run |
| `POST /api/sessions/<id>/abort` | Interrupt the current run |
| `POST /api/sessions/<id>/stop` | End the session and its harness process |
| `POST /api/sessions/<id>/resume` | Restart an ended session and continue its conversation; 409 if it is running |
| `GET /api/harnesses/<id>/models` | Models the harness reports for this host |
| `GET /api/harnesses/<id>/reasoning?provider=&model=` | Reasoning levels the harness reports for one model |
| `GET /api/events` | SSE stream, `sample` events; other `Accept` values get 406 |

POST bodies must be `application/json`. That forces a CORS preflight the
server never approves, so other websites cannot start or drive sessions.

## 1. Generate a dedicated APT signing key

On your development machine:

```bash
gpg --quick-generate-key "craftlions Factory APT 2026 <alexander@craftlions.com>" rsa4096 sign 2y
gpg --armor --export-secret-keys "craftlions Factory APT 2026 <alexander@craftlions.com>" > apt-signing-private.asc
```

If you already generated this key, reuse it and run only the export command.
The exported file must be nonempty and start with `-----BEGIN PGP PRIVATE KEY BLOCK-----`.

From this repository, upload the file directly using the [GitHub CLI](https://cli.github.com/manual/gh_secret_set):

```bash
test -s apt-signing-private.asc && gh secret set APT_SIGNING_KEY < apt-signing-private.asc
gh secret set APT_GPG_PASSPHRASE
```

The second command prompts for the key passphrase. These are GitHub Actions
repository **secrets**, not variables. Do not paste the filename or base64-encode
the key. If the `github-pages` environment also has these secrets, remove stale
overrides or update them with `gh secret set --env github-pages ...`.

Do not commit the private key. The workflow checks the key and passphrase before
building. `no valid OpenPGP data found` means the import received empty or invalid
key data; changing the passphrase will not fix that import error.

## 2. Enable GitHub Pages

Repository Settings -> Pages -> Build and deployment -> Source -> GitHub Actions.

## 3. Publish version 0.1.0

```bash
mise run release 0.1.0
```

This sets the version in `Cargo.toml`, `Cargo.lock`, and `ui/package.json`,
commits `release v0.1.0`, tags it, and pushes. The workflow refuses tags whose
version differs from `Cargo.toml`.

The APT repository will be available at:

```text
https://craftlions.github.io/factory/
```

## 4. Add the repository on Debian

```bash
sudo install -d -m 0755 /etc/apt/keyrings
sudo curl -fsSL \
  https://craftlions.github.io/factory/craftlions-factory-archive-keyring.asc \
  -o /etc/apt/keyrings/craftlions-factory-archive-keyring.asc

sudo tee /etc/apt/sources.list.d/craftlions-factory.sources >/dev/null <<'EOF2'
Types: deb
URIs: https://craftlions.github.io/factory/
Suites: stable
Components: main
Architectures: amd64
Signed-By: /etc/apt/keyrings/craftlions-factory-archive-keyring.asc
EOF2

sudo apt update
sudo apt install craftlions-factory
```

Visit `http://DEVICE_IP/` (port 80).

## 5. Upgrade

Publish another version:

```bash
mise run release 0.1.1
```

Then on the appliance:

```bash
sudo apt update
sudo apt upgrade
```

APT upgrades `craftlions-factory` and the package's `postinst` restarts `craftlions-factory.service`.

## Notes

- This deliberately publishes only the newest `.deb`; it is enough for a minimal upgrade path.
- The workflow targets amd64. For ARM64, switch the Rust target and Debian Architecture.
- For production, provision the APT public key into your appliance image instead of fetching it during first setup.
