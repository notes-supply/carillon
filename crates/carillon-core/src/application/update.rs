//! Replacing an installation's application from a local release archive.
//!
//! The archive is unpacked and checked while the services keep running. The
//! release's own `install.sh --update` then performs the existing atomic
//! application exchange and service registration; it does not touch the
//! registry, credentials, provider settings or state. Application files,
//! user units and systemd are not one transaction, so failures report what
//! may have changed and how to finish, instead of rolling back.

use std::{
    fs,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::Command as Process,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{config::LoadedConfig, home::is_managed_application};

use super::{
    admin::unpack_regular,
    operations::{
        Action, Activity, MaintenanceLock, Registration, Services, print_health, wait_until_healthy,
    },
};

/// Files and directories `install.sh --update` publishes; checked before
/// anything is stopped.
const RELEASE_ENTRIES: [&str; 8] = [
    "install.sh",
    "uninstall.sh",
    "bin/carillon",
    "bin/carillon-register-services",
    "bin",
    "lib",
    "licenses",
    "systemd",
];

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
    let (release, version) = unpack_release(archive, staging.path())
        .with_context(|| format!("refused release {}; nothing was changed", archive.display()))?;
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
    let published = Process::new(release.join("install.sh"))
        .arg("--update")
        .env("CARILLON_HOME", home)
        .env(MaintenanceLock::HELD_FOR, home)
        .status()
        .context("run the release installer")?;
    if !published.success() {
        bail!(
            "publishing Carillon {version} failed ({published}). Registry, credentials, provider \
             settings and state were not rewritten, but {} may hold the previous or the new \
             application and the user units may be mixed. Services are stopped{}. Rerun \
             `carillon update {}` until it succeeds{}",
            app.display(),
            if activity == Activity::Running {
                " (they were running)"
            } else {
                ""
            },
            archive.display(),
            if activity == Activity::Running {
                ", then `carillon start` if they remain stopped"
            } else {
                ""
            }
        );
    }

    if activity == Activity::Stopped {
        println!("Updated to Carillon {version}; services remain stopped as before.");
        return Ok(());
    }
    let recovery = format!(
        "Carillon {version} is installed at {}. Inspect `carillon logs` and `carillon status`; \
         to return to the previous release, run `carillon update` with its archive",
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

/// Unpack the archive into `staging` and return the release directory and its
/// version, requiring one `carillon-VERSION` directory whose `bin/carillon`
/// runs here and reports that version.
fn unpack_release(archive: &Path, staging: &Path) -> Result<(PathBuf, String)> {
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
    for entry in RELEASE_ENTRIES {
        if !root.join(entry).exists() {
            bail!("release is missing {entry}");
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
    Ok((root, version))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path};

    use anyhow::{Context as _, Result};
    use flate2::{Compression, write::GzEncoder};

    fn release(root: &Path, version: &str, reported: &str) -> Result<()> {
        let release = root.join(format!("carillon-{version}"));
        for directory in ["bin", "lib", "licenses", "systemd"] {
            fs::create_dir_all(release.join(directory))?;
        }
        for file in [
            "install.sh",
            "uninstall.sh",
            "bin/carillon-register-services",
        ] {
            fs::write(release.join(file), "")?;
        }
        let carillon = release.join("bin/carillon");
        fs::write(
            &carillon,
            format!("#!/bin/sh\necho 'carillon {reported}'\n"),
        )?;
        fs::set_permissions(&carillon, fs::Permissions::from_mode(0o755))?;
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
        let (root, version) = super::unpack_release(&path, &staging)?;
        assert_eq!(version, "1.2.3");
        assert_eq!(root, staging.join("carillon-1.2.3"));

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
