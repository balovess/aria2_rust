use aria2_core::config::{ConfigManager, OptionValue};
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::request::request_group_man::RequestGroupMan;

#[tokio::main]
async fn main() {
    let mut config = ConfigManager::new();

    config
        .set_global_option("dir", OptionValue::Str("./downloads".into()))
        .await
        .unwrap();
    config
        .set_global_option("split", OptionValue::Int(4))
        .await
        .unwrap();

    let uri = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("用法: cargo run --example simple_download -- <URL>");
        std::process::exit(1);
    });

    println!("下载: {}", uri);
    println!(
        "保存到: {}",
        config.get_global_str("dir").await.unwrap_or_default()
    );
    println!(
        "连接数: {}",
        config.get_global_i64("split").await.unwrap_or(1)
    );

    let gids = add_download(&config, &uri).await;
    for gid in &gids {
        println!("任务 #{} 已创建", gid);
    }

    let all_opts = config.get_all_global_options().await;
    println!("\n当前配置:");
    for (key, value) in &all_opts {
        if !matches!(value, OptionValue::None) {
            println!("  {} = {}", key, value);
        }
    }
}

async fn add_download(config: &ConfigManager, uri: &str) -> Vec<u64> {
    let opts = config
        .create_task_config(std::collections::HashMap::new())
        .await;

    let split = opts.get("split").and_then(|v| v.as_i64()).unwrap_or(1) as u16;
    let dir = opts
        .get("dir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let man = RequestGroupMan::new();
    let download_opts = DownloadOptions {
        split: Some(split),
        max_connection_per_server: None,
        max_download_limit: None,
        max_upload_limit: None,
        dir,
        out: None,
        seed_time: None,
        seed_ratio: None,
        checksum: None,
        cookie_file: None,
        cookies: None,
        bt_force_encrypt: false,
        bt_require_crypto: false,
        enable_dht: true,
        dht_listen_port: None,
        enable_public_trackers: true,
        bt_piece_selection_strategy: "rarest-first".to_string(),
        bt_endgame_threshold: 20,
        max_retries: 3,
        retry_wait: 1,
        http_proxy: None,
        dht_file_path: None,
        bt_max_upload_slots: None,
        bt_optimistic_unchoke_interval: None,
        bt_snubbed_timeout: None,
    };

    match man.add_group(vec![uri.to_string()], download_opts).await {
        Ok(gid) => vec![gid.value()],
        Err(e) => {
            eprintln!("添加任务失败: {}", e);
            vec![]
        }
    }
}
