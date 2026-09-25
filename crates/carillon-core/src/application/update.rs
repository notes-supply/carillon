//! Replacing an installation's application from a local release archive.
//!
//! The archive is unpacked and checked while the services keep running:
//! extraction admits only regular files and directories, the release's own
//! `carillon` must report the archive's version, and the release's
//! `install.sh --update --check` verifies its payload against the packager's
//! manifest and lets the release's programs validate the installed registry,
//! core database and Discord settings read-only. Only then are services
//! stopped and `install.sh --update` performs the existing application
//! exchange and service registration. Application files, user units and
//! systemd are not one transaction, so failures report what may have changed
//! and how to finish, instead of rolling back.

use std::{
    fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::{Command as Process, ExitStatus},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{
    config::LoadedConfig, home::is_managed_application, maintenance::MaintenanceLock,
};

use super::{
    admin::unpack_regular,
    operations::{Action, Activity, Registration, Services, print_health, wait_until_healthy},
};

pub(super) async fn update(home: &Path, archive: &Path, timeout: Duration) -> Result<()> {
    let config = LoadedConfig::load(&home.join("carillon.toml"))
        .with_context(|| format!("{} is not a Carillon installation", home.display()))?;
    // The release scripts name the installation by its canonical path.
    let home = &home.canonicalize()?;
    let owner = rustix::process::getuid();
    if owner.is_root() {
        bail!("update must run as the installation's non-root owner, not root");
    }
    if fs::metadata(home)?.uid() != owner.as_raw() {
        bail!("{} is not owned by the invoking user", home.display());
    }
    let app = home.join("app");
    if app.symlink_metadata().is_ok() && !is_managed_application(&app) {
        bail!(
            "refusing to replace unowned application path {}",
            app.display()
        );
    }
    let _lock = MaintenanceLock::acquire(home)?;
    let services = match Registration::of(home)? {
        Registration::Owned | Registration::Absent => Services::new(home),
        Registration::Other(other) => bail!(
            "Carillon's user units run the installation at {}; updating {} would take them over. Stop that installation and run {} first",
            other.display(),
            home.display(),
            app.join("bin/carillon-register-services").display()
        ),
    };
    let activity = services.activity()?;

    let staging = tempfile::Builder::new()
        .prefix(".update-")
        .tempdir_in(home)
        .with_context(|| format!("create staging directory in {}", home.display()))?;
    let release = prepare(home, archive, staging.path())
        .with_context(|| format!("refused release {}; nothing was changed", archive.display()))?;
    let version = &release.version;
    println!(
        "Updating {} to Carillon {version}; services are {activity}.",
        home.display()
    );

    if activity == Activity::Running {
        services.control(Action::Stop).context(
            "stopping the services failed; the application was not replaced. \
             Inspect `carillon logs`, then run `carillon start` or retry the update",
        )?;
    }
    let stopped = match activity {
        Activity::Running => "Services are stopped (they were running)",
        Activity::Stopped => "Services are stopped as before",
    };
    let published = release.install(home, false).with_context(|| {
        format!(
            "could not run the release installer, so the application was not replaced. \
             {stopped}; `carillon start` resumes the current release"
        )
    })?;
    if !published.success() {
        bail!(
            "publishing Carillon {version} failed ({published}). Registry, credentials, provider \
             settings and state were not rewritten, but {} may hold the previous or the new \
             application and the user units may be mixed. {stopped}. Rerun `carillon update {}` \
             until it succeeds, then `carillon start` if the services should run",
            app.display(),
            archive.display(),
        );
    }

    if activity == Activity::Stopped {
        println!("Updated to Carillon {version}; services remain stopped as before.");
        return Ok(());
    }
    let recovery = format!(
        "Carillon {version} is installed at {} and the previous application was not kept. \
         Inspect `carillon logs` and `carillon status`. Another release can be installed with \
         `carillon update`, which checks the registry, core database and Discord settings \
         against that release but not other provider-native state such as Pi's, so an older \
         release is not known to be safe",
        app.display()
    );
    services
        .control(Action::Start)
        .with_context(|| format!("restarting the services failed. {recovery}"))?;
    let health = wait_until_healthy(&config, timeout)
        .await
        .with_context(|| format!("services started but are not healthy. {recovery}"))?;
    println!("Updated to Carillon {version}; services are running again:");
    print_health(&health);
    Ok(())
}

/// An unpacked release whose identity and payload were checked.
struct Release {
    root: PathBuf,
    version: String,
}

impl Release {
    /// Run the release's `install.sh --update`, or only its read-only
    /// `--check`, for an installation whose maintenance lock is held.
    fn install(&self, home: &Path, check: bool) -> std::io::Result<ExitStatus> {
        let mut installer = Process::new(self.root.join("install.sh"));
        installer.arg("--update");
        if check {
            installer.arg("--check");
        }
        installer
            .env("CARILLON_HOME", home)
            .env(MaintenanceLock::HELD_FOR, home)
            .status()
    }
}

/// Everything that must hold before services are disturbed: a safely
/// unpacked, correctly identified release whose own preflight accepts its
/// payload, this system and the installation's current state.
fn prepare(home: &Path, archive: &Path, staging: &Path) -> Result<Release> {
    let release = unpack_release(archive, staging)?;
    let preflight = release
        .install(home, true)
        .context("run the release's preflight")?;
    if !preflight.success() {
        bail!(
            "the preflight of Carillon {} failed ({preflight}); its reasons are printed above",
            release.version
        );
    }
    Ok(release)
}

/// Unpack the archive into `staging` and return the release directory and its
/// version, requiring one `carillon-VERSION` directory whose `bin/carillon`
/// runs here and reports that version. The rest of the payload is checked by
/// the release's installer against its manifest.
fn unpack_release(archive: &Path, staging: &Path) -> Result<Release> {
    unpack_regular(archive, staging)?;
    let mut entries = fs::read_dir(staging)?.collect::<std::io::Result<Vec<_>>>()?;
    let root = match entries.pop() {
        Some(entry) if entries.is_empty() && entry.file_type()?.is_dir() => entry.path(),
        _ => bail!("a release archive contains exactly one carillon-VERSION directory"),
    };
    let version = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("carillon-"))
        .filter(|version| !version.is_empty())
        .context("a release archive contains exactly one carillon-VERSION directory")?
        .to_owned();
    for program in ["install.sh", "bin/carillon"] {
        let executable = fs::symlink_metadata(root.join(program))
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o100 != 0);
        if !executable {
            bail!("release {program} is missing or not executable");
        }
    }
    let output = Process::new(root.join("bin/carillon"))
        .arg("--version")
        .output()
        .context("run the release's carillon on this host")?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || reported.trim() != format!("carillon {version}") {
        bail!(
            "release carillon reports {:?}, not version {version}",
            reported.trim()
        );
    }
    Ok(Release { root, version })
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path};

    use anyhow::{Context as _, Result};
    use flate2::{Compression, write::GzEncoder};

    /// A release whose installer and `carillon` are executable scripts; the
    /// rest of the payload belongs to the installer's own manifest check.
    fn release(root: &Path, version: &str, reported: &str) -> Result<()> {
        let release = root.join(format!("carillon-{version}"));
        fs::create_dir_all(release.join("bin"))?;
        for (file, script) in [
            ("install.sh", "#!/bin/sh\n".to_owned()),
            (
                "bin/carillon",
                format!("#!/bin/sh\necho 'carillon {reported}'\n"),
            ),
        ] {
            fs::write(release.join(file), script)?;
            fs::set_permissions(release.join(file), fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    type Extra = fn(&mut tar::Builder<GzEncoder<fs::File>>) -> Result<()>;

    fn archive(source: &Path, destination: &Path, extra: Extra) -> Result<()> {
        let mut builder = tar::Builder::new(GzEncoder::new(
            fs::File::create(destination)?,
            Compression::fast(),
        ));
        builder.append_dir_all(".", source)?;
        extra(&mut builder)?;
        builder.into_inner()?.finish()?;
        Ok(())
    }

    #[test]
    fn releases_are_identified_by_their_runnable_binary() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        release(&source, "1.2.3", "1.2.3")?;
        let path = directory.path().join("release.tar.gz");
        archive(&source, &path, |_| Ok(()))?;
        let staging = directory.path().join("ok");
        fs::create_dir(&staging)?;
        let unpacked = super::unpack_release(&path, &staging)?;
        assert_eq!(unpacked.version, "1.2.3");
        assert_eq!(unpacked.root, staging.join("carillon-1.2.3"));

        fs::remove_dir_all(&source)?;
        release(&source, "1.2.3", "9.9.9")?;
        archive(&source, &path, |_| Ok(()))?;
        let staging = directory.path().join("mismatch");
        fs::create_dir(&staging)?;
        let failure = super::unpack_release(&path, &staging)
            .err()
            .context("accepted a relabelled release")?;
        assert!(
            failure.to_string().contains("not version 1.2.3"),
            "{failure}"
        );

        // The installer is checked before anything from the release runs.
        fs::remove_dir_all(&source)?;
        release(&source, "1.2.3", "1.2.3")?;
        let installer = source.join("carillon-1.2.3/install.sh");
        fs::set_permissions(&installer, fs::Permissions::from_mode(0o644))?;
        archive(&source, &path, |_| Ok(()))?;
        let staging = directory.path().join("installer");
        fs::create_dir(&staging)?;
        let failure = super::unpack_release(&path, &staging)
            .err()
            .context("accepted a release whose installer cannot run")?;
        assert!(
            failure
                .to_string()
                .contains("install.sh is missing or not executable"),
            "{failure}"
        );
        Ok(())
    }

    #[test]
    fn links_escapes_and_extra_roots_are_refused() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        release(&source, "1.2.3", "1.2.3")?;
        let outside = directory.path().join("outside");
        let cases: [(&str, Extra); 3] = [
            ("unsupported entry", |builder| {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                builder.append_link(&mut header, "carillon-1.2.3/lib/link", "/etc")?;
                Ok(())
            }),
            ("unsafe path", |builder| {
                let mut header = tar::Header::new_gnu();
                header.set_size(1);
                header.set_mode(0o644);
                // `append_data` refuses `..`; write the raw name like a hostile archive.
                header.as_gnu_mut().context("gnu header")?.name[..9].copy_from_slice(b"../escape");
                header.set_cksum();
                builder.append(&header, &b"x"[..])?;
                Ok(())
            }),
            ("exactly one", |builder| {
                let mut header = tar::Header::new_gnu();
                header.set_size(1);
                header.set_mode(0o644);
                builder.append_data(&mut header, "second/file", &b"x"[..])?;
                Ok(())
            }),
        ];
        for (index, (expected, extra)) in cases.into_iter().enumerate() {
            let path = directory.path().join(format!("{index}.tar.gz"));
            archive(&source, &path, extra)?;
            let staging = directory.path().join(format!("staging-{index}"));
            fs::create_dir(&staging)?;
            let failure = super::unpack_release(&path, &staging)
                .err()
                .with_context(|| format!("accepted archive expecting {expected}"))?;
            assert!(format!("{failure:#}").contains(expected), "{failure:#}");
            assert!(!outside.exists() && !directory.path().join("escape").exists());
        }
        Ok(())
    }
}
