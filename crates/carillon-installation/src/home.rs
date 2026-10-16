//! Which installation an operator command acts on.
//!
//! One rule for every Carillon program: an explicit `--home`, then
//! `CARILLON_HOME`, then the installation containing the running executable,
//! otherwise `~/.carillon`. The current directory is never searched.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};

/// The file marking `app/` as a Carillon-managed application tree, and its
/// content. The release scripts check the same marker before replacing or
/// removing the tree.
pub const APPLICATION_MARKER: &str = ".carillon-managed";
pub const APPLICATION_MARKER_VALUE: &str = "carillon-managed-application-v1";

/// Resolve the installation root for this process.
///
/// # Errors
///
/// Returns an error when `CARILLON_HOME` is relative, or when no other rule
/// applies and `HOME` is unset or relative.
pub fn resolve(explicit: Option<&Path>) -> Result<PathBuf> {
    resolve_from(
        explicit,
        env::var_os("CARILLON_HOME"),
        env::current_exe().ok().as_deref(),
        env::var_os("HOME"),
    )
}

fn resolve_from(
    explicit: Option<&Path>,
    variable: Option<OsString>,
    executable: Option<&Path>,
    home: Option<OsString>,
) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        // An explicit operand may be relative to where the operator typed it.
        return std::path::absolute(explicit)
            .with_context(|| format!("resolve --home {}", explicit.display()));
    }
    if let Some(variable) = variable.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(variable);
        if !path.is_absolute() {
            bail!("CARILLON_HOME must be an absolute path");
        }
        return Ok(path);
    }
    if let Some(root) = executable.and_then(installed_root) {
        return Ok(root);
    }
    let home = PathBuf::from(home.unwrap_or_default());
    if !home.is_absolute() {
        bail!("HOME must be an absolute path to locate ~/.carillon; pass --home");
    }
    Ok(home.join(".carillon"))
}

/// The installation whose managed `app/bin` contains `executable`, following
/// symbolic links such as the owner's PATH entry point.
#[must_use]
pub fn installed_root(executable: &Path) -> Option<PathBuf> {
    let executable = executable.canonicalize().ok()?;
    let app = executable.parent()?.parent()?;
    (executable.parent()?.file_name()? == "bin"
        && app.file_name()? == "app"
        && is_managed_application(app))
    .then(|| app.parent().map(Path::to_path_buf))
    .flatten()
}

/// Whether `app` is a real directory carrying the ownership marker.
#[must_use]
pub fn is_managed_application(app: &Path) -> bool {
    let marker = app.join(APPLICATION_MARKER);
    fs::symlink_metadata(app).is_ok_and(|metadata| metadata.is_dir())
        && fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.is_file())
        && fs::read_to_string(&marker)
            .is_ok_and(|value| value.trim_end() == APPLICATION_MARKER_VALUE)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use anyhow::Result;

    use super::{APPLICATION_MARKER, APPLICATION_MARKER_VALUE, resolve_from};

    fn installed(root: &Path) -> Result<std::path::PathBuf> {
        let bin = root.join("app/bin");
        fs::create_dir_all(&bin)?;
        fs::write(bin.join("carillon"), "")?;
        fs::write(
            root.join("app").join(APPLICATION_MARKER),
            format!("{APPLICATION_MARKER_VALUE}\n"),
        )?;
        Ok(bin.join("carillon"))
    }

    #[test]
    fn explicit_then_environment_then_executable_then_user_home() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = installed(&directory.path().join("installed"))?;
        let explicit = directory.path().join("explicit");
        let variable = directory.path().join("variable");
        let home = directory.path().join("owner");
        let resolve =
            |explicit: Option<&Path>, variable: Option<&Path>, executable: Option<&Path>| {
                resolve_from(
                    explicit,
                    variable.map(Into::into),
                    executable,
                    Some(home.clone().into()),
                )
            };

        assert_eq!(
            resolve(Some(&explicit), Some(&variable), Some(&executable))?,
            explicit
        );
        assert_eq!(resolve(None, Some(&variable), Some(&executable))?, variable);
        assert_eq!(
            resolve(None, None, Some(&executable))?,
            directory.path().join("installed").canonicalize()?
        );
        assert_eq!(resolve(None, None, None)?, home.join(".carillon"));
        // An empty variable is unset, as in the release scripts.
        assert_eq!(
            resolve_from(None, Some("".into()), None, Some(home.clone().into()))?,
            home.join(".carillon")
        );
        Ok(())
    }

    #[test]
    fn only_a_marked_application_identifies_an_installation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let home = directory.path().join("owner");
        let executable = installed(&directory.path().join("installed"))?;
        let link = directory.path().join("carillon");
        std::os::unix::fs::symlink(&executable, &link)?;
        assert_eq!(
            super::installed_root(&link),
            Some(directory.path().join("installed").canonicalize()?)
        );

        // A release directory or build tree is not an installation, and
        // neither is a tree whose marker was removed.
        let release = directory.path().join("release/bin");
        fs::create_dir_all(&release)?;
        fs::write(release.join("carillon"), "")?;
        fs::remove_file(
            directory
                .path()
                .join("installed/app")
                .join(APPLICATION_MARKER),
        )?;
        for executable in [release.join("carillon"), executable] {
            assert_eq!(super::installed_root(&executable), None);
            assert_eq!(
                resolve_from(None, None, Some(&executable), Some(home.clone().into()))?,
                home.join(".carillon")
            );
        }
        Ok(())
    }

    #[test]
    fn relative_locations_are_refused_rather_than_searched() {
        let refusal = |variable: Option<&str>, home: Option<&str>| {
            resolve_from(None, variable.map(Into::into), None, home.map(Into::into))
                .map_or_else(|error| error.to_string(), |path| path.display().to_string())
        };
        assert_eq!(
            refusal(Some("relative"), None),
            "CARILLON_HOME must be an absolute path"
        );
        let home = "HOME must be an absolute path to locate ~/.carillon; pass --home";
        assert_eq!(refusal(None, Some("relative")), home);
        assert_eq!(refusal(None, None), home);
    }
}
