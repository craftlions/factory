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

`cargo test --workspace` runs the tests. They include the microvm runner
working against the real guest agent, with a stand-in for Firecracker that
starts the agent as a plain process. `cargo test -- --ignored` adds tests that
need pi installed; one of them sends a tiny prompt.

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
the appliance does not need a JavaScript server. CI builds both parts and
publishes the package without installing or testing it.

## Dashboard

Rust samples the host every five seconds with `sysinfo`: CPU, one-minute load,
memory, the disk holding the data directory, the service's own CPU and resident
memory, and the number and size of files in the data directory. Samples are
stored in SQLite through `sqlx` with embedded migrations from `migrations/` and
pruned after seven days.

Sessions are the unit of work. A session is one harness process that the
factory starts and controls inside its own Firecracker microvm. Harnesses
execute shell commands without asking, so they never run directly on the
host; sessions recorded earlier without isolation can be read but not
restarted. So far one combination can be created: the pi harness, in a
microvm, with a new empty directory. Any session still open at startup is
marked `interrupted`. Restarting one boots a new microvm on the same disk and
continues the conversation from the harness's own session file.

Session ids are six random characters from `6789bcdfghjkmnpqrtwx`: lowercase,
no look-alike characters, no vowels. Rows from before that keep their number.
After launch the harness reports its own session id and session file, which
are stored with the session and used to read the transcript and to restart.

`src/harness/mod.rs` defines a harness-agnostic chat model (`ChatItem`,
`ChatEvent`) and two traits. `Harness` says how to start a harness and speaks
its protocol over a `Process`, a pair of pipes that the isolated environment
hands over; an adapter never starts a process itself. `TranscriptReader`
parses that harness's own session file back into chat items, which is how
ended sessions are shown. There is no transcript in SQLite. While a session
runs, finished messages are kept in memory so a client that connects late gets
a consistent snapshot plus the live stream.

The pi adapter (`src/harness/pi.rs`) speaks the JSON-lines protocol of
`pi --mode rpc`. Providers, models and per-model reasoning levels are asked
from pi on demand, in a small catalog microvm, never hardcoded. Extension
dialogs are declined automatically because the chat has no dialog UI yet.

## Session microvms

`src/microvm/` runs a harness in Firecracker. Nothing in it needs root: the
service only needs access to `/dev/kvm`, which the systemd unit gets through
the `kvm` group.

- **Boot.** The guest kernel and the minimal Ubuntu root filesystem are the
  ones the Firecracker project publishes. They are downloaded on first use to
  `microvm/` under the data directory. The root filesystem is booted
  read-only and unmodified, shared by all sessions.
- **Guest agent.** `guest/` is a small static binary that is the only file in
  the initramfs and therefore the guest's first process. It layers the
  session's own disk over the root filesystem, runs the setup script, then the
  harness, and shuts the machine down when the harness ends.
- **Disk.** Each session has a sparse `disk.ext4` in `sessions/<id>/`. It
  holds everything the session installs or writes, the workspace, and the
  harness's session file. When a session ends, `harness/` and `workspace/` are
  copied out of the disk with `debugfs`, next to it.
- **Tools.** `packaging/guest/startup.sh` runs at every session start and
  installs the tools in `packaging/guest/mise.toml` with mise. A restarted
  session finds them on its disk already.
- **Network.** The guest has no network device. The agent forwards a loopback
  port over vsock to a proxy in the factory, which only opens TLS connections
  to the API host of the session's model, taken from pi's own model list, and
  to the three GitHub hosts mise needs. Addresses in private ranges are
  refused even for allowed names. Every refusal shows up in the chat.
- **Credentials.** pi's `auth.json`, and `models.json` and `settings.json` if
  present, are read from `pi/agent/` under the data directory and copied into
  each guest. Code running in the guest can read them. OAuth tokens that pi
  refreshes inside a guest are not written back.
- **Limits.** Firecracker runs without its jailer, which needs root. The
  published guest files carry no checksums; mise and Firecracker are verified.
  mise asks the GitHub API at every start, which allows 60 anonymous requests
  an hour per address.

`FACTORY_FIRECRACKER_BIN`, `FACTORY_GUEST_INITRD` and `FACTORY_GUEST_DIR`
override where the monitor, the initramfs and the guest files are found; the
package installs them under `/usr/lib/craftlions-factory`.

To try it on a Debian host with KVM, install the package, then give pi its
credentials and create a session in the UI:

```bash
sudo install -d -o craftlions-factory -g craftlions-factory -m 0700 \
  /var/lib/craftlions-factory/pi /var/lib/craftlions-factory/pi/agent
sudo install -o craftlions-factory -g craftlions-factory -m 0600 \
  ~/.pi/agent/auth.json /var/lib/craftlions-factory/pi/agent/auth.json
```

The first model list takes a few minutes: it downloads the guest files and
installs pi in the catalog microvm. If a microvm does not come up, its console
is in `sessions/<id>/vm/console.log`, and the end of it is shown in the chat.

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
