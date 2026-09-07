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
The page checks `/api/health` when it loads.

Node and Aube are pinned in `mise.toml`; frontend dependency versions are pinned
in `ui/package.json` and `ui/aube-lock.yaml`. Aube uses its current upstream
`aubepkg/aube` repository explicitly because older mise registry entries still
reference its previous release identity.

```bash
mise run check
mise run build
FACTORY_HOST=127.0.0.1 FACTORY_PORT=8080 FACTORY_UI_DIR=ui/dist ./target/release/craftlions-factory
```

Stop `mise run dev` before running the last command, which serves the production
UI at `http://127.0.0.1:8080` directly from Rust.
Trillium serves assets and falls back to the UI for extensionless HTML navigation;
unknown API routes and missing assets return 404.

On Debian, the single systemd service serves port 80 and reads UI files from
`/usr/share/craftlions-factory/ui`. `FACTORY_HOST`, `FACTORY_PORT`, and
`FACTORY_UI_DIR` override those defaults. Node and Aube are build tools only;
the appliance does not need a JavaScript server. CI builds both parts, installs
the package, and checks its HTTP endpoints before publishing.

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
git tag v0.1.0
git push origin v0.1.0
```

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

Publish another tag:

```bash
git tag v0.1.1
git push origin v0.1.1
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
