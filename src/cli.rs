// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
// SPDX-FileCopyrightText: 2021 Yannik Sander <contact@ysndr.de>
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;

use clap::{ArgMatches, Parser, FromArgMatches};
use futures::StreamExt;

use crate::{self as deploy, push::PushProfileData};
use self::deploy::{DeployFlake, ParseFlakeError};
use futures_util::stream::TryStreamExt;
use log::{debug, error, info, warn};
use std::path::PathBuf;
use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;
use tokio::{task, sync::mpsc, time::Duration};

use crate::{
    build::{evaluate_jobs, build_jobs},
    tui::{self, NodeStatus}
};

/// A fast and reliable deployment tool for mass-scale NixOS deployments
#[derive(Parser, Debug, Clone)]
#[command(version = "1.0", author = "Pyro Inc. <team@pyro.host>")]
pub struct Opts {
    /// The flake to deploy
    #[arg(group = "deploy")]
    pub target: Option<String>,

    /// A list of flakes to deploy alternatively
    #[arg(long, group = "deploy")]
    pub targets: Option<Vec<String>>,
    /// Check signatures when using `nix copy`
    #[arg(short, long)]
    pub checksigs: bool,
    /// Use the interactive prompt before deployment
    #[arg(short, long)]
    pub interactive: bool,
    /// Extra arguments to be passed to nix build
    pub extra_build_args: Vec<String>,

    /// Print debug logs to output
    #[arg(short, long)]
    pub debug_logs: bool,
    /// Directory to print logs to (including the background activation process)
    #[arg(long)]
    pub log_dir: Option<String>,

    /// Keep the build outputs of each built profile
    #[arg(short, long)]
    pub keep_result: bool,
    /// Location to keep outputs from built profiles in
    #[arg(short, long)]
    pub result_path: Option<String>,

    /// Skip the automatic pre-build checks
    #[arg(short, long)]
    pub skip_checks: bool,

    /// Build on remote host
    #[arg(long)]
    pub remote_build: bool,

    /// Override the SSH user with the given value
    #[arg(long)]
    pub ssh_user: Option<String>,
    /// Override the profile user with the given value
    #[arg(long)]
    pub profile_user: Option<String>,
    /// Override the SSH options used
    #[arg(long, allow_hyphen_values = true)]
    pub ssh_opts: Option<String>,
    /// Override if the connecting to the target node should be considered fast
    #[arg(long)]
    pub fast_connection: Option<bool>,
    /// Override if a rollback should be attempted if activation fails
    #[arg(long)]
    pub auto_rollback: Option<bool>,
    /// Override hostname used for the node
    #[arg(long)]
    pub hostname: Option<String>,
    /// Make activation wait for confirmation, or roll back after a period of time
    #[arg(long)]
    pub magic_rollback: Option<bool>,
    /// How long activation should wait for confirmation (if using magic-rollback)
    #[arg(long)]
    pub confirm_timeout: Option<u16>,
    /// How long we should wait for profile activation
    #[arg(long)]
    pub activation_timeout: Option<u16>,
    /// Where to store temporary files (only used by magic-rollback)
    #[arg(long)]
    pub temp_path: Option<PathBuf>,
    /// Show what will be activated on the machines
    #[arg(long)]
    pub dry_activate: bool,
    /// Don't activate, but update the boot loader to boot into the new profile
    #[arg(long)]
    pub boot: bool,
    /// Revoke all previously succeeded deploys when deploying multiple profiles
    #[arg(long)]
    pub rollback_succeeded: Option<bool>,
    /// Which sudo command to use. Must accept at least two arguments: user name to execute commands as and the rest is the command to execute
    #[arg(long)]
    pub sudo: Option<String>,
    /// Specify sudo password for non-interactive use
    #[arg(long, conflicts_with = "interactive_sudo")]
    pub sudo_password: Option<String>,
    /// Prompt for sudo password during activation.
    #[arg(long, conflicts_with = "sudo_password")]
    pub interactive_sudo: Option<bool>,
}

/// Returns if the available Nix installation supports flakes
async fn test_flake_support() -> Result<bool, std::io::Error> {
    debug!("Checking for flake support");

    Ok(Command::new("nix")
        .arg("eval")
        .arg("--expr")
        .arg("builtins.getFlake")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?
        .success())
}

#[derive(Error, Debug)]
pub enum CheckDeploymentError {
    #[error("Failed to execute Nix checking command: {0}")]
    NixCheck(#[from] std::io::Error),
    #[error("Nix checking command resulted in a bad exit code: {0:?}")]
    NixCheckExit(Option<i32>),
}

pub async fn check_deployment(
    supports_flakes: bool,
    repo: &str,
    extra_build_args: &[String],
) -> Result<(), CheckDeploymentError> {
    info!("Running checks for flake in {}", repo);

    let mut check_command = if supports_flakes {
        Command::new("nix")
    } else {
        Command::new("nix-build")
    };

    if supports_flakes {
        check_command.arg("flake").arg("check").arg(repo);
    } else {
        check_command.arg("-E")
            .arg("--no-out-link")
            .arg(format!("let r = import {}/.; x = (if builtins.isFunction r then (r {{}}) else r); in if x ? checks then x.checks.${{builtins.currentSystem}} else {{}}", repo));
    }

    check_command.args(extra_build_args);

    let check_status = check_command.status().await?;

    match check_status.code() {
        Some(0) => (),
        a => return Err(CheckDeploymentError::NixCheckExit(a)),
    };

    Ok(())
}

#[derive(Error, Debug)]
pub enum GetDeploymentDataError {
    #[error("Failed to execute nix eval command: {0}")]
    NixEval(std::io::Error),
    #[error("Failed to read output from evaluation: {0}")]
    NixEvalOut(std::io::Error),
    #[error("Evaluation resulted in a bad exit code: {0:?}")]
    NixEvalExit(Option<i32>),
    #[error("Error converting evaluation output to utf8: {0}")]
    DecodeUtf8(#[from] std::string::FromUtf8Error),
    #[error("Error decoding the JSON from evaluation: {0}")]
    DecodeJson(#[from] serde_json::error::Error),
    #[error("Impossible happened: profile is set but node is not")]
    ProfileNoNode,
}

/// Evaluates the Nix in the given `repo` and return the processed Data from it
pub async fn get_deployment_data(
    supports_flakes: bool,
    flakes: &[deploy::DeployFlake<'_>],
    extra_build_args: &[String],
) -> Result<Vec<deploy::data::Data>, GetDeploymentDataError> {
    futures_util::stream::iter(flakes).then(|flake| async move {
        info!("Evaluating flake in {}", flake.repo);

        let mut c = if supports_flakes {
            Command::new("nix")
        } else {
            Command::new("nix-instantiate")
        };

        if supports_flakes {
            c.arg("eval")
                .arg("--json")
                .arg(format!("{}#deploy", flake.repo))
                .arg("--apply");

            match (&flake.node, &flake.profile) {
                (Some(node), Some(profile)) => {
                    c.arg(format!(
                        r#"deploy: (deploy // {{ nodes = {{ "{0}" = deploy.nodes."{0}" // {{ profiles = {{ inherit (deploy.nodes."{0}".profiles) "{1}"; }}; }}; }}; }})"#,
                        node, profile
                    ))
                }
                (Some(node), None) => {
                    c.arg(format!(
                        r#"deploy: (deploy // {{ nodes = {{ inherit (deploy.nodes) "{}"; }}; }})"#,
                        node
                    ))
                }
                (None, None) => {
                    c.arg("deploy: deploy")
                }
                (None, Some(_)) => return Err(GetDeploymentDataError::ProfileNoNode),
            }
        } else {
            c.arg("--strict")
                .arg("--read-write-mode")
                .arg("--json")
                .arg("--eval")
                .arg("-E")
                .arg(format!("let r = import {}/.; in if builtins.isFunction r then (r {{}}).deploy else r.deploy", flake.repo))
        };

        c.args(extra_build_args);

        let build_child = c
            .stdout(Stdio::piped())
            .spawn()
            .map_err(GetDeploymentDataError::NixEval)?;

        let build_output = build_child
            .wait_with_output()
            .await
            .map_err(GetDeploymentDataError::NixEvalOut)?;

        match build_output.status.code() {
            Some(0) => (),
            a => return Err(GetDeploymentDataError::NixEvalExit(a)),
        };

        let data_json = String::from_utf8(build_output.stdout)?;

        Ok(serde_json::from_str(&data_json)?)
    }).try_collect().await
}

#[derive(Error, Debug)]
pub enum PromptDeploymentError {
    #[error("Failed to make printable TOML of deployment: {0}")]
    TomlFormat(#[from] toml::ser::Error),
    #[error("Failed to flush stdout prior to query: {0}")]
    StdoutFlush(std::io::Error),
    #[error("Failed to read line from stdin: {0}")]
    StdinRead(std::io::Error),
    #[error("User cancelled deployment")]
    Cancelled,
}

#[derive(Error, Debug)]
pub enum RunDeployError {
    #[error("Failed to deploy profile to node {0}: {1}")]
    DeployProfile(String, deploy::deploy::DeployProfileError),
    #[error("Failed to build profile on node {0}: {0}")]
    BuildProfile(String, deploy::push::PushProfileError),
    #[error("Failed to push profile to node {0}: {0}")]
    PushProfile(String, deploy::push::PushProfileError),
    #[error("No profile named `{0}` was found")]
    ProfileNotFound(String),
    #[error("No node named `{0}` was found")]
    NodeNotFound(String),
    #[error("Profile was provided without a node name")]
    ProfileWithoutNode,
    #[error("Error processing deployment definitions: {0}")]
    DeployDataDefs(#[from] deploy::DeployDataDefsError),
    #[error("Failed to make printable TOML of deployment: {0}")]
    TomlFormat(#[from] toml::ser::Error),
    #[error("{0}")]
    PromptDeployment(#[from] PromptDeploymentError),
    #[error("Failed to revoke profile for node {0}: {1}")]
    RevokeProfile(String, deploy::deploy::RevokeProfileError),
    #[error("Deployment to node {0} failed, rolled back to previous generation")]
    Rollback(String),
    #[error("Task join error: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
    #[error("Build error: {0}")]
    BuildError(String),
}

impl From<crate::build::BuildError> for RunDeployError {
    fn from(err: crate::build::BuildError) -> Self {
        RunDeployError::BuildError(err.to_string())
    }
}

impl From<std::io::Error> for RunDeployError {
    fn from(err: std::io::Error) -> Self {
        RunDeployError::BuildError(err.to_string())
    }
}

// Fix ToDeploy type - remove lifetime from Data

// Struct to capture context from each deployment task.
struct DeployTaskResult {
    node: String,
    push_result: Result<(), deploy::push::PushProfileError>,
    generic_settings: deploy::data::GenericSettings,
    node_data: deploy::data::Node,
    profile_data: deploy::data::Profile,
    cmd_overrides: deploy::CmdOverrides,
    log_dir: Option<String>,
    status_sender: mpsc::Sender<(String, NodeStatus)>,
    progress_sender: mpsc::Sender<(String, f32)>,
    log_sender: mpsc::Sender<(String, String)>,
    debug_logs: bool,
}

async fn handle_rollback<'a>(
    deploy_data: &'a deploy::DeployData<'a>,
    deploy_defs: &'a deploy::DeployDefs,
    node_name: &str,
    log_sender: &mpsc::Sender<(String, String)>,
    status_sender: &mpsc::Sender<(String, NodeStatus)>,
) -> Result<(), deploy::deploy::RevokeProfileError> {
    match deploy::deploy::revoke(deploy_data, deploy_defs).await {
        Ok(()) => {
            log_sender.send((node_name.to_string(), "Successfully rolled back to previous generation".to_string())).await.ok();
            status_sender.send((node_name.to_string(), NodeStatus::RolledBack)).await.ok();
            Ok(())
        }
        Err(e) => {
            let error_msg = format!("Failed to rollback: {}", e);
            log_sender.send((node_name.to_string(), error_msg.clone())).await.ok();
            status_sender.send((node_name.to_string(), NodeStatus::Failed(error_msg))).await.ok();
            Err(e)
        }
    }
}

async fn run_deploy(
    deploy_flakes: Vec<deploy::DeployFlake<'_>>,
    data: Vec<deploy::data::Data>,
    supports_flakes: bool,
    check_sigs: bool,
    _interactive: bool,
    cmd_overrides: &deploy::CmdOverrides,
    keep_result: bool,
    result_path: Option<&str>,
    extra_build_args: &[String],
    debug_logs: bool,
    _dry_activate: bool,
    _boot: bool,
    log_dir: &Option<String>,
    rollback_succeeded: bool,
) -> Result<(), RunDeployError> {
    let mut tui = tui::Tui::new().map_err(|e| RunDeployError::BuildError(e.to_string()))?;
    let status_tx = tui.status_sender();
    let _log_sender = tui.log_sender(); // renamed from log_sender
    let progress_tx = tui.progress_sender();
    
    // Set up UI with nodes
    for (_deploy_flake, data) in deploy_flakes.iter().zip(&data) {
        for (node_name, _) in &data.nodes {
            tui.add_node(node_name.clone());
        }
    }
    // Evaluate and build jobs in parallel
    let mut jobs = HashMap::new();
    for (deploy_flake, data_item) in deploy_flakes.iter().zip(&data) {
        let system = std::env::consts::ARCH;
        let eval_result = evaluate_jobs(deploy_flake.repo, system).await?;
        jobs.extend(eval_result.jobs);
        
        // Update cache hits for all nodes in this deployment
        for node_name in data_item.nodes.keys() {
            tui.update_cache_hit(node_name, eval_result.cache_hit);
        }
    }

    let drv_paths: Vec<String> = jobs.values().map(|j| j.drv_path.clone()).collect();
    build_jobs(&drv_paths, num_cpus::get()).await?;

    // Create a channel for activation coordination
    let (activation_tx, mut activation_rx) = mpsc::channel::<String>(100);
    let activation_tx_clone = activation_tx.clone();

    // Start UI task after evaluation and before deployments
    let ui_handle = task::spawn(async move {
        let mut active_nodes = HashMap::new();
        
        loop {
            tokio::select! {
                Some(node_name) = activation_rx.recv() => {
                    tui.set_activation_start(&node_name);
                    active_nodes.insert(node_name, std::time::Instant::now());
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let now = std::time::Instant::now();
                    active_nodes.retain(|node_name, start_time| {
                        if now.duration_since(*start_time).as_secs() > 300 {
                            tui.update_status(node_name, NodeStatus::Failed("Activation timeout".to_string()));
                            false
                        } else {
                            true
                        }
                    });
                }
            }

            if active_nodes.is_empty() {
                break;
            }
        }

        tui.run().await
    });

    // Deploy in parallel
    let mut futures: Vec<tokio::task::JoinHandle<Result<DeployTaskResult, RunDeployError>>> = Vec::new();
    
    for (deploy_flake, data_item) in deploy_flakes.iter().zip(&data) {
        for (node_name, node) in &data_item.nodes {
            let deploy_generic_settings = data_item.generic_settings.clone();
            let deploy_node_data = node.clone();
            let deploy_profile_data = node.node_settings.profiles[node_name.as_str()].clone();
            let deploy_cmd_overrides = (*cmd_overrides).clone();
            let repo = deploy_flake.repo.to_string();
            let extra_args = extra_build_args.to_vec();
            let result_path_owned = result_path.map(|s| s.to_string());
            let log_dir_owned = log_dir.as_ref().map(|s| s.to_string());
            let status_sender = status_tx.clone();
            let log_sender = _log_sender.clone();
            let progress_sender = progress_tx.clone();
            let node_name = node_name.clone();
            let debug_logs = debug_logs;

            let deploy_task = async move {
                let generic_settings = deploy_generic_settings;
                let node_data = deploy_node_data;
                let profile_data = deploy_profile_data;
                let cmd_overrides = deploy_cmd_overrides;

                let deploy_data = deploy::make_deploy_data(
                    &generic_settings,
                    &node_data,
                    &node_name,
                    &profile_data,
                    &node_name,
                    &cmd_overrides,
                    debug_logs,
                    log_dir_owned.as_deref(),
                );
                let deploy_defs = match deploy_data.defs() {
                    Ok(defs) => defs,
                    Err(e) => {
                        let error_msg = format!("Failed to prepare deployment: {}", e);
                        log_sender.send((node_name.clone(), error_msg.clone())).await.ok();
                        status_sender.send((node_name.clone(), NodeStatus::Failed(error_msg))).await.ok();
                        return Ok(DeployTaskResult {
                            node: node_name,
                            push_result: Err(deploy::push::PushProfileError::BuildExit(None)),
                            generic_settings,
                            node_data,
                            profile_data,
                            cmd_overrides,
                            log_dir: log_dir_owned,
                            status_sender,
                            progress_sender,
                            log_sender,
                            debug_logs,
                        });
                    }
                };

                status_sender.send((node_name.clone(), NodeStatus::Building)).await.ok();
                progress_sender.send((node_name.clone(), 0.2)).await.ok();

                status_sender.send((node_name.clone(), NodeStatus::Pushing)).await.ok();
                progress_sender.send((node_name.clone(), 0.4)).await.ok();

                log_sender.send((node_name.clone(), format!("Starting parallel deployment to {}", node_name))).await.ok();

                let push_result = deploy::push::push_profile(PushProfileData {
                    supports_flakes,
                    check_sigs,
                    repo: &repo,
                    deploy_data: &deploy_data,
                    deploy_defs: &deploy_defs,
                    keep_result,
                    result_path: result_path_owned.as_deref(),
                    extra_build_args: &extra_args,
                    status_sender: Some(status_sender.clone()),
                    log_sender: Some(log_sender.clone()),
                }).await;

                if push_result.is_ok() {
                    progress_sender.send((node_name.clone(), 1.0)).await.ok();
                } else {
                    progress_sender.send((node_name.clone(), 0.0)).await.ok();
                    status_sender.send((node_name.clone(), NodeStatus::Failed(push_result.as_ref().err().unwrap().to_string()))).await.ok();
                    log_sender.send((node_name.clone(), format!("Deployment failed: {}.", push_result.as_ref().err().unwrap()))).await.ok();
                }

                Ok(DeployTaskResult {
                    node: node_name,
                    push_result,
                    generic_settings,
                    node_data,
                    profile_data,
                    cmd_overrides,
                    log_dir: log_dir_owned,
                    status_sender,
                    progress_sender,
                    log_sender,
                    debug_logs,
                })
            };

            futures.push(tokio::spawn(deploy_task));
        }
    }

    for future in futures {
        match future.await {
            Ok(result) => {
                let result = result?;
                if result.push_result.is_ok() {
                    result.status_sender.send((result.node.clone(), NodeStatus::Activating)).await.ok();
                    result.progress_sender.send((result.node.clone(), 0.8)).await.ok();
                    result.log_sender.send((result.node.clone(), "Starting activation phase".to_string())).await.ok();
                    activation_tx_clone.send(result.node.clone()).await.ok();
                    result.status_sender.send((result.node.clone(), NodeStatus::Done)).await.ok();
                    result.progress_sender.send((result.node.clone(), 1.0)).await.ok();
                    result.log_sender.send((result.node.clone(), "Deployment completed successfully".to_string())).await.ok();
                } else if rollback_succeeded {
                    let deploy_data = deploy::make_deploy_data(
                        &result.generic_settings,
                        &result.node_data,
                        &result.node,
                        &result.profile_data,
                        &result.node,
                        &result.cmd_overrides,
                        result.debug_logs,
                        result.log_dir.as_deref()
                    );
                    match deploy_data.defs() {
                        Ok(deploy_defs) => {
                            let _ = handle_rollback(&deploy_data, &deploy_defs, &result.node, &result.log_sender, &result.status_sender).await;
                        }
                        Err(e) => {
                            let error_msg = format!("Failed to prepare rollback: {}", e);
                            result.log_sender.send((result.node.clone(), error_msg.clone())).await.ok();
                            result.status_sender.send((result.node.clone(), NodeStatus::Failed(error_msg))).await.ok();
                        }
                    }
                }
            }
            Err(e) => {
                error!("Task join error: {}", e);
                return Err(RunDeployError::TaskJoin(e));
            }
        }
    }

    ui_handle.await?.map_err(RunDeployError::from)?;

    Ok(())
}

#[derive(Error, Debug)]
pub enum RunError {
    #[error("Failed to deploy profile: {0}")]
    DeployProfile(#[from] deploy::deploy::DeployProfileError),
    #[error("Failed to push profile: {0}")]
    PushProfile(#[from] deploy::push::PushProfileError),
    #[error("Failed to test for flake support: {0}")]
    FlakeTest(std::io::Error),
    #[error("Failed to check deployment: {0}")]
    CheckDeployment(#[from] CheckDeploymentError),
    #[error("Failed to evaluate deployment data: {0}")]
    GetDeploymentData(#[from] GetDeploymentDataError),
    #[error("Error parsing flake: {0}")]
    ParseFlake(#[from] deploy::ParseFlakeError),
    #[error("Error initiating logger: {0}")]
    Logger(#[from] flexi_logger::FlexiLoggerError),
    #[error("{0}")]
    RunDeploy(#[from] RunDeployError),
    #[error("Clap error: {0}")]
    Clap(clap::Error),
}

impl From<clap::Error> for RunError {
    fn from(err: clap::Error) -> Self {
        RunError::Clap(err)
    }
}

pub async fn run(args: Option<&ArgMatches>) -> Result<(), RunError> {
    let opts = match args {
        Some(o) => <Opts as FromArgMatches>::from_arg_matches(o)?,
        None => Opts::parse(),
    };

    deploy::init_logger(
        opts.debug_logs,
        opts.log_dir.as_deref(),
        &deploy::LoggerType::Deploy,
    )?;

    if opts.dry_activate && opts.boot {
        error!("Cannot use both --dry-activate & --boot!");
    }

    let deploys = opts
        .clone()
        .targets
        .unwrap_or_else(|| vec![opts.clone().target.unwrap_or_else(|| ".".to_string())]);

    let deploy_flakes: Vec<DeployFlake> = deploys
        .iter()
        .map(|f| deploy::parse_flake(f.as_str()))
        .collect::<Result<Vec<DeployFlake>, ParseFlakeError>>()?;

    let cmd_overrides = deploy::CmdOverrides {
        ssh_user: opts.ssh_user,
        profile_user: opts.profile_user,
        ssh_opts: opts.ssh_opts,
        fast_connection: opts.fast_connection,
        auto_rollback: opts.auto_rollback,
        hostname: opts.hostname,
        magic_rollback: opts.magic_rollback,
        temp_path: opts.temp_path,
        confirm_timeout: opts.confirm_timeout,
        activation_timeout: opts.activation_timeout,
        dry_activate: opts.dry_activate,
        remote_build: opts.remote_build,
        sudo: opts.sudo,
        interactive_sudo: opts.interactive_sudo
    };

    let supports_flakes = test_flake_support().await.map_err(RunError::FlakeTest)?;

    if !supports_flakes {
        warn!("A Nix version without flakes support was detected, support for this is work in progress");
    }

    if !opts.skip_checks {
        for deploy_flake in &deploy_flakes {
            check_deployment(supports_flakes, deploy_flake.repo, &opts.extra_build_args).await?;
        }
    }
    let result_path = opts.result_path.as_deref();
    let data = get_deployment_data(supports_flakes, &deploy_flakes, &opts.extra_build_args).await?;
    run_deploy(
        deploy_flakes,
        data,
        supports_flakes,
        opts.checksigs,
        opts.interactive,
        &cmd_overrides,
        opts.keep_result,
        result_path,
        &opts.extra_build_args,
        opts.debug_logs,
        opts.dry_activate,
        opts.boot,
        &opts.log_dir,
        opts.rollback_succeeded.unwrap_or(true),
    )
    .await?;

    Ok(())
}
