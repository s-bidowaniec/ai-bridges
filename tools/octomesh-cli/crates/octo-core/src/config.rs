use crate::error::{OctoError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OctoConfig {
    #[serde(default)]
    pub mesh: MeshConfig,
    #[serde(default)]
    pub roles: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub peers: HashMap<String, PeerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_cooldown_secs")]
    pub default_cooldown_secs: u64,
    #[serde(default = "default_state_db")]
    pub state_db: PathBuf,
    #[serde(default = "default_python_runner")]
    pub python_runner: String,
    #[serde(default = "default_python_runner_args")]
    pub python_runner_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub adapter: String,
    #[serde(default)]
    pub command: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    pub cooldown_secs: Option<u64>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_max_depth() -> u32 {
    2
}

fn default_cooldown_secs() -> u64 {
    300
}

fn default_timeout_secs() -> u64 {
    120
}

fn default_state_db() -> PathBuf {
    PathBuf::from(".octomesh/state.db")
}

fn default_python_runner() -> String {
    "uv".to_string()
}

fn default_python_runner_args() -> Vec<String> {
    vec![
        "run".to_string(),
        "--project".to_string(),
        "adapters".to_string(),
        "python".to_string(),
        "-m".to_string(),
        "octomesh_adapters.runner".to_string(),
    ]
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            max_depth: default_max_depth(),
            default_cooldown_secs: default_cooldown_secs(),
            state_db: default_state_db(),
            python_runner: default_python_runner(),
            python_runner_args: default_python_runner_args(),
        }
    }
}

impl OctoConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = path.as_ref();
        let raw = fs::read_to_string(path_ref).map_err(|e| {
            OctoError::Config(format!("Failed to read '{}': {}", path_ref.display(), e))
        })?;
        let cfg: OctoConfig = toml::from_str(&raw).map_err(|e| {
            OctoError::Config(format!("Invalid TOML in '{}': {}", path_ref.display(), e))
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.peers.is_empty() {
            return Err(OctoError::Config(
                "At least one peer must be configured under [peers.*]".to_string(),
            ));
        }
        for (role, chain) in &self.roles {
            if chain.is_empty() {
                return Err(OctoError::Config(format!(
                    "Role '{}' must define at least one peer",
                    role
                )));
            }
            for peer_name in chain {
                if !self.peers.contains_key(peer_name) {
                    return Err(OctoError::Config(format!(
                        "Role '{}' references unknown peer '{}'",
                        role, peer_name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Resolves a target string into an ordered list of candidate peer names.
    /// If `target` matches a configured role, returns its failover chain.
    /// Otherwise if `target` matches a single configured peer, returns `[target]`.
    pub fn resolve_candidates(&self, target: &str) -> Result<Vec<String>> {
        if let Some(chain) = self.roles.get(target) {
            return Ok(chain.clone());
        }
        if self.peers.contains_key(target) {
            return Ok(vec![target.to_string()]);
        }
        Err(OctoError::UnknownTarget(target.to_string()))
    }

    pub fn effective_cooldown(&self, peer_name: &str) -> u64 {
        self.peers
            .get(peer_name)
            .and_then(|p| p.cooldown_secs)
            .unwrap_or(self.mesh.default_cooldown_secs)
    }
}
