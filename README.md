# Minimal Rust appliance with an APT repository on GitHub Pages

This repo builds a statically linked Rust/Trillium appliance, packages it as a `.deb`, creates signed APT metadata, and deploys the repository to GitHub Pages whenever a `v*` tag is pushed.

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

Visit `http://DEVICE_IP:8080/`.

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

APT upgrades `appliance` and the package's `postinst` restarts `appliance.service`.

## Notes

- This deliberately publishes only the newest `.deb`; it is enough for a minimal upgrade path.
- The workflow targets amd64. For ARM64, switch the Rust target and Debian Architecture.
- For production, provision the APT public key into your appliance image instead of fetching it during first setup.
