//! Download and verify a release before changing either installed program.
//! The CLI is replaced last: self-replace handles its running Windows image.

use semver::Version;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const REPOSITORY: &str = "fpgames/multibox";
const LATEST: &str = "https://github.com/fpgames/multibox/releases/latest";
const CLI: &str = if cfg!(windows) {
    "git-multibox.exe"
} else {
    "git-multibox"
};
const UI: &str = if cfg!(windows) {
    "multibox-ui.exe"
} else {
    "multibox-ui"
};
const NOTICES: &str = "multibox-THIRD-PARTY-LICENSES.md";

#[derive(Default, Debug, PartialEq, Eq)]
struct Options {
    check: bool,
    force: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        for arg in args {
            match arg.as_str() {
                "--check" => options.check = true,
                "--force" => options.force = true,
                other => return Err(format!("unknown update option {other}")),
            }
        }
        if options.check && options.force {
            return Err("update --check and --force cannot be used together".to_owned());
        }
        Ok(options)
    }
}

struct Release {
    tag: String,
    version: Version,
}

impl Release {
    fn from_url(url: &str) -> Result<Self, String> {
        let prefix = format!("https://github.com/{REPOSITORY}/releases/tag/");
        let tag = url
            .trim()
            .strip_prefix(&prefix)
            .ok_or("GitHub did not redirect to a Multibox release")?;
        let version = tag
            .strip_prefix('v')
            .and_then(|tag| Version::parse(tag).ok())
            .filter(|version| version.pre.is_empty() && version.build.is_empty())
            .ok_or_else(|| format!("unsupported release tag {tag:?}"))?;
        Ok(Self {
            tag: tag.to_owned(),
            version,
        })
    }

    fn asset_url(&self, name: &str) -> String {
        // Construct the URL from the fixed repository and a parsed version;
        // the redirect never selects another download host or filename.
        format!(
            "https://github.com/{REPOSITORY}/releases/download/{}/{name}",
            self.tag
        )
    }
}

fn target() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        (os, arch) => Err(format!(
            "no prebuilt Multibox release for {os} {arch}; update from source with `cargo install --locked --git https://github.com/{REPOSITORY} git-multibox multibox-ui`"
        )),
    }
}

fn curl() -> Command {
    let mut command = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    command.args([
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "15",
        "--max-time",
        "120",
        "--retry",
        "2",
        "--user-agent",
        concat!("git-multibox/", env!("CARGO_PKG_VERSION")),
    ]);
    multibox_clone::no_console_window(&mut command);
    command
}

fn latest() -> Result<Release, String> {
    let output = curl()
        // The public redirect avoids GitHub's anonymous API rate limit and
        // never needs credentials. HEAD avoids downloading the release page.
        .args([
            "--head",
            "--output",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
            "--write-out",
            "%{url_effective}",
            LATEST,
        ])
        .output()
        .map_err(|err| format!("cannot run curl to check for updates: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot check GitHub for updates: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Release::from_url(&String::from_utf8_lossy(&output.stdout))
}

fn download(url: &str, file: &Path) -> Result<(), String> {
    let status = curl()
        .arg("--output")
        .arg(file)
        .arg(url)
        .status()
        .map_err(|err| format!("cannot run curl: {err}"))?;
    if !status.success() {
        return Err(format!("could not download {url}; nothing was installed"));
    }
    Ok(())
}

fn verify(archive: &Path, manifest: &str, name: &str) -> Result<(), String> {
    let mut expected = None;
    for line in manifest.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.get(1).copied() != Some(name) {
            continue;
        }
        if expected.is_some()
            || fields.len() != 2
            || fields[0].len() != 64
            || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!(
                "SHA256SUMS has an invalid or duplicate entry for {name}; nothing was installed"
            ));
        }
        expected = Some(fields[0].to_ascii_lowercase());
    }
    let expected = expected
        .ok_or_else(|| format!("SHA256SUMS has no entry for {name}; nothing was installed"))?;
    let mut file = File::open(archive).map_err(|err| err.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer).map_err(|err| err.to_string())?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(format!(
            "{name} does not match SHA256SUMS; nothing was installed"
        ));
    }
    Ok(())
}

fn unpack(archive: &Path, directory: &Path, package: &str) -> Result<PathBuf, String> {
    let mut command = Command::new(if cfg!(windows) { "tar.exe" } else { "tar" });
    command.arg("-xf").arg(archive).arg("-C").arg(directory);
    for name in [CLI, UI, "THIRD-PARTY-LICENSES.md"] {
        command.arg(format!("{package}/{name}"));
    }
    multibox_clone::no_console_window(&mut command);
    let status = command
        .status()
        .map_err(|err| format!("cannot run tar to unpack the update: {err}"))?;
    if !status.success() {
        return Err("cannot unpack the release; nothing was installed".to_owned());
    }
    let root = directory.join(package);
    for name in [CLI, UI, "THIRD-PARTY-LICENSES.md"] {
        if !fs::symlink_metadata(root.join(name))
            .is_ok_and(|metadata| metadata.file_type().is_file())
        {
            return Err(format!(
                "the release does not contain a regular {name}; nothing was installed"
            ));
        }
    }
    Ok(root)
}

/// The OS releases this lock on exit or a crash. Keep its directory entry:
/// unlinking it could let concurrent updaters lock different files.
fn lock_installation(directory: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let file = options.open(directory.join(".multibox-update.lock"))
        .map_err(|err| format!("cannot lock {} for updating (another update may be running, or this directory is not writable): {err}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "another update is running: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(file)
}

/// Side files move only after all of them were staged beside the installation.
/// If replacing the running CLI fails, put the previous UI and notices back.
fn install(
    package: &Path,
    executable: &Path,
    replace_cli: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), String> {
    let directory = executable
        .parent()
        .ok_or("cannot find the installation directory")?;
    let transaction = tempfile::Builder::new()
        .prefix(".multibox-install-")
        .tempdir_in(directory)
        .map_err(|err| format!("cannot write to {}: {err}", directory.display()))?;
    // On Windows, self-replace has to move the running image first. A later
    // failure must not leave its original filename missing.
    let cli_backup = transaction.path().join(format!("{CLI}.previous"));
    fs::copy(executable, &cli_backup)
        .map_err(|err| format!("cannot back up the running command: {err}"))?;
    let mut files = Vec::new();
    for (source, name) in [(UI, UI), ("THIRD-PARTY-LICENSES.md", NOTICES)] {
        let destination = directory.join(name);
        // A writable file open detects a running Windows UI before changing
        // any file; unlike writing the destination it never truncates it.
        if destination.exists() {
            OpenOptions::new()
                .write(true)
                .open(&destination)
                .map_err(|err| {
                    format!(
                        "cannot update {}; close the Multibox window and try again: {err}",
                        destination.display()
                    )
                })?;
        }
        let staged = transaction.path().join(name);
        fs::copy(package.join(source), &staged)
            .map_err(|err| format!("cannot stage {name}: {err}"))?;
        if destination.exists() {
            fs::set_permissions(
                &staged,
                fs::metadata(&destination)
                    .map_err(|err| err.to_string())?
                    .permissions(),
            )
            .map_err(|err| err.to_string())?;
        }
        files.push((
            staged,
            destination,
            transaction.path().join(format!("{name}.previous")),
        ));
    }
    let mut changed = Vec::new();
    let result = (|| -> io::Result<()> {
        for (staged, destination, backup) in &files {
            let existed = destination.try_exists()?;
            if existed {
                fs::rename(destination, backup)?;
            }
            changed.push((destination, backup, existed));
            fs::rename(staged, destination)?;
        }
        replace_cli(&package.join(CLI))
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        let cli_unchanged = fs::read(executable)
            .is_ok_and(|bytes| fs::read(&cli_backup).is_ok_and(|previous| bytes == previous));
        if !cli_unchanged && let Err(err) = fs::copy(&cli_backup, executable) {
            failures.push(format!("cannot restore the command: {err}"));
        }
        for (destination, backup, existed) in changed.into_iter().rev() {
            if let Err(err) = fs::remove_file(destination) {
                if err.kind() != io::ErrorKind::NotFound {
                    failures.push(err.to_string());
                }
            }
            if existed && let Err(err) = fs::rename(backup, destination) {
                failures.push(err.to_string());
            }
        }
        if !failures.is_empty() {
            let backup = transaction.keep();
            return Err(format!(
                "update failed: {error}; could not fully restore the previous files ({}); backups kept in {}",
                failures.join("; "),
                backup.display()
            ));
        }
        return Err(format!("update failed; previous files restored: {error}"));
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<ExitCode, String> {
    let options = Options::parse(args)?;
    let release = latest()?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|err| err.to_string())?;
    println!("Current: {current}; latest: {}", release.version);
    if release.version <= current && !options.force {
        println!("Multibox is up to date.");
        return Ok(ExitCode::SUCCESS);
    }
    if options.check {
        println!("Run `git multibox update` to install {}.", release.tag);
        return Ok(ExitCode::SUCCESS);
    }
    let target = target()?;
    let package = format!("git-multibox-{}-{target}", release.tag);
    let name = format!("{package}.{}", if cfg!(windows) { "zip" } else { "tar.gz" });
    let archive_url = release.asset_url(&name);
    let manifest_url = release.asset_url("SHA256SUMS");
    let executable = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|err| err.to_string())?;
    let directory = executable
        .parent()
        .ok_or("cannot find the installation directory")?;
    println!(
        "Updating {} in {} (command and window)",
        release.tag,
        directory.display()
    );
    let _lock = lock_installation(directory)?;
    let staging = tempfile::Builder::new()
        .prefix(".multibox-update-")
        .tempdir_in(directory)
        .map_err(|err| format!("cannot write to {}: {err}", directory.display()))?;
    let archive = staging.path().join(&name);
    let manifest = staging.path().join("SHA256SUMS");
    download(&manifest_url, &manifest)?;
    download(&archive_url, &archive)?;
    verify(
        &archive,
        &fs::read_to_string(&manifest).map_err(|err| err.to_string())?,
        &name,
    )?;
    let package = unpack(&archive, staging.path(), &package)?;
    install(&package, &executable, |cli| self_replace::self_replace(cli))?;
    println!(
        "Updated Multibox to {} in {}. Restart any open Multibox window to use the new version.",
        release.tag,
        directory.display()
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn stable_releases_only_download_from_the_multibox_repository() {
        let release =
            Release::from_url("https://github.com/fpgames/multibox/releases/tag/v0.10.0").unwrap();
        assert_eq!(release.version, Version::new(0, 10, 0));
        assert_eq!(
            release.asset_url("SHA256SUMS"),
            "https://github.com/fpgames/multibox/releases/download/v0.10.0/SHA256SUMS"
        );
    }

    #[test]
    fn rejects_invalid_tags_prereleases_and_redirects_outside_the_repository() {
        for tag in [
            "../../other",
            "v1.2.3/other",
            "v1.2.3-beta",
            "v1.2.3+build",
            "unknown",
        ] {
            assert!(
                Release::from_url(&format!(
                    "https://github.com/fpgames/multibox/releases/tag/{tag}"
                ))
                .is_err()
            );
        }
        for url in [
            "https://another.example/v1.2.3",
            "https://github.com/other/repository/releases/tag/v1.2.3",
            LATEST,
        ] {
            assert!(Release::from_url(url).is_err());
        }
    }

    #[test]
    fn checksums_reject_tampered_archives_and_missing_or_duplicate_entries() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("release.zip");
        fs::write(&file, b"release bytes").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"release bytes"));
        let line = format!("{hash}  release.zip\n");
        verify(&file, &line, "release.zip").unwrap();
        verify(
            &file,
            &line
                .to_ascii_uppercase()
                .replace("RELEASE.ZIP", "release.zip"),
            "release.zip",
        )
        .unwrap();
        for manifest in [
            String::new(),
            line.replace("release.zip", "other.zip"),
            format!("{line}{line}"),
            "invalid  release.zip\n".to_owned(),
        ] {
            assert!(verify(&file, &manifest, "release.zip").is_err());
        }
        fs::write(&file, b"tampered").unwrap();
        assert!(
            verify(&file, &line, "release.zip")
                .unwrap_err()
                .contains("does not match")
        );
    }

    fn installation() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let directory = temp.path().join("custom install & spaces");
        let package = temp.path().join("package");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(&package).unwrap();
        for (name, content) in [
            (CLI, "old command"),
            (UI, "old window"),
            (NOTICES, "old notices"),
            ("another-program", "keep me"),
        ] {
            fs::write(directory.join(name), content).unwrap();
        }
        for (name, content) in [
            (CLI, "new command"),
            (UI, "new window"),
            ("THIRD-PARTY-LICENSES.md", "new notices"),
        ] {
            fs::write(package.join(name), content).unwrap();
        }
        (temp, directory, package)
    }

    #[test]
    fn updates_both_programs_in_the_current_custom_directory() {
        let (_temp, directory, package) = installation();
        let executable = directory.join(CLI);
        install(&package, &executable, |source| {
            fs::copy(source, &executable).map(|_| ())
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&executable).unwrap(), "new command");
        assert_eq!(
            fs::read_to_string(directory.join(UI)).unwrap(),
            "new window"
        );
        assert_eq!(
            fs::read_to_string(directory.join(NOTICES)).unwrap(),
            "new notices"
        );
        assert_eq!(
            fs::read_to_string(directory.join("another-program")).unwrap(),
            "keep me"
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 4);
    }

    #[test]
    fn failed_self_replacement_restores_even_a_moved_command() {
        for remove_command in [false, true] {
            let (_temp, directory, package) = installation();
            let executable = directory.join(CLI);
            let err = install(&package, &executable, |_| {
                if remove_command {
                    fs::remove_file(&executable)?;
                }
                Err(io::Error::other("replacement failed"))
            })
            .unwrap_err();
            assert!(err.contains("previous files restored"), "{err}");
            assert_eq!(fs::read_to_string(&executable).unwrap(), "old command");
            assert_eq!(
                fs::read_to_string(directory.join(UI)).unwrap(),
                "old window"
            );
            assert_eq!(
                fs::read_to_string(directory.join(NOTICES)).unwrap(),
                "old notices"
            );
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 4);
        }
    }

    #[test]
    fn failed_install_does_not_leave_a_new_window_or_notices_behind() {
        let (_temp, directory, package) = installation();
        fs::remove_file(directory.join(UI)).unwrap();
        fs::remove_file(directory.join(NOTICES)).unwrap();
        assert!(
            install(&package, &directory.join(CLI), |_| Err(io::Error::other(
                "failed"
            )))
            .is_err()
        );
        assert!(!directory.join(UI).exists());
        assert!(!directory.join(NOTICES).exists());
        assert_eq!(
            fs::read_to_string(directory.join(CLI)).unwrap(),
            "old command"
        );
    }

    #[test]
    fn concurrent_updates_are_locked_out_and_exit_releases_the_lock() {
        let temp = TempDir::new().unwrap();
        let lock = lock_installation(temp.path()).unwrap();
        assert!(lock_installation(temp.path()).is_err());
        drop(lock);
        assert!(lock_installation(temp.path()).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn a_window_held_open_on_windows_prevents_any_replacement() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_temp, directory, package) = installation();
        let _window = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(directory.join(UI))
            .unwrap();
        let err = install(&package, &directory.join(CLI), |_| {
            panic!("CLI must not be replaced")
        })
        .unwrap_err();
        assert!(err.contains("close the Multibox window"), "{err}");
        assert_eq!(
            fs::read_to_string(directory.join(CLI)).unwrap(),
            "old command"
        );
        assert_eq!(
            fs::read_to_string(directory.join(NOTICES)).unwrap(),
            "old notices"
        );
    }

    #[test]
    fn unpacks_only_the_programs_and_their_notices() {
        let (_temp, directory, package) = installation();
        fs::write(package.join("unrelated"), "do not extract me").unwrap();
        let archive = directory.join(if cfg!(windows) {
            "release.zip"
        } else {
            "release.tar.gz"
        });
        let status = Command::new(if cfg!(windows) { "tar.exe" } else { "tar" })
            .arg("-a")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(package.parent().unwrap())
            .arg("package")
            .status()
            .unwrap();
        assert!(status.success());
        let unpacked = unpack(&archive, &directory, "package").unwrap();
        assert_eq!(
            fs::read_to_string(unpacked.join(CLI)).unwrap(),
            "new command"
        );
        assert_eq!(fs::read_to_string(unpacked.join(UI)).unwrap(), "new window");
        assert_eq!(
            fs::read_to_string(unpacked.join("THIRD-PARTY-LICENSES.md")).unwrap(),
            "new notices"
        );
        assert!(!unpacked.join("unrelated").exists());
    }
}
