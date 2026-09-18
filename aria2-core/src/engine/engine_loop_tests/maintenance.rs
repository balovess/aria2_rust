use super::*;

#[tokio::test]
async fn server_stat_interval_persists_through_engine_maintenance() {
    let dir = tempfile::tempdir().expect("server-stat output directory");
    let path = dir.path().join("server-stat.json");
    let mut ctx = test_ctx(false);
    ctx.server_stat_man = Arc::new(ServerStatMan::new());
    ctx.server_stat_man
        .update_with_protocol("interval.example", "http", 1024, false);
    ctx.server_stat_save_path = Some(path.clone());
    ctx.server_stat_save_interval = Some(Duration::from_secs(60));
    ctx.server_stat_next_save = Some(Instant::now() - Duration::from_secs(1));

    run_deadline_maintenance(&mut ctx, &mut []).await;

    let content = tokio::fs::read_to_string(&path)
        .await
        .expect("server-stat interval must write its configured output");
    assert!(content.contains("interval.example"));
    assert!(
        ctx.server_stat_next_save
            .is_some_and(|deadline| deadline > Instant::now()),
        "server-stat interval must schedule its next save"
    );
}

#[tokio::test]
async fn server_stat_timeout_controls_stale_cleanup() {
    let dir = tempfile::tempdir().expect("server-stat input directory");
    let path = dir.path().join("stale-server-stat.json");
    let file = crate::selector::server_stat_man::ServerStatFile {
        version: "1.0".to_string(),
        saved_at: 1,
        servers: vec![crate::selector::server_stat::ServerStatSnapshot {
            host: "stale.example".to_string(),
            protocol: "http".to_string(),
            download_speed: 1,
            single_connection_avg_speed: 1,
            multi_connection_avg_speed: 0,
            last_updated: 1,
            status: 0,
            counter: 1,
            last_error_time: None,
            last_error_code: 0,
            consecutive_failures: 0,
        }],
    };
    tokio::fs::write(&path, serde_json::to_vec(&file).unwrap())
        .await
        .expect("write stale server-stat fixture");

    let mut ctx = test_ctx(false);
    ctx.server_stat_man = Arc::new(ServerStatMan::new());
    ctx.server_stat_man
        .load_from_file_async(&path)
        .await
        .expect("load stale server-stat fixture");
    ctx.server_stat_max_age = Some(Duration::from_secs(1));
    assert_eq!(ctx.server_stat_man.count(), 1);

    run_event_cleanup(&ctx).await;

    assert_eq!(
        ctx.server_stat_man.count(),
        0,
        "configured server-stat timeout must remove stale entries"
    );
}
