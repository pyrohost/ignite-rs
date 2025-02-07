use serde::{Deserialize, Serialize};
use tokio::process::Command;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JobInfo {
    pub system: String,
    pub drv_path: String,  // Fix the field name to match Rust naming conventions
    #[serde(rename = "outputs")]
    pub out_paths: HashMap<String, String>,
}

#[derive(Error, Debug)]
pub enum BuildError {
    #[error("Failed to run nix-eval-jobs: {0}")]
    EvalError(std::io::Error),
    #[error("nix-eval-jobs exited with error: {0:?}")]
    EvalExitError(Option<i32>),
    #[error("Failed to parse nix-eval-jobs output: {0}")]
    ParseError(serde_json::Error),
    #[error("Build failed: {0}")]
    BuildError(std::io::Error),
    #[error("Build failed with output: {0}")]
    BuildOutputError(String),
    #[error("Build exited with error: {0:?}")] 
    BuildExitError(Option<i32>),
}

#[derive(Debug)]
pub struct EvalResult {
    pub jobs: HashMap<String, JobInfo>,
    pub cache_hit: bool,
}

pub async fn evaluate_jobs(
    flake: &str,
    system: &str,
) -> Result<EvalResult, BuildError> {
    let output = Command::new("nix-eval-jobs")
        .arg("--jobs")
        .arg(format!("{}#deploy", flake))
        .arg("--json")
        .arg("--system")
        .arg(system)
        .arg("--eval-caching")
        .arg("--use-eval-cache")
        .arg("--builders")
        .arg("@/etc/nix/machines")
        .arg("--max-memory-size")
        .arg("8G")
        .arg("--eval-cache-dir")
        .arg("--builders-use-substitutes")
        .arg("/var/cache/nix/eval-cache")
        .output()
        .await
        .map_err(BuildError::EvalError)?;

    if !output.status.success() {
        return Err(BuildError::EvalExitError(output.status.code()));
    }

    let jobs: HashMap<String, JobInfo> = serde_json::from_slice(&output.stdout)
        .map_err(BuildError::ParseError)?;

    // Check if any job has cached outputs
    let cache_hit = jobs.values().any(|job| {
        !job.out_paths.is_empty()
    });

    Ok(EvalResult { jobs, cache_hit })
}

pub async fn build_jobs(
    jobs: &[String],
    max_jobs: usize,
) -> Result<(), BuildError> {
    // Split jobs into chunks for better parallelization
    let mut cmd = Command::new("nix");
    cmd.arg("build")
        .args(jobs)
        .arg("-L") // Show build logs
        .arg("--max-jobs")
        .arg(max_jobs.to_string())
        .arg("--parallel-builds")
        .arg(max_jobs.to_string())
        .arg("--cores")
        .arg(max_jobs.to_string())
        .arg("--keep-going") // Continue building other derivations if one fails
        .arg("--option")
        .arg("connect-timeout")
        .arg("10") // Increased timeout for better reliability
        .arg("--option")
        .arg("substitute")
        .arg("true") // Enable binary cache substitution
        .arg("--option")
        .arg("builders-use-substitutes")
        .arg("true") // Allow builders to use binary cache
        .arg("--option")
        .arg("build-fallback")
        .arg("true"); // Try alternative builders if one fails

    let output = cmd.output()
        .await
        .map_err(BuildError::BuildError)?;

    if !output.status.success() {
        // Capture error output for better debugging
        let error_output = String::from_utf8_lossy(&output.stderr);
        if !error_output.is_empty() {
            return Err(BuildError::BuildOutputError(error_output.to_string()));
        }
        return Err(BuildError::BuildExitError(output.status.code()));
    }

    // Verify all outputs exist
    for job in jobs {
        let output_path = Command::new("nix")
            .arg("path-info")
            .arg(job)
            .output()
            .await
            .map_err(BuildError::BuildError)?;

        if !output_path.status.success() {
            return Err(BuildError::BuildOutputError(format!("Failed to verify output for {}", job)));
        }
    }

    Ok(())
}
