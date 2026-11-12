//! The way across a format change. A release reads only its own formats,
//! and nothing translates another's.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use toml::Table;

use crate::{
    bundled::Bundled,
    config::{RELEASE_VERSION, absolutize},
};

/// Why this release refuses an installation another release wrote. Displayed,
/// it is the procedure that crosses to this release, with the steps this
/// installation needs and no others.
#[derive(Debug)]
pub struct Crossing {
    home: PathBuf,
    written: Written,
}

#[derive(Debug)]
enum Written {
    Registry {
        missing_lines: String,
        database_dir: Option<PathBuf>,
    },
    Database {
        database_dir: PathBuf,
    },
}

const DATABASE: &str = "carillon.sqlite3";

impl Crossing {
    /// The installation at `home` keeps a core database of another format in
    /// `data`.
    #[must_use]
    pub fn of_database(home: &Path, data: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            written: Written::Database {
                database_dir: data.to_path_buf(),
            },
        }
    }

    pub(crate) fn of_registry(path: &Path, text: &str) -> Option<Self> {
        let registry: Table = toml::from_str(text).ok()?;
        let lines: Vec<String> = registry
            .get("providers")?
            .as_array()?
            .iter()
            .filter_map(toml::Value::as_table)
            .filter(|provider| !provider.contains_key("program"))
            .map(|provider| {
                let id = provider.get("id").and_then(toml::Value::as_str);
                let id = id.unwrap_or_default();
                match Bundled::ALL.into_iter().find(|bundled| bundled.id() == id) {
                    Some(bundled) => format!(
                        "     under id = \"{id}\": program = \"{}\"",
                        bundled.program()
                    ),
                    None => format!(
                        "     under id = \"{id}\": name the program that serves it. Those this \
                         release bundles: {}",
                        Bundled::ALL
                            .map(|bundled| format!("program = \"{}\"", bundled.program()))
                            .join(", ")
                    ),
                }
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        let home = path.parent().unwrap_or(path);
        let data = registry.get("data_dir").and_then(toml::Value::as_str);
        let data = data.map(|data| absolutize(home, Path::new(data)));
        Some(Self {
            home: home.to_path_buf(),
            written: Written::Registry {
                missing_lines: lines.join("\n"),
                // Every published release whose registry names no program
                // kept the earlier database format.
                database_dir: data.filter(|data| data.join(DATABASE).symlink_metadata().is_ok()),
            },
        })
    }
}

impl std::error::Error for Crossing {}

impl fmt::Display for Crossing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let home = shell_word(&self.home);
        let registry = self.home.join("carillon.toml");
        let registry = registry.display();
        let (missing_lines, database_dir) = match &self.written {
            Written::Registry {
                missing_lines,
                database_dir,
            } => {
                write!(
                    f,
                    "{registry} does not say which program serves each provider, which \
                     Carillon {RELEASE_VERSION} requires"
                )?;
                if database_dir.is_some() {
                    write!(
                        f,
                        ", and the release that wrote it keeps core's database in another format"
                    )?;
                }
                (Some(missing_lines), database_dir.as_ref())
            }
            Written::Database { database_dir } => {
                write!(
                    f,
                    "core's database in {} is not in the format of Carillon {RELEASE_VERSION}",
                    database_dir.display()
                )?;
                (None, Some(database_dir))
            }
        };
        let mut steps = vec![format!(
            "Stop Carillon with the release it has, once `carillon status` shows no turn and \
             no delivery pending or active:\n     carillon --home {home} stop"
        )];
        if database_dir.is_some() {
            let mut backup = self.home.clone().into_os_string();
            backup.push(format!("-before-{RELEASE_VERSION}.tar.gz"));
            steps.push(format!(
                "Back it up with that release. The backup is the way back:\n     \
                 carillon --home {home} backup --output {}",
                shell_word(Path::new(&backup))
            ));
        }
        if let Some(missing_lines) = missing_lines {
            steps.push(format!(
                "In {registry}, add one line under each of these `id` lines, and change \
                 nothing else:\n{missing_lines}"
            ));
        }
        if let Some(database_dir) = database_dir {
            let aside = shell_word(&self.home.join(format!("core-before-{RELEASE_VERSION}")));
            let attachments = shell_word(&database_dir.join("attachments"));
            steps.push(format!(
                "Set core's database and the files it kept aside, in a new folder nothing \
                 reads:\n     \
                 mkdir -m 700 {aside} && mv {}* {aside} && \
                 {{ [ ! -e {attachments} ] || mv {attachments} {aside}; }}",
                shell_word(&database_dir.join(DATABASE))
            ));
        }
        let rejected = match missing_lines {
            Some(_) => " The release installed now cannot, because it rejects the lines.",
            None => "",
        };
        steps.push(format!(
            "From one folder, unpack this release and run its installer.{rejected} ARCHIVE \
             stands for the path of this release's archive, which you gave `carillon update` \
             or unpacked this installer from:\n     \
             tar -xzf ARCHIVE\n     \
             CARILLON_HOME={home} carillon-{RELEASE_VERSION}/install.sh --update"
        ));
        steps.push(format!("Start it:\n     carillon --home {home} start"));
        write!(
            f,
            ". To cross to this release, in this order, leaving out a step already done:"
        )?;
        for (step, text) in steps.iter().enumerate() {
            write!(f, "\n  {}. {text}", step + 1)?;
        }
        if database_dir.is_none() {
            return Ok(());
        }
        write!(
            f,
            "\nCore's record of past turns and the files it kept start over, and an answer \
             still running or not yet delivered at the stop is lost. The assistant's \
             conversations, schedules, settings, credentials and sign-ins stay. Each schedule \
             resumes after the next message in its conversation, and a run that falls due \
             before that message is skipped, not postponed."
        )
    }
}

fn shell_word(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        process::Command,
    };

    use anyhow::{Context as _, Result, ensure};

    use super::{Crossing, RELEASE_VERSION};
    use crate::config::Config;

    fn registry(programs: [Option<&str>; 4]) -> String {
        let hash = "0".repeat(64);
        let provider = |id: &str, role: &str, port: u16, program: Option<&str>| {
            let program = program.map(|program| format!("program = \"{program}\"\n"));
            let program = program.unwrap_or_default();
            format!(
                "[[providers]]\nid = \"{id}\"\nrole = \"{role}\"\n{program}version = \"1\"\n\
                 url = \"http://127.0.0.1:{port}\"\ntoken_hash = \"{hash}\"\nenabled = true\n"
            )
        };
        format!(
            "data_dir = \"state\"\n[core]\nlisten = \"127.0.0.1:44000\"\n\
             operator_token_hash = \"{hash}\"\n{}{}{}{}",
            provider("discord-private", "interface", 44_001, programs[0]),
            provider("runtime-pi", "runtime", 44_002, programs[1]),
            provider("runtime-t3", "runtime", 44_003, programs[2]),
            provider("chat", "interface", 44_009, programs[3]),
        )
    }

    struct QuotedHome {
        directory: tempfile::TempDir,
        home: PathBuf,
    }

    impl QuotedHome {
        fn new() -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let home = directory.path().join("owner's installation");
            fs::create_dir(&home)?;
            let record = "#!/bin/sh\nprintf '%s\\n' \"${CARILLON_HOME:-} $*\" >>\"$RECORD\"\n";
            let release = format!("carillon-{RELEASE_VERSION}");
            fs::create_dir(directory.path().join(&release))?;
            for program in ["carillon".to_owned(), format!("{release}/install.sh")] {
                let program = directory.path().join(program);
                fs::write(&program, record)?;
                fs::set_permissions(program, fs::Permissions::from_mode(0o700))?;
            }
            let packed = Command::new("tar")
                .args(["-czf", "ARCHIVE", "--remove-files", &release])
                .current_dir(directory.path())
                .status()?;
            ensure!(packed.success(), "pack the release");
            Ok(Self { directory, home })
        }

        fn with_state(self) -> Result<Self> {
            for kept in ["resource-snapshots/a", "components/runtime-pi"] {
                fs::create_dir_all(self.home.join("state").join(kept))?;
            }
            for file in ["", "-shm", "-wal"] {
                fs::write(
                    self.home.join(format!("state/carillon.sqlite3{file}")),
                    file,
                )?;
            }
            Ok(self)
        }

        fn run_printed_commands(&self, refusal: &str) -> Result<String> {
            let directory = self.directory.path();
            let search = std::env::join_paths(std::iter::once(directory.to_path_buf()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))?;
            let commands = refusal.lines().filter_map(|line| {
                let command = line.strip_prefix("     ")?;
                (!command.contains(": ")).then_some(command)
            });
            for command in commands {
                let ran = Command::new("sh")
                    .args(["-c", command])
                    .current_dir(directory)
                    .env("PATH", &search)
                    .env_remove("CARILLON_HOME")
                    .env("RECORD", directory.join("record"))
                    .status()?;
                ensure!(ran.success(), "{command}");
            }
            fs::read_to_string(directory.join("record")).context("nothing ran")
        }

        fn files(&self, folder: &str) -> Result<Vec<String>> {
            let mut names = fs::read_dir(self.home.join(folder))?
                .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
                .collect::<Result<Vec<_>>>()?;
            names.sort();
            Ok(names)
        }
    }

    fn refusal(path: &Path) -> String {
        let refused = Config::read(path).err();
        refused.map_or_else(String::new, |error| format!("{error:#}"))
    }

    #[test]
    fn a_registry_without_programs_and_its_database_are_crossed_as_printed() -> Result<()> {
        let owner = QuotedHome::new()?.with_state()?;
        let path = owner.home.join("carillon.toml");
        fs::write(&path, registry([None, Some("runtime-pi"), None, None]))?;
        let refusal = refusal(&path);
        let home = owner.home.display();
        let quoted = format!(
            "'{}/owner'\\''s installation",
            owner.directory.path().display()
        );
        let expected = format!(
            "{home}/carillon.toml does not say which program serves each provider, which \
             Carillon {RELEASE_VERSION} requires, and the release that wrote it keeps core's \
             database in another format. To cross to this release, in this order, leaving out \
             a step already done:\n  \
             1. Stop Carillon with the release it has, once `carillon status` shows no turn \
             and no delivery pending or active:\n     \
             carillon --home {quoted}' stop\n  \
             2. Back it up with that release. The backup is the way back:\n     \
             carillon --home {quoted}' backup --output {quoted}-before-{RELEASE_VERSION}.tar.gz'\n  \
             3. In {home}/carillon.toml, add one line under each of these `id` lines, and \
             change nothing else:\n     \
             under id = \"discord-private\": program = \"discord\"\n     \
             under id = \"runtime-t3\": program = \"runtime-t3\"\n     \
             under id = \"chat\": name the program that serves it. Those this release bundles: \
             program = \"web\", program = \"discord\", program = \"runtime-pi\", \
             program = \"runtime-t3\"\n  \
             4. Set core's database and the files it kept aside, in a new folder nothing \
             reads:\n     \
             mkdir -m 700 {quoted}/core-before-{RELEASE_VERSION}' && \
             mv {quoted}/state/carillon.sqlite3'* {quoted}/core-before-{RELEASE_VERSION}' && \
             {{ [ ! -e {quoted}/state/attachments' ] || mv {quoted}/state/attachments' \
             {quoted}/core-before-{RELEASE_VERSION}'; }}\n  \
             5. From one folder, unpack this release and run its installer. The release \
             installed now cannot, because it rejects the lines. ARCHIVE stands for the path \
             of this release's archive, which you gave `carillon update` or unpacked this \
             installer from:\n     \
             tar -xzf ARCHIVE\n     \
             CARILLON_HOME={quoted}' carillon-{RELEASE_VERSION}/install.sh --update\n  \
             6. Start it:\n     \
             carillon --home {quoted}' start\n\
             Core's record of past turns and the files it kept start over, and an answer \
             still running or not yet delivered at the stop is lost. The assistant's \
             conversations, schedules, settings, credentials and sign-ins stay. Each schedule \
             resumes after the next message in its conversation, and a run that falls due \
             before that message is skipped, not postponed."
        );
        assert_eq!(refusal, expected);

        assert_eq!(
            owner.run_printed_commands(&refusal)?,
            format!(
                " --home {home} stop\n --home {home} backup --output {home}-before-{RELEASE_VERSION}.tar.gz\n\
                 {home} --update\n --home {home} start\n"
            )
        );
        assert_eq!(
            owner.files(&format!("core-before-{RELEASE_VERSION}"))?,
            [
                "carillon.sqlite3",
                "carillon.sqlite3-shm",
                "carillon.sqlite3-wal"
            ]
        );
        assert_eq!(owner.files("state")?, ["components", "resource-snapshots"]);

        let named = ["discord", "runtime-pi", "runtime-t3", "example"].map(Some);
        fs::write(&path, registry(named))?;
        Config::read(&path)?.validate()
    }

    #[test]
    fn a_registry_without_programs_alone_is_crossed_without_touching_state() -> Result<()> {
        let owner = QuotedHome::new()?;
        let path = owner.home.join("carillon.toml");
        fs::write(&path, registry([None; 4]))?;
        let refusal = refusal(&path);
        for absent in ["backup", "mkdir", "mv ", "start over"] {
            assert!(!refusal.contains(absent), "{absent}: {refusal}");
        }
        assert!(refusal.contains("  2. In "), "{refusal}");
        assert!(refusal.contains("  4. Start it:"), "{refusal}");
        let home = owner.home.display();
        assert_eq!(
            owner.run_printed_commands(&refusal)?,
            format!(" --home {home} stop\n{home} --update\n --home {home} start\n")
        );
        Ok(())
    }

    #[test]
    fn a_database_alone_is_crossed_with_the_files_core_kept() -> Result<()> {
        let owner = QuotedHome::new()?.with_state()?;
        let kept = owner.home.join("state/attachments/turn");
        fs::create_dir_all(&kept)?;
        fs::write(kept.join("0"), "kept")?;
        let refusal = Crossing::of_database(&owner.home, &owner.home.join("state")).to_string();
        for absent in ["add one line", "rejects the lines"] {
            assert!(!refusal.contains(absent), "{absent}: {refusal}");
        }
        assert!(refusal.contains("  5. Start it:"), "{refusal}");
        let home = owner.home.display();
        assert_eq!(
            owner.run_printed_commands(&refusal)?,
            format!(
                " --home {home} stop\n --home {home} backup --output {home}-before-{RELEASE_VERSION}.tar.gz\n\
                 {home} --update\n --home {home} start\n"
            )
        );
        let aside = format!("core-before-{RELEASE_VERSION}");
        assert_eq!(
            owner.files(&aside)?,
            [
                "attachments",
                "carillon.sqlite3",
                "carillon.sqlite3-shm",
                "carillon.sqlite3-wal"
            ]
        );
        assert_eq!(
            fs::read_to_string(owner.home.join(&aside).join("attachments/turn/0"))?,
            "kept"
        );
        assert_eq!(owner.files("state")?, ["components", "resource-snapshots"]);

        let later = owner.home.join("state/carillon.sqlite3");
        fs::write(&later, "a later database")?;
        let set_aside = refusal.lines().find(|line| line.contains("mkdir "));
        let again = Command::new("sh")
            .args(["-c", set_aside.context("no step sets the database aside")?])
            .stderr(std::process::Stdio::null())
            .status()?;
        assert!(!again.success(), "set aside over an earlier crossing");
        assert_eq!(fs::read_to_string(later)?, "a later database");
        assert_eq!(
            fs::read_to_string(owner.home.join(aside).join("carillon.sqlite3"))?,
            ""
        );
        Ok(())
    }
}
