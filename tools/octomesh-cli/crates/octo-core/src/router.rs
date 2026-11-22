use crate::adapter::{invoke_python_adapter, AdapterErrorCode};
use crate::blackboard::{Blackboard, PeerHealth};
use crate::config::OctoConfig;
use crate::error::{OctoError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequest {
    pub target: String,
    pub prompt: String,
    #[serde(default)]
    pub caller: Option<String>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub include_diff: bool,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub peer: String,
    pub outcome: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResponse {
    pub target: String,
    pub resolved_peer: String,
    pub output: String,
    pub attempts: Vec<AttemptRecord>,
}

pub struct MeshRouter {
    config: OctoConfig,
    blackboard: Blackboard,
    workdir: PathBuf,
}

impl MeshRouter {
    pub fn new(config: OctoConfig, blackboard: Blackboard, workdir: impl AsRef<Path>) -> Self {
        let peer_keys: Vec<String> = config.peers.keys().cloned().collect();
        let _ = blackboard.ensure_peers(&peer_keys);
        Self {
            config,
            blackboard,
            workdir: workdir.as_ref().to_path_buf(),
        }
    }

    pub fn config(&self) -> &OctoConfig {
        &self.config
    }

    pub fn blackboard(&self) -> &Blackboard {
        &self.blackboard
    }

    /// Anti-loop guard: checks delegation depth and prevents caller == target self-invocation.
    pub fn validate_guard(&self, req: &AskRequest) -> Result<()> {
        if req.depth > self.config.mesh.max_depth {
            return Err(OctoError::LoopGuard(format!(
                "Delegation depth {} exceeds configured max_depth {}",
                req.depth, self.config.mesh.max_depth
            )));
        }

        if let Some(caller) = req.caller.as_deref() {
            let caller_trimmed = caller.trim();
            if !caller_trimmed.is_empty() && caller_trimmed == req.target.trim() {
                return Err(OctoError::LoopGuard(format!(
                    "Self-invocation rejected: caller '{}' cannot target itself",
                    caller_trimmed
                )));
            }
        }

        Ok(())
    }

    pub async fn ask(&self, req: AskRequest) -> Result<AskResponse> {
        self.validate_guard(&req)?;

        let candidates = self.config.resolve_candidates(&req.target)?;
        let mut attempts = Vec::new();

        for peer_name in candidates {
            // Skip caller if caller appears inside a role chain
            if let Some(caller) = req.caller.as_deref() {
                if caller.trim() == peer_name {
                    attempts.push(AttemptRecord {
                        peer: peer_name.clone(),
                        outcome: "skipped_caller".to_string(),
                        error: Some("Peer is the requesting caller".to_string()),
                    });
                    continue;
                }
            }

            let state = self.blackboard.get_peer(&peer_name)?;
            match state.health {
                PeerHealth::RateLimited | PeerHealth::Dead => {
                    let msg = format!(
                        "Peer '{}' is {} until {:?}",
                        peer_name,
                        state.health.as_str(),
                        state.cooldown_until
                    );
                    info!(peer = %peer_name, "Skipping peer on cooldown");
                    attempts.push(AttemptRecord {
                        peer: peer_name,
                        outcome: format!("skipped_{}", state.health.as_str()),
                        error: Some(msg),
                    });
                    continue;
                }
                PeerHealth::Busy => {
                    info!(peer = %peer_name, "Skipping busy peer");
                    attempts.push(AttemptRecord {
                        peer: peer_name,
                        outcome: "skipped_busy".to_string(),
                        error: Some("Peer is currently busy".to_string()),
                    });
                    continue;
                }
                PeerHealth::Ready => {}
            }

            let peer_cfg = match self.config.peers.get(&peer_name) {
                Some(cfg) => cfg,
                None => continue,
            };

            self.blackboard.set_peer_busy(&peer_name)?;

            let result = invoke_python_adapter(
                &self.config.mesh,
                &peer_name,
                peer_cfg,
                &req.prompt,
                req.include_diff,
                req.readonly,
                &self.workdir,
            )
            .await;

            match result {
                Ok(res) if res.status == AdapterErrorCode::Ok => {
                    self.blackboard.set_peer_ready(&peer_name)?;
                    self.blackboard.record_task(
                        req.caller.as_deref(),
                        &req.target,
                        &peer_name,
                        req.depth,
                        "ok",
                        &req.prompt,
                    )?;
                    attempts.push(AttemptRecord {
                        peer: peer_name.clone(),
                        outcome: "ok".to_string(),
                        error: None,
                    });
                    return Ok(AskResponse {
                        target: req.target,
                        resolved_peer: peer_name,
                        output: res.output,
                        attempts,
                    });
                }
                Ok(res) => {
                    let err_msg = res
                        .error_message
                        .clone()
                        .unwrap_or_else(|| format!("{:?}", res.status));
                    let cooldown = self.config.effective_cooldown(&peer_name);

                    let next_health = match res.status {
                        AdapterErrorCode::RateLimited | AdapterErrorCode::Timeout => {
                            PeerHealth::RateLimited
                        }
                        AdapterErrorCode::AuthError
                        | AdapterErrorCode::CliNotFound
                        | AdapterErrorCode::ExecError => PeerHealth::Dead,
                        AdapterErrorCode::Ok => unreachable!(),
                    };

                    warn!(
                        peer = %peer_name,
                        status = ?res.status,
                        cooldown_secs = cooldown,
                        "Peer failed; placing on cooldown and failing over"
                    );

                    self.blackboard.set_peer_cooldown(
                        &peer_name,
                        next_health,
                        cooldown,
                        &err_msg,
                    )?;
                    self.blackboard.record_task(
                        req.caller.as_deref(),
                        &req.target,
                        &peer_name,
                        req.depth,
                        next_health.as_str(),
                        &req.prompt,
                    )?;

                    attempts.push(AttemptRecord {
                        peer: peer_name,
                        outcome: next_health.as_str().to_string(),
                        error: Some(err_msg),
                    });
                }
                Err(err) => {
                    let err_msg = err.to_string();
                    let cooldown = self.config.effective_cooldown(&peer_name);
                    self.blackboard.set_peer_cooldown(
                        &peer_name,
                        PeerHealth::Dead,
                        cooldown,
                        &err_msg,
                    )?;
                    attempts.push(AttemptRecord {
                        peer: peer_name,
                        outcome: "adapter_error".to_string(),
                        error: Some(err_msg),
                    });
                }
            }
        }

        let summary = attempts
            .iter()
            .map(|a| format!("{}: {} ({})", a.peer, a.outcome, a.error.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("; ");

        Err(OctoError::AllPeersExhausted {
            target: req.target,
            details: summary,
        })
    }
}
