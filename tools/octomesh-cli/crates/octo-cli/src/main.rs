use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use octo_core::mcp::serve_stdio;
use octo_core::{AskRequest, Blackboard, MeshRouter, OctoConfig};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG_TOML: &str = include_str!("../../../octomesh.toml.example");
const DEFAULT_PROTOCOL_MD: &str = include_str!("../../../templates/PROTOCOL.md");

#[derive(Parser)]
#[command(
    name = "octo",
    version,
    about = "Hybrid P2P & failover mesh bridge for native terminal AI agents"
)]
struct Cli {
    /// Path to octomesh.toml configuration file
    #[arg(short, long, default_value = "octomesh.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold octomesh.toml, .octomesh/PROTOCOL.md, and optionally sync AGENTS.md / CLAUDE.md
    Init {
        /// Overwrite existing octomesh.toml if present
        #[arg(long)]
        force: bool,

        /// Append OctoMesh protocol instructions to AGENTS.md and CLAUDE.md
        #[arg(long)]
        sync_agents: bool,
    },

    /// Display current peer health, active cooldowns, and configured role chains
    Status,

    /// Route a prompt to a peer or role chain with automatic failover
    Ask {
        /// Target peer name (e.g. 'claude', 'vibe') or role (e.g. 'coder', 'reviewer')
        target: String,

        /// Task instructions or question
        prompt: String,

        /// Include truncated git status and git diff context
        #[arg(long)]
        include_diff: bool,

        /// Run target agent in read-only advisory/review mode
        #[arg(long)]
        readonly: bool,

        /// Optional caller identifier for anti-loop tracking
        #[arg(long)]
        caller: Option<String>,
    },

    /// Start local MCP server over stdio (JSON-RPC 2.0; logs strictly to stderr)
    McpServe,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Strictly direct all logs to stderr so `octo mcp-serve` never pollutes stdout JSON-RPC frames
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let workdir = std::env::current_dir().context("Failed to resolve current directory")?;

    match cli.command {
        Commands::Init { force, sync_agents } => {
            cmd_init(&workdir, &cli.config, force, sync_agents)?;
        }
        Commands::Status => {
            cmd_status(&workdir, &cli.config)?;
        }
        Commands::Ask {
            target,
            prompt,
            include_diff,
            readonly,
            caller,
        } => {
            cmd_ask(
                &workdir,
                &cli.config,
                target,
                prompt,
                include_diff,
                readonly,
                caller,
            )
            .await?;
        }
        Commands::McpServe => {
            cmd_mcp_serve(&workdir, &cli.config).await?;
        }
    }

    Ok(())
}

fn cmd_init(workdir: &Path, config_path: &Path, force: bool, sync_agents: bool) -> Result<()> {
    let target_cfg = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        workdir.join(config_path)
    };

    if target_cfg.exists() && !force {
        println!(
            "Config '{}' already exists (use --force to overwrite).",
            target_cfg.display()
        );
    } else {
        fs::write(&target_cfg, DEFAULT_CONFIG_TOML)?;
        println!("Created {}", target_cfg.display());
    }

    let octo_dir = workdir.join(".octomesh");
    fs::create_dir_all(&octo_dir)?;
    let protocol_path = octo_dir.join("PROTOCOL.md");
    fs::write(&protocol_path, DEFAULT_PROTOCOL_MD)?;
    println!("Created {}", protocol_path.display());

    // Initialize SQLite blackboard
    let cfg = OctoConfig::load_from_path(&target_cfg)?;
    let db_path = if cfg.mesh.state_db.is_absolute() {
        cfg.mesh.state_db.clone()
    } else {
        workdir.join(&cfg.mesh.state_db)
    };
    let board = Blackboard::open(&db_path)?;
    let peer_names: Vec<String> = cfg.peers.keys().cloned().collect();
    board.ensure_peers(&peer_names)?;
    println!("Initialized SQLite blackboard at {}", db_path.display());

    if sync_agents {
        let marker = "<!-- OCTOMESH_PROTOCOL -->";
        let snippet = format!(
            "\n{}\n## OctoMesh P2P Collaboration\nSee `.octomesh/PROTOCOL.md` for MCP mesh delegation rules (`mesh_status`, `mesh_ask`, `mesh_handoff`).\n",
            marker
        );
        for doc in ["AGENTS.md", "CLAUDE.md"] {
            let doc_path = workdir.join(doc);
            let existing = fs::read_to_string(&doc_path).unwrap_or_default();
            if !existing.contains(marker) {
                let mut f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&doc_path)?;
                f.write_all(snippet.as_bytes())?;
                println!("Synced mesh instructions into {}", doc_path.display());
            } else {
                println!(
                    "{} already contains OctoMesh instructions.",
                    doc_path.display()
                );
            }
        }
    }

    Ok(())
}

fn load_router(workdir: &Path, config_path: &Path) -> Result<MeshRouter> {
    let resolved_cfg = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        workdir.join(config_path)
    };
    let cfg = OctoConfig::load_from_path(&resolved_cfg)?;
    let db_path = if cfg.mesh.state_db.is_absolute() {
        cfg.mesh.state_db.clone()
    } else {
        workdir.join(&cfg.mesh.state_db)
    };
    let board = Blackboard::open(&db_path)?;
    Ok(MeshRouter::new(cfg, board, workdir))
}

fn cmd_status(workdir: &Path, config_path: &Path) -> Result<()> {
    let router = load_router(workdir, config_path)?;
    let mut peer_names: Vec<String> = router.config().peers.keys().cloned().collect();
    peer_names.sort();
    let records = router.blackboard().list_peers(&peer_names)?;

    println!(
        "=== OctoMesh Peers (max_depth={}) ===",
        router.config().mesh.max_depth
    );
    for rec in records {
        let cooldown = rec
            .cooldown_until
            .map(|dt| format!("until {}", dt.to_rfc3339()))
            .unwrap_or_else(|| "-".to_string());
        let err = rec.last_error.unwrap_or_default();
        println!(
            "  {:<14} status={:<12} cooldown={:<28} {}",
            rec.peer,
            rec.health.as_str(),
            cooldown,
            err
        );
    }

    println!("\n=== Role Failover Chains ===");
    let mut roles: Vec<(&String, &Vec<String>)> = router.config().roles.iter().collect();
    roles.sort_by_key(|(k, _)| *k);
    for (role, chain) in roles {
        println!("  {:<14} -> {}", role, chain.join(" -> "));
    }

    Ok(())
}

async fn cmd_ask(
    workdir: &Path,
    config_path: &Path,
    target: String,
    prompt: String,
    include_diff: bool,
    readonly: bool,
    caller: Option<String>,
) -> Result<()> {
    let router = load_router(workdir, config_path)?;
    let resp = router
        .ask(AskRequest {
            target,
            prompt,
            caller,
            depth: 1,
            include_diff,
            readonly,
        })
        .await?;

    eprintln!("[octo] resolved_peer={}", resp.resolved_peer);
    println!("{}", resp.output);
    Ok(())
}

async fn cmd_mcp_serve(workdir: &Path, config_path: &Path) -> Result<()> {
    let router = Arc::new(load_router(workdir, config_path)?);
    serve_stdio(router).await
}
