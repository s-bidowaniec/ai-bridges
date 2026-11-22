use octo_core::{
    AskRequest, Blackboard, MeshConfig, MeshRouter, OctoConfig, OctoError, PeerConfig, PeerHealth,
};
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::tempdir;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn build_test_config(state_db: PathBuf) -> OctoConfig {
    let mut peers = HashMap::new();

    peers.insert(
        "mock-primary".to_string(),
        PeerConfig {
            adapter: "mock".to_string(),
            command: "mock".to_string(),
            timeout_secs: 10,
            cooldown_secs: Some(120),
            extra_args: vec!["--simulate-rate-limit-on-keyword".to_string()],
            ..Default::default()
        },
    );

    peers.insert(
        "mock-fallback".to_string(),
        PeerConfig {
            adapter: "mock".to_string(),
            command: "mock".to_string(),
            timeout_secs: 10,
            cooldown_secs: Some(60),
            extra_args: vec![],
            ..Default::default()
        },
    );

    let mut roles = HashMap::new();
    roles.insert(
        "coder".to_string(),
        vec!["mock-primary".to_string(), "mock-fallback".to_string()],
    );

    OctoConfig {
        mesh: MeshConfig {
            max_depth: 2,
            default_cooldown_secs: 300,
            state_db,
            python_runner: "python3".to_string(),
            python_runner_args: vec!["-m".to_string(), "octomesh_adapters.runner".to_string()],
        },
        roles,
        peers,
    }
}

#[tokio::test]
async fn test_automatic_failover_on_rate_limit() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("state.db");
    let cfg = build_test_config(db_path.clone());
    let board = Blackboard::open(&db_path).unwrap();

    let adapters_path = repo_root().join("adapters");
    std::env::set_var("PYTHONPATH", &adapters_path);

    let router = MeshRouter::new(cfg, board.clone(), repo_root());

    let resp = router
        .ask(AskRequest {
            target: "coder".to_string(),
            prompt: "Please trigger rate_limit and verify fallback".to_string(),
            caller: Some("orchestrator".to_string()),
            depth: 0,
            include_diff: false,
            readonly: true,
        })
        .await
        .expect("Failover to mock-fallback should succeed");

    assert_eq!(resp.resolved_peer, "mock-fallback");
    assert!(resp.output.contains("[mock:mock-fallback]"));
    assert_eq!(resp.attempts.len(), 2);
    assert_eq!(resp.attempts[0].peer, "mock-primary");
    assert_eq!(resp.attempts[0].outcome, "rate_limited");
    assert_eq!(resp.attempts[1].peer, "mock-fallback");
    assert_eq!(resp.attempts[1].outcome, "ok");

    let primary_state = board.get_peer("mock-primary").unwrap();
    assert_eq!(primary_state.health, PeerHealth::RateLimited);
    assert!(primary_state.cooldown_until.is_some());
}

#[tokio::test]
async fn test_anti_loop_guard_rejects_excess_depth_and_self_call() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("state.db");
    let cfg = build_test_config(db_path.clone());
    let board = Blackboard::open(&db_path).unwrap();
    let router = MeshRouter::new(cfg, board, repo_root());

    let err_depth = router
        .ask(AskRequest {
            target: "coder".to_string(),
            prompt: "Deep call".to_string(),
            caller: Some("claude".to_string()),
            depth: 3,
            include_diff: false,
            readonly: false,
        })
        .await
        .unwrap_err();

    assert!(matches!(err_depth, OctoError::LoopGuard(_)));

    let err_self = router
        .ask(AskRequest {
            target: "mock-primary".to_string(),
            prompt: "Self call".to_string(),
            caller: Some("mock-primary".to_string()),
            depth: 1,
            include_diff: false,
            readonly: false,
        })
        .await
        .unwrap_err();

    assert!(matches!(err_self, OctoError::LoopGuard(_)));
}
