use clap::Parser;
use color_eyre::eyre::bail;
use color_eyre::Result;
use directories::BaseDirs;
use directories::ProjectDirs;
use globset::{Glob, GlobSetBuilder};
use ignore::DirEntry;
use ignore::WalkBuilder;
use jiff::{tz::TimeZone, Timestamp, ToSpan};
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::info;
use tracing::warn;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;
use varpath::environment::Environment;
use varpath::environment::EnvironmentBuilder;
use varpath::VarPath;
use which::which;

const THIS_CRATE_NAME: &'static str = env!("CARGO_PKG_NAME");
const DEFAULT_LOG_LEVEL: &'static str = "INFO";
const DEFAULT_TIMEZONE: &'static str = "UTC";
const DEFAULT_TIMESTAMP_FMT: &'static str = "%Y-%m-%d_%T";

#[derive(Debug, Parser)]
#[command(version, about, long_about=None)]
struct Cli {
    /// Do not execute the rsync command, only print what would be executed.
    #[clap(short, long, group = "dry-run")]
    dry_run: bool,

    /// Pass '--dry-run' to rsync.
    /// Unlike this program's '--dry-run' option, this does execute rsync.
    #[clap(long, group = "dry-run")]
    rsync_dry_run: bool,

    /// Log level (TRACE, DEBUG, INFO, WARN, ERROR).
    #[clap(long, short)]
    log_level: Option<tracing::Level>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
/// Settings to control file discovery.
/// Settings are passed to [ignore::WalkiBuilder](https://docs.rs/ignore/latest/ignore/struct.WalkBuilder.html)
struct IgnoreSettings {
    /// Enable ignorning hidden files
    hidden: Option<bool>,
    /// Enable reading ignore files from parent directories
    parents: Option<bool>,
    /// Enable reading `.ignore` files
    ignore: Option<bool>,
    /// Enable reading global gitignore file
    git_global: Option<bool>,
    /// Enable reading `.gitignore` files
    git_ignore: Option<bool>,
    /// Enable reading `.git/info/exclude` files
    git_exclude: Option<bool>,
    /// Do not cross file system boundaries
    same_file_system: Option<bool>,
}

impl Default for IgnoreSettings {
    fn default() -> Self {
        Self {
            hidden: None,
            parents: None,
            ignore: None,
            git_ignore: None,
            git_global: None,
            git_exclude: None,
            same_file_system: None,
        }
    }
}

fn default_timezone() -> String {
    String::from(DEFAULT_TIMEZONE)
}

fn default_timestamp_fmt() -> String {
    String::from(DEFAULT_TIMESTAMP_FMT)
}

fn default_relative_to() -> VarPath {
    VarPath::from_str("${HOME}").unwrap()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeneralSettings {
    log_level: Option<String>,

    /// List of globs to exclude
    exclude: Vec<String>,

    /// Base path relative source paths should be relative to
    #[serde(default = "default_relative_to")]
    relative_to: VarPath,

    #[serde(default = "default_timezone")]
    timezone: String,

    #[serde(default = "default_timestamp_fmt")]
    timestamp_fmt: String,

    #[serde(flatten)]
    ignore_settings: IgnoreSettings,
}
impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            log_level: Some(String::from(DEFAULT_LOG_LEVEL)),
            exclude: Vec::new(),
            timezone: default_timezone(),
            timestamp_fmt: default_timestamp_fmt(),
            ignore_settings: IgnoreSettings::default(),
            relative_to: default_relative_to(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct SyncSettings {
    path: PathBuf,
    exclude: Vec<String>,
    #[serde(flatten)]
    ignore_settings: IgnoreSettings,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteSettings {
    user: String,
    host: IpAddr,
    destination: VarPath,
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    general: Option<GeneralSettings>,
    sync: Vec<SyncSettings>,
    remote: RemoteSettings,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let default_log_level = tracing::Level::DEBUG;

    let Some(proj_dirs) = ProjectDirs::from("", "", THIS_CRATE_NAME) else {
        bail!("Unable to get XDG dirs");
    };
    let config_dir = proj_dirs.config_dir();
    let config_file = config_dir.join("config.toml");
    let contents = fs::read_to_string(&config_file)?;
    let config: Config = toml::from_str(&contents)?;
    let general_settings = match config.general {
        Some(ref general) => general.clone(),
        None => GeneralSettings::default(),
    };
    let remote_settings = &config.remote;

    let args = Cli::parse();
    let dry_run = args.dry_run;
    let rsync_dry_run = args.rsync_dry_run;
    let log_level = match args.log_level {
        Some(level) => level,
        None => match config.general {
            Some(ref general) => match &general.log_level {
                Some(level) => tracing::Level::from_str(&level)?,
                None => default_log_level,
            },
            None => default_log_level,
        },
    };

    let subscriber = FmtSubscriber::builder().with_max_level(log_level).finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    debug!("config: {:?}", &config);
    debug!("args: {:?}", args);

    let rsync = which("rsync")?;
    debug!("rsync: {}", rsync.display());

    let Some(base_dirs) = BaseDirs::new() else {
        bail!("Unable to get HOME dir");
    };
    let home_dir = base_dirs.home_dir();

    let local_hostname = gethostname::gethostname();
    let local_hostname = local_hostname.to_str().unwrap();

    let user = &remote_settings.user;
    let remote_host = remote_settings.host;
    let remote = format!(
        "{user}@{remote_host}",
        user = user,
        remote_host = remote_host
    );

    let timestamp = Timestamp::now().intz(&general_settings.timezone)?;
    let formatted_timestamp = timestamp
        .strftime(&general_settings.timestamp_fmt)
        .to_string();

    // TODO: document what variables are set outside of process env
    let variables = EnvironmentBuilder::default()
        .with_process_env()
        .set("timestamp", &formatted_timestamp)
        .set("hostname", local_hostname)
        .build();
    let destination_dir = &remote_settings.destination.eval(&variables)?;

    let destination = format!(
        "{remote}:{dest_dir}/", // Trailing slash to tell rsync to put contents in this directory
        remote = remote,
        dest_dir = destination_dir.display(),
    );

    for sync in &config.sync {
        let source = &sync.path;
        let source = if source.is_relative() {
            let base = general_settings.relative_to.eval(&variables)?;
            let absolute = base.join(source);
            debug!(
                "Relative dir {source} -> {absolute}",
                source = source.display(),
                absolute = absolute.display()
            );
            absolute
        } else {
            source.to_path_buf()
        };
        let source = source.canonicalize()?;

        let mut excludes = HashSet::new();
        for e in &sync.exclude {
            excludes.insert(e);
        }
        for e in &general_settings.exclude {
            excludes.insert(e);
        }
        let mut glob_builder = GlobSetBuilder::new();
        for e in excludes {
            glob_builder.add(Glob::new(e)?);
        }
        let glob = glob_builder.build()?;
        let exclude_filter = move |entry: &DirEntry| {
            let path = entry.path().as_os_str().to_str().unwrap();
            !glob.is_match(path)
        };

        let hidden = match sync.ignore_settings.hidden {
            Some(b) => b,
            None => match general_settings.ignore_settings.hidden {
                Some(b) => b,
                None => true,
            },
        };

        let parents = match sync.ignore_settings.parents {
            Some(b) => b,
            None => match general_settings.ignore_settings.parents {
                Some(b) => b,
                None => true,
            },
        };

        let ignore = match sync.ignore_settings.ignore {
            Some(b) => b,
            None => match general_settings.ignore_settings.ignore {
                Some(b) => b,
                None => true,
            },
        };

        let git_ignore = match sync.ignore_settings.git_ignore {
            Some(b) => b,
            None => match general_settings.ignore_settings.git_ignore {
                Some(b) => b,
                None => true,
            },
        };

        let git_global = match sync.ignore_settings.git_global {
            Some(b) => b,
            None => match general_settings.ignore_settings.git_global {
                Some(b) => b,
                None => true,
            },
        };

        let git_exclude = match sync.ignore_settings.git_exclude {
            Some(b) => b,
            None => match general_settings.ignore_settings.git_exclude {
                Some(b) => b,
                None => true,
            },
        };

        let same_file_system = match sync.ignore_settings.same_file_system {
            Some(b) => b,
            None => match general_settings.ignore_settings.same_file_system {
                Some(b) => b,
                None => true,
            },
        };

        let walker = WalkBuilder::new(&source)
            .hidden(hidden)
            .parents(parents)
            .ignore(ignore)
            .git_ignore(git_ignore)
            .git_global(git_global)
            .git_exclude(git_exclude)
            .same_file_system(same_file_system)
            .filter_entry(exclude_filter)
            .build();
        for result in walker {
            let mut cmd = Command::new(&rsync);
            let result = result?;
            let source = result.path();

            let mut args = vec!["--archive", "--verbose", "--compress"];
            if rsync_dry_run {
                args.push("--dry-run");
            }
            args.push(source.to_str().unwrap());
            args.push(&destination);
            cmd.args(args);

            info!("Syncing {} to {}", source.display(), &destination);
            if dry_run {
                println!("[dry-run] {:?}", cmd);
            } else {
                // Create a oneshot to get back the status code of the child process once it finishes to our main task
                let (tx, rx) = oneshot::channel();

                cmd.stdout(Stdio::piped());

                let mut child = cmd.spawn().expect("Failed to start child process (rsync)");

                let stdout = child
                    .stdout
                    .take()
                    .expect("Unable to take handle to child's (rsync) stdout");

                let mut reader = BufReader::new(stdout).lines();
                tokio::spawn(async move {
                    let status = child
                        .wait()
                        .await
                        .expect("Child process (rsync) encountered an error");
                    // Send the status to the main task
                    tx.send(status).unwrap(); // TODO: handle error somehow
                });
                while let Some(line) = reader.next_line().await? {
                    println!("[rsync] {}", line);
                }
                let Ok(status) = rx.await else {
                    bail!("Unable to get status code from child process (rsync)");
                };
                let Some(code) = status.code() else {
                    bail!("Unable to get status code from child process (rsync)");
                };
                println!("rsync exited with code {}", code);
            }
        }
    }

    Ok(())
}
