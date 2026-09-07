# Minimal Rust appliance with an APT repository on GitHub Pages

This repo builds a statically linked Rust/Trillium appliance, packages it as a `.deb`, creates signed APT metadata, and deploys the repository to GitHub Pages whenever a `v*` tag is pushed.

## 1. Generate a dedicated APT signing key

On your development machine:

```bash
gpg --quick-generate-key "craftlions Factory APT <alexander@craftlions.com>" rsa4096 sign 2y
gpg --armor --export-secret-keys "craftlions Factory APT <alexander@craftlions.com>" > apt-signing-private.asc
```

Add these GitHub Actions repository secrets:

- `APT_SIGNING_KEY`: the complete contents of `apt-signing-private.asc`
- `APT_GPG_PASSPHRASE`: the key passphrase

Do not commit the private key.

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
