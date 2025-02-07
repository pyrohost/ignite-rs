// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
// SPDX-FileCopyrightText: 2020 Andreas Fuchs <asf@boinkor.net>
// SPDX-FileCopyrightText: 2021 Yannik Sander <contact@ysndr.de>
//
// SPDX-License-Identifier: MPL-2.0

use rnix::{self, SyntaxKind};
use merge::Merge;
use thiserror::Error;
use flexi_logger::*;
use std::path::{Path, PathBuf};

pub fn make_lock_path(temp_path: &Path, closure: &str) -> PathBuf {
    let lock_hash =
        &closure["/nix/store/".len()..closure.find('-').unwrap_or_else(|| closure.len())];
    temp_path.join(format!("ignite-rs-canary-{}", lock_hash))
}

const fn make_emoji(level: log::Level) -> &'static str {
    match level {
        log::Level::Error => "❌",
        log::Level::Warn => "⚠️",
        log::Level::Info => "ℹ️",
        log::Level::Debug => "❓",
        log::Level::Trace => "🖊️",
    }
}

pub fn logger_formatter_activate(
    w: &mut dyn std::io::Write,
    _now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    let level = record.level();

    write!(
        w,
        "⭐ {} [activate] [{}] {}",
        make_emoji(level),
        level.to_string(),
        record.args()
    )
}

pub fn logger_formatter_wait(
    w: &mut dyn std::io::Write,
    _now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    let level = record.level();

    write!(
        w,
        "👀 {} [wait] [{}] {}",
        make_emoji(level),
        level.to_string(),
        record.args()
    )
}

pub fn logger_formatter_revoke(
    w: &mut dyn std::io::Write,
    _now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    let level = record.level();

    write!(
        w,
        "↩️ {} [revoke] [{}] {}",
        make_emoji(level),
        level.to_string(),
        record.args()
    )
}

pub fn logger_formatter_deploy(
    w: &mut dyn std::io::Write,
    _now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    let level = record.level();

    write!(
        w,
        "🚀 {} [deploy] [{}] {}",
        make_emoji(level),
        level.to_string(),
        record.args()
    )
}

pub enum LoggerType {
    Deploy,
    Activate,
    Wait,
    Revoke,
}

pub fn init_logger(
    debug_logs: bool,
    log_dir: Option<&str>,
    logger_type: &LoggerType,
) -> Result<(), FlexiLoggerError> {
    let logger_formatter = match logger_type {
        LoggerType::Deploy => logger_formatter_deploy,
        LoggerType::Activate => logger_formatter_activate,
        LoggerType::Wait => logger_formatter_wait,
        LoggerType::Revoke => logger_formatter_revoke,
    };

    // Only the first call is fallible:
    let mut logger = Logger::try_with_env_or_str(if debug_logs { "debug" } else { "info" })?;
    
    if let Some(log_dir) = log_dir {
        let file_spec = FileSpec::try_from(log_dir)?;
        logger = logger
            .log_to_file(file_spec)
            .format_for_stderr(logger_formatter)
            .set_palette("196;208;51;7;8".to_string())
            .write_mode(WriteMode::Direct)
            .duplicate_to_stderr(if debug_logs { Duplicate::Debug } else { Duplicate::Info });
        logger.start()?;  // start logger here
    } else {
        logger = logger
            .log_to_stderr()
            .format(logger_formatter)
            .set_palette("196;208;51;7;8".to_string());
        logger.start()?; // start logger here
    }
    Ok(())
}

pub mod build;
pub mod cli;
pub mod data;
pub mod deploy;
pub mod push;
pub mod tui;

#[derive(Debug, Clone)]
pub struct CmdOverrides {
    pub ssh_user: Option<String>,
    pub profile_user: Option<String>,
    pub ssh_opts: Option<String>,
    pub fast_connection: Option<bool>,
    pub auto_rollback: Option<bool>,
    pub hostname: Option<String>,
    pub magic_rollback: Option<bool>,
    pub temp_path: Option<PathBuf>,
    pub confirm_timeout: Option<u16>,
    pub activation_timeout: Option<u16>,
    pub sudo: Option<String>,
    pub interactive_sudo: Option<bool>,
    pub dry_activate: bool,
    pub remote_build: bool,
}

impl Default for CmdOverrides {
    fn default() -> Self {
        Self {
            ssh_user: None,
            profile_user: None,
            ssh_opts: None,
            fast_connection: None,
            auto_rollback: None,
            hostname: None,
            magic_rollback: None,
            temp_path: None,
            confirm_timeout: None,
            activation_timeout: None,
            sudo: None,
            interactive_sudo: None,
            dry_activate: false,
            remote_build: false,
        }
    }
}

#[derive(PartialEq, Debug)]
pub struct DeployFlake<'a> {
    pub repo: &'a str,
    pub node: Option<String>,
    pub profile: Option<String>,
}

#[derive(Error, Debug)]
pub enum ParseFlakeError {
    #[error("The given path was too long, did you mean to put something in quotes?")]
    PathTooLong,
    #[error("Unrecognized node or token encountered")]
    Unrecognized,
}
pub fn parse_flake(flake: &str) -> Result<DeployFlake, ParseFlakeError> {
    let flake_fragment_start = flake.find('#');
    let (repo, maybe_fragment) = match flake_fragment_start {
        Some(s) => (&flake[..s], Some(&flake[s + 1..])),
        None => (flake, None),
    };

    let mut node: Option<String> = None;
    let mut profile: Option<String> = None;

    if let Some(fragment) = maybe_fragment {
        let ast = rnix::Root::parse(fragment);
        let first_child = ast.syntax();

        let mut node_over = false;
        for entry in first_child.children_with_tokens() {
            let x: Option<String> = match (entry.kind(), node_over) {
                (SyntaxKind::TOKEN_DOT, false) => {
                    node_over = true;
                    None
                }
                (SyntaxKind::TOKEN_DOT, true) => {
                    return Err(ParseFlakeError::PathTooLong);
                }
                (SyntaxKind::NODE_IDENT, _) => Some(entry.into_node().unwrap().text().to_string()),
                (rnix::SyntaxKind::TOKEN_IDENT, _) => Some(entry.into_token().unwrap().text().to_string()),
                (rnix::SyntaxKind::NODE_STRING, _) => {
                    let c = entry
                        .into_node()
                        .unwrap()
                        .children_with_tokens()
                        .nth(1)
                        .unwrap();

                    Some(c.into_token().unwrap().text().to_string())
                }
                _ => return Err(ParseFlakeError::Unrecognized),
            };
            if !node_over {
                node = x;
            } else {
                profile = x;
            }
        }
    }
    Ok(DeployFlake { repo, node, profile })
}

#[derive(Debug, Clone)]
pub struct DeployData<'a> {
    pub node_name: &'a str,
    pub node: &'a data::Node,
    pub profile_name: &'a str,
    pub profile: &'a data::Profile,

    pub cmd_overrides: &'a CmdOverrides,

    pub merged_settings: data::GenericSettings,

    pub debug_logs: bool,
    pub log_dir: Option<&'a str>,
}

#[derive(Debug)]
pub struct DeployDefs {
    pub ssh_user: String,
    pub profile_user: String,
    pub sudo: Option<String>,
    pub sudo_password: Option<String>,
}
enum ProfileInfo {
    ProfilePath {
        profile_path: String,
    },
    ProfileUserAndName {
        profile_user: String,
        profile_name: String,
    },
}

#[derive(Error, Debug)]
pub enum DeployDataDefsError {
    #[error("Neither `user` nor `sshUser` are set for profile {0} of node {1}")]
    NoProfileUser(String, String),
}

impl<'a> DeployData<'a> {
    pub fn defs(&'a self) -> Result<DeployDefs, DeployDataDefsError> {
        let ssh_user = match self.merged_settings.ssh_user {
            Some(ref u) => u.clone(),
            None => whoami::username(),
        };

        let profile_user = self.get_profile_user()?;

        let sudo: Option<String> = match self.merged_settings.user {
            Some(ref user) if user != &ssh_user => Some(format!("{} {}", self.get_sudo(), user)),
            _ => None,
        };

        Ok(DeployDefs {
            ssh_user,
            profile_user,
            sudo,
            sudo_password: None,
        })
    }

    fn get_profile_user(&'a self) -> Result<String, DeployDataDefsError> {
        let profile_user = match self.merged_settings.user {
            Some(ref x) => x.clone(),
            None => match self.merged_settings.ssh_user {
                Some(ref x) => x.clone(),
                None => {
                    return Err(DeployDataDefsError::NoProfileUser(
                        self.profile_name.to_owned(),
                        self.node_name.to_owned(),
                    ))
                }
            },
        };
        Ok(profile_user)
    }

    fn get_sudo(&'a self) -> String {
        match self.merged_settings.sudo {
            Some(ref x) => x.clone(),
            None => "sudo -u".to_string(),
        }
    }

    fn get_profile_info(&'a self) -> Result<ProfileInfo, DeployDataDefsError> {
        match self.profile.profile_settings.profile_path {
            Some(ref profile_path) => Ok(ProfileInfo::ProfilePath { profile_path: profile_path.to_string() }),
            None => {
                let profile_user = self.get_profile_user()?;
                Ok(ProfileInfo::ProfileUserAndName { profile_user, profile_name: self.profile_name.to_string() })
            },
        }
    }
}

pub fn make_deploy_data<'a, 's>(
    top_settings: &'s data::GenericSettings,
    node: &'a data::Node,
    node_name: &'a str,
    profile: &'a data::Profile,
    profile_name: &'a str,
    cmd_overrides: &'a CmdOverrides,
    debug_logs: bool,
    log_dir: Option<&'a str>,
) -> DeployData<'a> {
    let mut merged_settings = profile.generic_settings.clone();
    merged_settings.merge(node.generic_settings.clone());
    merged_settings.merge(top_settings.clone());

    // build all machines remotely when the command line flag is set
    if cmd_overrides.remote_build {
        merged_settings.remote_build = Some(cmd_overrides.remote_build);
    }
    if cmd_overrides.ssh_user.is_some() {
        merged_settings.ssh_user = cmd_overrides.ssh_user.clone();
    }
    if cmd_overrides.profile_user.is_some() {
        merged_settings.user = cmd_overrides.profile_user.clone();
    }
    if let Some(ref ssh_opts) = cmd_overrides.ssh_opts {
        merged_settings.ssh_opts = ssh_opts.split(' ').map(|x| x.to_owned()).collect();
    }
    if let Some(fast_connection) = cmd_overrides.fast_connection {
        merged_settings.fast_connection = Some(fast_connection);
    }
    if let Some(auto_rollback) = cmd_overrides.auto_rollback {
        merged_settings.auto_rollback = Some(auto_rollback);
    }
    if let Some(magic_rollback) = cmd_overrides.magic_rollback {
        merged_settings.magic_rollback = Some(magic_rollback);
    }
    if let Some(confirm_timeout) = cmd_overrides.confirm_timeout {
        merged_settings.confirm_timeout = Some(confirm_timeout);
    }
    if let Some(activation_timeout) = cmd_overrides.activation_timeout {
        merged_settings.activation_timeout = Some(activation_timeout);
    }
    if let Some(interactive_sudo) = cmd_overrides.interactive_sudo {
        merged_settings.interactive_sudo = Some(interactive_sudo);
    }

    DeployData {
        node_name,
        node,
        profile_name,
        profile,
        cmd_overrides,
        merged_settings,
        debug_logs,
        log_dir,
    }
}