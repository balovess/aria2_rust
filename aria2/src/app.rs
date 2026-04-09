use colored::Colorize;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use aria2_core::config::{ConfigManager, OptionValue};
use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
use aria2_core::engine::magnet_download_command::MagnetDownloadCommand;
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
use aria2_core::engine::sftp_download_command::SftpDownloadCommand;
use aria2_core::init_logging;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::validation::protocol_detector::{detect, DetectedInput, InputType};
use tracing::{error, info, warn, Level};

/// Top-level application runtime for aria2-rust CLI.
///
/// `App` encapsulates the complete download lifecycle:
///
/// 1. **Configuration** — `ConfigManager` with 4-source option merging
/// 2. **Engine** — `DownloadEngine` event loop for command execution
/// 3. **Request management** — `RequestGroupMan` for task lifecycle
/// 4. **UI** — Progress display, status panel, and logging
///
/// # Example
///
/// ```rust,no_run
/// use aria2::app::App;
///
/// #[tokio::main]
/// async fn main() {
///     let exit_code = App::new()
///         .run(&["--dir=/downloads", "http://example.com/file.zip".into()])
///         .await;
///     std::process::exit(exit_code);
/// }
/// ```
pub struct App {
    pub config: Arc<RwLock<ConfigManager>>,
    engine: Arc<Mutex<Option<DownloadEngine>>>,
    request_man: Arc<RwLock<RequestGroupMan>>,
    detected_inputs: Vec<DetectedInput>,
}

impl App {
    /// Create a new `App` instance with default configuration.
    pub fn new() -> Self {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let request_man = Arc::new(RwLock::new(RequestGroupMan::new()));

        Self {
            config,
            engine: Arc::new(Mutex::new(None)),
            request_man,
            detected_inputs: Vec::new(),
        }
    }

    pub async fn load_args(&mut self, args: &[String]) -> std::result::Result<(), String> {
        let mut conf = self.config.write().await;

        let mut positional_uris = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg.starts_with('-') && !arg.starts_with("--") && arg.len() == 2 {
                let c = arg.chars().nth(1).unwrap_or('\0');
                if let Some(opt_name) = self.map_short_option(c) {
                    if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                        conf.set_global_option(&opt_name, OptionValue::Str(args[i + 1].clone()))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                        i += 2;
                        continue;
                    } else {
                        conf.set_global_option(&opt_name, OptionValue::Bool(true))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                        i += 1;
                        continue;
                    }
                }
            } else if arg.starts_with("--") {
                let opt_str = &arg[2..];
                if opt_str == "help" || opt_str == "h" || opt_str == "version" || opt_str == "V" {
                    i += 1;
                    continue;
                }
                let (opt_name, value) = if let Some(eq_pos) = opt_str.find('=') {
                    (&opt_str[..eq_pos], Some(&opt_str[eq_pos + 1..]))
                } else {
                    (opt_str, None)
                };

                let actual_name = if opt_name.starts_with("no-") && opt_name.len() > 3 {
                    &opt_name[3..]
                } else {
                    opt_name
                };

                if let Some(val) = value {
                    if opt_name.starts_with("no-") {
                        conf.set_global_option(actual_name, OptionValue::Bool(false))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                    } else {
                        conf.set_global_option(actual_name, OptionValue::Str(val.to_string()))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                    }
                } else if opt_name.starts_with("no-") {
                    conf.set_global_option(actual_name, OptionValue::Bool(false))
                        .await
                        .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    conf.set_global_option(opt_name, OptionValue::Str(args[i + 1].clone()))
                        .await
                        .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                    i += 1;
                    i += 1;
                    continue;
                } else {
                    conf.set_global_option(opt_name, OptionValue::Bool(true))
                        .await
                        .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                }

                i += 1;
                continue;
            } else if arg.starts_with('@') {
                let path = &arg[1..];
                match aria2_core::config::UriListFile::from_file(path) {
                    Ok(uri_list) => {
                        for entry in uri_list.entries() {
                            for uri in &entry.uris {
                                positional_uris.push(uri.clone());
                            }
                        }
                    }
                    Err(e) => {
                        warn!("无法加载URI列表文件 {}: {}", path, e);
                    }
                }
                i += 1;
                continue;
            } else {
                positional_uris.push(arg.clone());
            }
            i += 1;
        }

        drop(conf);

        let input_file = self.get_opt_str("input-file").await;
        if let Some(path) = input_file {
            match aria2_core::config::UriListFile::from_file(&path) {
                Ok(uri_list) => {
                    for entry in uri_list.entries() {
                        for uri in &entry.uris {
                            positional_uris.push(uri.clone());
                        }
                    }
                }
                Err(e) => {
                    warn!("无法加载input-file {}: {}", path, e);
                }
            }
        }

        self.detected_inputs = positional_uris
            .into_iter()
            .filter_map(|uri| match detect(&uri) {
                Ok(d) => Some(d),
                Err(e) => {
                    warn!("无法检测输入类型 '{}': {}", uri, e);
                    None
                }
            })
            .collect();
        Ok(())
    }

    pub async fn load_env(&mut self) {
        let mut conf = self.config.write().await;
        conf.load_env().await;
    }

    pub async fn load_config_file(
        &mut self,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        let conf_path = if let Some(p) = path {
            p.to_string()
        } else {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| ".".to_string());

            let candidate = format!("{}/.aria2/aria2.conf", home);
            if std::path::Path::new(&candidate).exists() {
                candidate
            } else {
                return Ok(());
            }
        };

        let mut conf = self.config.write().await;
        conf.load_file(&conf_path).await;
        Ok(())
    }

    pub async fn initialize_engine(&self) {
        let tick_ms = self
            .get_opt_i64("bt-request-peer-timeout")
            .await
            .unwrap_or(100) as u64;
        let mut engine = DownloadEngine::new(tick_ms);

        let save_session_path = self
            .get_opt_str("save-session")
            .await
            .map(|s| std::path::PathBuf::from(s));
        let save_session_interval = self
            .get_opt_i64("save-session-interval")
            .await
            .and_then(|v| {
                if v > 0 {
                    Some(std::time::Duration::from_secs(v as u64))
                } else {
                    None
                }
            });

        if let Some(path) = save_session_path {
            engine.set_save_session(path, save_session_interval, self.request_man.clone());
        }

        *self.engine.lock().await = Some(engine);
        info!("引擎初始化完成");
    }

    pub async fn add_downloads(&self) -> std::result::Result<Vec<u64>, String> {
        if self.detected_inputs.is_empty() {
            return Err("No download inputs provided".to_string());
        }

        let dir = self.get_opt_str("dir").await;
        let out = self.get_opt_str("out").await;
        let dl_limit = self.get_opt_i64("max-download-limit").await.and_then(|v| {
            if v > 0 {
                Some(v as u64)
            } else {
                None
            }
        });
        let ul_limit = self.get_opt_i64("max-upload-limit").await.and_then(|v| {
            if v > 0 {
                Some(v as u64)
            } else {
                None
            }
        });

        let split =
            self.get_opt_i64("split")
                .await
                .and_then(|v| if v > 0 { Some(v as u16) } else { None });
        let max_conn = self
            .get_opt_i64("max-connection-per-server")
            .await
            .and_then(|v| if v > 0 { Some(v as u16) } else { None });
        let seed_time =
            self.get_opt_i64("seed-time")
                .await
                .and_then(|v| if v > 0 { Some(v as u64) } else { None });
        let seed_ratio = self
            .get_opt_str("seed-ratio")
            .await
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|&r| r > 0.0);
        let checksum = self.get_opt_str("checksum").await.and_then(|v| {
            if let Some((algo, val)) = v.split_once('=') {
                Some((algo.trim().to_string(), val.trim().to_string()))
            } else {
                None
            }
        });

        let options = DownloadOptions {
            split,
            max_connection_per_server: max_conn,
            max_download_limit: dl_limit,
            max_upload_limit: ul_limit,
            dir: dir.clone(),
            out: out.clone(),
            seed_time,
            seed_ratio,
            checksum,
            cookie_file: self.get_opt_str("load-cookies").await,
            cookies: self.get_opt_str("cookie").await,
            bt_force_encrypt: self.get_opt_bool("bt-force-encrypt").await.unwrap_or(false),
            bt_require_crypto: self
                .get_opt_bool("bt-require-crypto")
                .await
                .unwrap_or(false),
            enable_dht: self.get_opt_bool("enable-dht").await.unwrap_or(true),
            dht_listen_port: self.get_opt_i64("dht-listen-port").await.and_then(|v| {
                if v > 0 {
                    Some(v as u16)
                } else {
                    None
                }
            }),
            enable_public_trackers: self
                .get_opt_bool("enable-public-trackers")
                .await
                .unwrap_or(true),
            bt_piece_selection_strategy: self
                .get_opt_str("bt-piece-selection-strategy")
                .await
                .unwrap_or("rarest-first".to_string()),
            bt_endgame_threshold: self
                .get_opt_i64("bt-endgame-threshold")
                .await
                .and_then(|v| if v > 0 { Some(v as u32) } else { Some(20) })
                .unwrap_or(20),
            max_retries: self
                .get_opt_i64("max-retries")
                .await
                .and_then(|v| if v >= 0 { Some(v as u32) } else { Some(3) })
                .unwrap_or(3),
            retry_wait: self
                .get_opt_i64("retry-wait")
                .await
                .and_then(|v| if v > 0 { Some(v as u64) } else { Some(1) })
                .unwrap_or(1),
            http_proxy: self.get_opt_str("http-proxy").await,
            dht_file_path: self.get_opt_str("dht-file-path").await,
        };

        let mut engine_lock = self.engine.lock().await;
        let engine = engine_lock
            .as_mut()
            .ok_or_else(|| "Engine not initialized".to_string())?;

        let global_dl = self
            .get_opt_i64("max-overall-download-limit")
            .await
            .and_then(|v| if v > 0 { Some(v as u64) } else { None });
        let global_ul = self
            .get_opt_i64("max-overall-upload-limit")
            .await
            .and_then(|v| if v > 0 { Some(v as u64) } else { None });
        if global_dl.is_some() || global_ul.is_some() {
            use aria2_core::rate_limiter::RateLimiterConfig;
            engine.set_global_rate_limiter(RateLimiterConfig::new(global_dl, global_ul));
        }

        let mut gids = Vec::new();

        for (i, input) in self.detected_inputs.iter().enumerate() {
            let gid = GroupId::new(i as u64 + 1);

            let cmd: Box<dyn Command> = match &input.input_type {
                InputType::HttpUrl => Box::new(
                    DownloadCommand::new(gid, &input.raw, &options, dir.as_deref(), out.as_deref())
                        .map_err(|e| format!("HTTP download command failed: {}", e))?,
                ),
                InputType::FtpUrl => Box::new(
                    FtpDownloadCommand::new(
                        gid,
                        &input.raw,
                        &options,
                        dir.as_deref(),
                        out.as_deref(),
                    )
                    .map_err(|e| format!("FTP download command failed: {}", e))?,
                ),
                InputType::SftpUrl => Box::new(
                    SftpDownloadCommand::new(
                        gid,
                        &input.raw,
                        &options,
                        dir.as_deref(),
                        out.as_deref(),
                    )
                    .map_err(|e| format!("SFTP download command failed: {}", e))?,
                ),
                InputType::TorrentFile => {
                    let data = input
                        .file_data
                        .as_ref()
                        .ok_or_else(|| "Torrent file data not available".to_string())?;
                    Box::new(
                        BtDownloadCommand::new(gid, data, &options, dir.as_deref())
                            .map_err(|e| format!("BT download command failed: {}", e))?,
                    )
                }
                InputType::MetalinkFile => {
                    let data = input
                        .file_data
                        .as_ref()
                        .ok_or_else(|| "Metalink file data not available".to_string())?;
                    Box::new(
                        MetalinkDownloadCommand::new(gid, data, &options, dir.as_deref())
                            .map_err(|e| format!("Metalink download command failed: {}", e))?,
                    )
                }
                InputType::MagnetLink => Box::new(
                    MagnetDownloadCommand::new(gid, &input.raw, &options, dir.as_deref())
                        .map_err(|e| format!("Magnet download command failed: {}", e))?,
                ),
            };

            engine
                .add_command(cmd)
                .map_err(|e| format!("Failed to add command to engine: {}", e))?;
            gids.push(gid.value());
        }

        Ok(gids)
    }

    pub async fn run_engine(&self) -> std::result::Result<(), String> {
        let mut engine_lock: tokio::sync::MutexGuard<'_, Option<DownloadEngine>> =
            self.engine.lock().await;
        if let Some(engine) = engine_lock.take() {
            drop(engine_lock);
            info!("启动下载引擎, 共 {} 个任务", self.detected_inputs.len());
            let result: Result<(), _> = engine.run().await;
            result.map_err(|e| format!("引擎运行错误: {}", e))
        } else {
            Err("引擎未初始化".to_string())
        }
    }

    /// Run the complete application lifecycle.
    ///
    /// This is the main entry point that:
    /// 1. Handles `--help` / `--version` flags
    /// 2. Loads config from env → file → CLI args (4-source merge)
    /// 3. Initializes the download engine
    /// 4. Adds download tasks from positional URIs
    /// 5. Runs the engine event loop
    ///
    /// Returns exit code: `0` = success, `1` = error.
    pub async fn run(&mut self, args: &[String]) -> i32 {
        if self.print_help_or_version(args) {
            return 0;
        }

        self.load_env().await;

        if let Err(e) = self.load_config_file(None).await {
            error!("加载配置文件失败: {}", e);
        }

        if let Err(e) = self.load_args(args).await {
            eprintln!("{}", format!("参数解析错误: {}", e).red());
            return 1;
        }

        let log_level = if self.get_opt_bool("verbose").await.unwrap_or(false) {
            Level::DEBUG
        } else {
            Level::INFO
        };
        let log_path = self.get_opt_str("log").await;
        init_logging(log_level, log_path.as_deref());

        self.print_banner();

        if self.detected_inputs.is_empty() {
            eprintln!("{}", "错误: 请提供下载URI或torrent文件路径".red());
            return 1;
        }

        self.initialize_engine().await;

        match self.add_downloads().await {
            Ok(gids) => {
                info!("已添加 {} 个下载任务", gids.len());
                for gid in &gids {
                    println!("  {} 任务 #{}", "#".cyan(), gid.to_string().yellow());
                }
            }
            Err(e) => {
                eprintln!("{}", format!("添加任务失败: {}", e).red());
                return 1;
            }
        }

        println!();

        match self.run_engine().await {
            Ok(()) => {
                println!("{}", "所有任务完成!".green().bold());
                0
            }
            Err(e) => {
                eprintln!("{}", format!("下载失败: {}", e).red());
                1
            }
        }
    }

    fn print_help_or_version(&self, args: &[String]) -> bool {
        for arg in args {
            if arg == "--help" || arg == "-h" {
                self.print_help();
                return true;
            }
            if arg == "--version" || arg == "-V" || arg == "-v" {
                self.print_version();
                return true;
            }
        }
        false
    }

    fn print_banner(&self) {
        println!("{}", "aria2-rust v0.1.0".green().bold());
        println!(
            "{} {}",
            "Copyright:".blue(),
            "(C) 2024 aria2-rust contributors".white()
        );
        println!();
    }

    fn print_help(&self) {
        println!(
            "{}",
            "aria2-rust - The ultra fast download utility"
                .green()
                .bold()
        );
        println!();
        println!("{}", "用法:".yellow());
        println!("  aria2c [选项] <URI> [URI]...");
        println!("  aria2c [选项] -T <torrent文件>");
        println!();
        println!("{}", "主要选项:".yellow());
        println!("  -d, --dir=<DIR>              保存目录 (默认: 当前目录)");
        println!("  -o, --out=<FILE>             输出文件名");
        println!("  -s, --split=<N>               每服务器连接数 (默认: 1)");
        println!("  -x, --max-connection-per-server=<N>  最大连接数 (默认: 1)");
        println!("  --max-download-limit=<SPEED>   最大下载速度限制");
        println!("  --timeout=<SEC>               超时时间 (默认: 60)");
        println!("  --max-tries=<N>               最大重试次数 (默认: 5)");
        println!();
        println!("{}", "HTTP/FTP选项:".yellow());
        println!("  --user=<USER>                 HTTP/FTP用户名");
        println!("  --password=<PASS>             HTTP/FTP密码");
        println!("  --header=[HEADER]             自定义HTTP头");
        println!("  --proxy=<PROXY>               代理服务器");
        println!();
        println!("{}", "BitTorrent选项:".yellow());
        println!("  --seed-time=<MIN>             做种时间 (默认: 0=不做种)");
        println!("  --bt-tracker=<URL>[,...]      Tracker URL列表");
        println!();
        println!("{}", "RPC选项:".yellow());
        println!("  --enable-rpc[=true]           启用RPC服务");
        println!("  --rpc-listen-port=<PORT>      RPC监听端口 (default: 6800)");
        println!();
        println!("{}", "通用选项:".yellow());
        println!("  -i, --input-file=<FILE>       URI列表输入文件");
        println!("  --conf-path=<PATH>            配置文件路径");
        println!("  --log=<PATH>                  日志文件路径");
        println!("  -q, --quiet                   安静模式");
        println!("  -v, --verbose                 详细输出");
        println!("  --version                     显示版本号");
        println!("  -h, --help                    显示帮助信息");
        println!();
        println!("示例:");
        println!("  aria2c http://example.com/file.zip");
        println!("  aria2c -o output.iso http://example.com/image.iso");
        println!("  aria2c -d /downloads -s 4 http://example.com/large.bin");
    }

    fn print_version(&self) {
        println!("aria2-rust v{}", env!("CARGO_PKG_VERSION"));
        println!("基于Rust实现的aria2下载工具");
        println!();
        println!("支持的协议:");
        println!("  HTTP/HTTPS  ✅");
        println!("  FTP/SFTP    ✅");
        println!("  BitTorrent  ✅");
        println!("  Metalink    ✅");
        println!("  RPC(JSON/XML/WebSocket) ✅");
    }

    async fn get_opt_str(&self, name: &str) -> Option<String> {
        self.config.read().await.get_global_str(name).await
    }

    async fn get_opt_i64(&self, name: &str) -> Option<i64> {
        self.config.read().await.get_global_i64(name).await
    }

    async fn get_opt_bool(&self, name: &str) -> Option<bool> {
        self.config.read().await.get_global_bool(name).await
    }

    fn map_short_option(&self, c: char) -> Option<&'static str> {
        match c {
            'd' | 'D' => Some("dir"),
            'o' | 'O' => Some("out"),
            's' | 'S' => Some("split"),
            'x' | 'X' => Some("max-connection-per-server"),
            'i' | 'I' => Some("input-file"),
            'l' | 'L' => Some("log"),
            'j' | 'J' => Some("max-concurrent-downloads"),
            'v' | 'V' => Some("verbose"),
            'q' | 'Q' => Some("quiet"),
            _ => None,
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
