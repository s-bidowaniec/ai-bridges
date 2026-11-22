use crate::config::{MeshConfig, PeerConfig};
use crate::error::{OctoError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize)]
pub struct AdapterPayload {
    pub adapter: String,
    pub peer: String,
    pub command: String,
    pub prompt: String,
    pub include_diff: bool,
    pub readonly: bool,
    pub timeout_secs: u64,
    pub extra_args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AdapterErrorCode {
    Ok,
    RateLimited,
    Timeout,
    AuthError,
    CliNotFound,
    ExecError,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdapterResult {
    pub status: AdapterErrorCode,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub peer: String,
    #[serde(default)]
    pub latency_ms: Option<u64>,
}

pub async fn invoke_python_adapter(
    mesh: &MeshConfig,
    peer_name: &str,
    peer_cfg: &PeerConfig,
    prompt: &str,
    include_diff: bool,
    readonly: bool,
    workdir: &Path,
) -> Result<AdapterResult> {
    let payload = AdapterPayload {
        adapter: peer_cfg.adapter.clone(),
        peer: peer_name.to_string(),
        command: peer_cfg.command.clone(),
        prompt: prompt.to_string(),
        include_diff,
        readonly,
        timeout_secs: peer_cfg.timeout_secs,
        extra_args: peer_cfg.extra_args.clone(),
        env: peer_cfg.env.clone(),
        cwd: workdir.to_string_lossy().to_string(),
    };

    let stdin_json = serde_json::to_vec(&payload)?;

    let mut cmd = Command::new(&mesh.python_runner);
    cmd.args(&mesh.python_runner_args)
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    debug!(
        peer = peer_name,
        adapter = %peer_cfg.adapter,
        runner = %mesh.python_runner,
        "Spawning Python adapter subprocess"
    );

    let mut child = cmd.spawn().map_err(|e| {
        OctoError::AdapterFailure(format!(
            "Failed to spawn python runner '{}': {}",
            mesh.python_runner, e
        ))
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&stdin_json).await?;
        stdin.shutdown().await?;
    }

    let wait_budget = Duration::from_secs(peer_cfg.timeout_secs.saturating_add(5));
    let output = match timeout(wait_budget, child.wait_with_output()).await {
        Ok(res) => res?,
        Err(_) => {
            warn!(peer = peer_name, "Subprocess exceeded hard timeout budget");
            return Ok(AdapterResult {
                status: AdapterErrorCode::Timeout,
                output: String::new(),
                error_message: Some(format!(
                    "Hard timeout after {}s waiting for adapter '{}'",
                    wait_budget.as_secs(),
                    peer_cfg.adapter
                )),
                peer: peer_name.to_string(),
                latency_ms: Some(wait_budget.as_millis() as u64),
            });
        }
    };

    let stderr_str = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr_str.is_empty() {
        debug!(peer = peer_name, stderr = %stderr_str, "Adapter stderr output");
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout_str.is_empty() {
        return Ok(AdapterResult {
            status: AdapterErrorCode::ExecError,
            output: String::new(),
            error_message: Some(format!(
                "Adapter produced empty stdout (exit={:?}, stderr={})",
                output.status.code(),
                stderr_str
            )),
            peer: peer_name.to_string(),
            latency_ms: None,
        });
    }

    let parsed: AdapterResult = serde_json::from_str(&stdout_str).map_err(|e| {
        OctoError::AdapterFailure(format!(
            "Invalid JSON from adapter '{}': {} (raw stdout: {})",
            peer_cfg.adapter, e, stdout_str
        ))
    })?;

    Ok(parsed)
}
