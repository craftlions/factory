//! What every microvm boots from: the guest kernel and root filesystem the
//! Firecracker project publishes, and a small tools drive with mise and the
//! setup script. Downloaded and built once, then shared read-only.

use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{fs, process::Command, sync::mpsc};

const ARTIFACTS: &str = "https://s3.amazonaws.com/spec.ccfc.min";
/// The kernel series Firecracker's own tests boot.
const KERNEL_SERIES: &str = "vmlinux-6.1.";
const MISE_VERSION: &str = "v2026.9.12";
const TOOLS_DRIVE_SIZE: &str = "512M";

#[derive(Clone, Debug)]
pub struct Assets {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub tools: PathBuf,
}

/// Runs a tool to completion and turns a failure into a readable error.
pub async fn run(program: &str, args: &[&str]) -> io::Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| io::Error::new(error.kind(), format!("{program}: {error}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(io::Error::other(format!(
            "{program} failed ({}): {}",
            output.status,
            stderr.trim()
        )))
    }
}

async fn download(url: &str, target: &Path) -> io::Result<()> {
    let partial = target.with_extension("partial");
    let (url, partial_str) = (url.to_owned(), partial.to_string_lossy().into_owned());
    run("curl", &["-fsSL", "--retry", "3", "-o", &partial_str, &url]).await?;
    fs::rename(&partial, target).await
}

fn keys(listing: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    listing
        .split(&open)
        .skip(1)
        .filter_map(|rest| rest.split_once(&close).map(|(key, _)| key.to_owned()))
        .collect()
}

/// Version-aware order, so 6.1.100 sorts after 6.1.99.
fn version_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// Picks the kernel and root filesystem out of one build's file listing.
fn pick(listing: &str, arch: &str) -> Option<(String, String)> {
    let files = keys(listing, "Key");
    let in_arch = |key: &&String| key.contains(&format!("/{arch}/")) && !key.contains("/debug/");
    let kernel = files
        .iter()
        .filter(in_arch)
        .filter(|key| {
            key.rsplit('/').next().is_some_and(|name| {
                name.strip_prefix(KERNEL_SERIES)
                    .is_some_and(|patch| patch.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .max_by_key(|key| version_key(key))?;
    let rootfs = files
        .iter()
        .filter(in_arch)
        .filter(|key| {
            key.rsplit('/')
                .next()
                .is_some_and(|name| name.starts_with("ubuntu-") && name.ends_with(".squashfs"))
        })
        .max_by_key(|key| version_key(key))?;
    Some((kernel.clone(), rootfs.clone()))
}

/// The newest published build that has both files for this architecture.
async fn latest_guest_files(arch: &str) -> io::Result<(String, String)> {
    let builds = run(
        "curl",
        &[
            "-fsSL",
            &format!("{ARTIFACTS}?list-type=2&prefix=firecracker-ci/&delimiter=/"),
        ],
    )
    .await?;
    let mut prefixes = keys(&builds, "Prefix");
    prefixes.retain(|prefix| {
        prefix
            .trim_start_matches("firecracker-ci/")
            .starts_with(|c: char| c.is_ascii_digit())
    });
    prefixes.sort();
    for prefix in prefixes.iter().rev().take(5) {
        let listing = run(
            "curl",
            &[
                "-fsSL",
                &format!("{ARTIFACTS}?list-type=2&prefix={prefix}{arch}/"),
            ],
        )
        .await?;
        if let Some(found) = pick(&listing, arch) {
            return Ok(found);
        }
    }
    Err(io::Error::other(
        "no published Firecracker guest kernel and root filesystem found",
    ))
}

async fn sha256(path: &Path) -> io::Result<String> {
    let out = run("sha256sum", &[&path.to_string_lossy()]).await?;
    Ok(out.split_whitespace().next().unwrap_or_default().to_owned())
}

async fn download_mise(target: &Path, arch: &str) -> io::Result<()> {
    let arch = if arch == "aarch64" { "arm64" } else { "x64" };
    let name = format!("mise-{MISE_VERSION}-linux-{arch}-musl");
    let release = format!("https://github.com/jdx/mise/releases/download/{MISE_VERSION}");
    download(&format!("{release}/{name}"), target).await?;
    let sums = run("curl", &["-fsSL", &format!("{release}/SHASUMS256.txt")]).await?;
    let expected = sums
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, file)| file.trim().trim_start_matches(['*', '.', '/']) == name)
        .map(|(sum, _)| sum.to_owned());
    if expected != Some(sha256(target).await?) {
        let _ = fs::remove_file(target).await;
        return Err(io::Error::other(format!(
            "{name} does not match its published checksum"
        )));
    }
    Ok(())
}

/// The tools drive is rebuilt whenever what goes into it changes.
async fn tools_stamp(guest_dir: &Path) -> io::Result<String> {
    let mut stamp = format!("mise {MISE_VERSION}\n");
    for name in ["startup.sh", "mise.toml"] {
        stamp.push_str(&fs::read_to_string(guest_dir.join(name)).await?);
    }
    Ok(stamp)
}

async fn build_tools_drive(
    dir: &Path,
    guest_dir: &Path,
    mise: &Path,
    drive: &Path,
) -> io::Result<()> {
    let staging = dir.join("tools-staging");
    let _ = fs::remove_dir_all(&staging).await;
    fs::create_dir_all(staging.join("bin")).await?;
    fs::copy(mise, staging.join("bin/mise")).await?;
    for name in ["startup.sh", "mise.toml"] {
        fs::copy(guest_dir.join(name), staging.join(name)).await?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            staging.join("bin/mise"),
            std::fs::Permissions::from_mode(0o755),
        )
        .await?;
    }
    let _ = fs::remove_file(drive).await;
    run(
        "mkfs.ext4",
        &[
            "-q",
            "-F",
            "-d",
            &staging.to_string_lossy(),
            &drive.to_string_lossy(),
            TOOLS_DRIVE_SIZE,
        ],
    )
    .await?;
    fs::remove_dir_all(&staging).await
}

/// Makes sure everything is on disk, telling `progress` about the slow parts.
pub async fn ensure(
    dir: &Path,
    guest_dir: &Path,
    progress: &mpsc::UnboundedSender<String>,
) -> io::Result<Assets> {
    fs::create_dir_all(dir).await?;
    let arch = std::env::consts::ARCH;
    let assets = Assets {
        kernel: dir.join("vmlinux"),
        rootfs: dir.join("rootfs.squashfs"),
        tools: dir.join("tools.ext4"),
    };

    if !assets.kernel.exists() || !assets.rootfs.exists() {
        let _ = progress
            .send("Downloading the guest kernel and root filesystem (about 120 MB, once).".into());
        let (kernel, rootfs) = latest_guest_files(arch).await?;
        download(&format!("{ARTIFACTS}/{kernel}"), &assets.kernel).await?;
        download(&format!("{ARTIFACTS}/{rootfs}"), &assets.rootfs).await?;
    }

    let mise = dir.join(format!("mise-{MISE_VERSION}"));
    if !mise.exists() {
        let _ = progress.send("Downloading mise for the guest (once).".into());
        download_mise(&mise, arch).await?;
    }

    let stamp = tools_stamp(guest_dir).await?;
    let stamp_file = dir.join("tools.stamp");
    if !assets.tools.exists() || fs::read_to_string(&stamp_file).await.ok().as_ref() != Some(&stamp)
    {
        build_tools_drive(dir, guest_dir, &mise, &assets.tools).await?;
        fs::write(&stamp_file, &stamp).await?;
    }
    Ok(assets)
}

/// Puts placeholder assets in place, so tests neither download nor build.
#[cfg(test)]
pub fn prefill_for_tests(dir: &Path, guest_dir: &Path) {
    for name in [
        "vmlinux",
        "rootfs.squashfs",
        "tools.ext4",
        &format!("mise-{MISE_VERSION}"),
    ] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    let mut stamp = format!("mise {MISE_VERSION}\n");
    for name in ["startup.sh", "mise.toml"] {
        stamp.push_str(&std::fs::read_to_string(guest_dir.join(name)).unwrap());
    }
    std::fs::write(dir.join("tools.stamp"), stamp).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "<Contents><Key>firecracker-ci/20260916-x/x86_64/debug/vmlinux-6.1.186</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/x86_64/vmlinux-5.10.268</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/x86_64/vmlinux-6.1.99</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/x86_64/vmlinux-6.1.186</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/x86_64/vmlinux-6.1.186.config</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/x86_64/vmlinux-6.18.48</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/aarch64/ubuntu-26.04.squashfs</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/x86_64/ubuntu-24.04.manifest</Key></Contents>\
        <Contents><Key>firecracker-ci/20260916-x/x86_64/ubuntu-24.04.squashfs</Key></Contents>";

    #[test]
    fn picks_the_newest_kernel_of_the_series_and_the_rootfs_for_the_arch() {
        let (kernel, rootfs) = pick(LISTING, "x86_64").unwrap();
        assert_eq!(kernel, "firecracker-ci/20260916-x/x86_64/vmlinux-6.1.186");
        assert_eq!(
            rootfs,
            "firecracker-ci/20260916-x/x86_64/ubuntu-24.04.squashfs"
        );
        assert!(pick(LISTING, "riscv64").is_none());
    }
}
