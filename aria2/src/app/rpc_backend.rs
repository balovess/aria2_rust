//! Core-owned implementation of the protocol-independent RPC backend.
//!
//! `aria2-rpc` owns the wire protocol and knows only [`RpcBackend`].  This
//! adapter is the single place where RPC operations are translated into
//! `aria2-core` state changes, queries, and engine commands.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aria2_core::checksum::checksum::Checksum;
use aria2_core::config::{ConfigManager, OptionValue, project_initial_options};
#[cfg(feature = "bittorrent")]
use aria2_core::engine::bittorrent::registry::BtRegistry;
use aria2_core::engine::engine_command::{EngineCommand, EngineCommandSender};
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::request::request_group_man::{GroupIdResolution, RequestGroupMan};
use aria2_rpc::{BackendError, BackendMetadata};
use tokio::sync::RwLock;

mod creation;
mod dispatch;
mod lifecycle;
mod query;
mod values;

const RPC_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

fn rpc_peer_port(addr: SocketAddr, is_incoming: bool) -> u16 {
    if is_incoming { 0 } else { addr.port() }
}

#[cfg(test)]
mod peer_id_tests {
    use super::rpc_peer_id;

    #[test]
    fn peer_id_uses_aria2_percent_encoding() {
        let mut peer_id = [0u8; 20];
        peer_id[0] = b'A';
        peer_id[1] = b'7';
        peer_id[2] = b' ';
        peer_id[3] = 0xff;

        assert_eq!(
            rpc_peer_id(&peer_id),
            "A7%20%FF%00%00%00%00%00%00%00%00%00%00%00%00%00%00%00%00"
        );
    }
}

fn rpc_peer_id(peer_id: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(peer_id.len() * 3);
    for &byte in peer_id {
        if byte.is_ascii_alphanumeric() {
            output.push(byte as char);
        } else {
            output.push('%');
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    output
}

/// The application adapter behind the RPC wire layer.
pub struct CoreRpcBackend {
    group_man: Arc<RequestGroupMan>,
    engine_cmd_tx: EngineCommandSender,
    config: Arc<RwLock<ConfigManager>>,
    save_session_path: Option<PathBuf>,
    metadata: BackendMetadata,
    #[cfg(feature = "bittorrent")]
    bt_registry: Option<Arc<std::sync::RwLock<BtRegistry>>>,
}
impl CoreRpcBackend {
    pub fn new(
        group_man: Arc<RequestGroupMan>,
        engine_cmd_tx: EngineCommandSender,
        config: Arc<RwLock<ConfigManager>>,
        save_session_path: Option<PathBuf>,
        product_version: impl Into<String>,
    ) -> Self {
        let mut metadata = BackendMetadata::base(product_version);
        #[cfg(feature = "bittorrent")]
        {
            metadata = metadata.with_bittorrent();
        }
        #[cfg(feature = "metalink")]
        {
            metadata = metadata.with_metalink();
        }
        #[cfg(feature = "sftp")]
        {
            metadata = metadata.with_sftp();
        }

        Self {
            group_man,
            engine_cmd_tx,
            config,
            save_session_path,
            metadata,
            #[cfg(feature = "bittorrent")]
            bt_registry: None,
        }
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_registry(&mut self, registry: Arc<std::sync::RwLock<BtRegistry>>) {
        self.bt_registry = Some(registry);
    }

    fn invalid(message: impl Into<String>) -> BackendError {
        BackendError::InvalidParams(message.into())
    }

    fn execution(message: impl Into<String>) -> BackendError {
        BackendError::Execution(message.into())
    }

    #[cfg(feature = "bittorrent")]
    fn validate_torrent_data(data: &[u8]) -> Result<(), BackendError> {
        if data.len() < 3 || data[0] != b'd' || data[1] != b'8' || data[2] != b':' {
            return Err(Self::invalid("Invalid BEncode data (not a .torrent file)"));
        }
        Ok(())
    }

    fn parse_gid(&self, gid: &str) -> Result<GroupId, BackendError> {
        match self.group_man.resolve_gid_hex_detailed(gid) {
            GroupIdResolution::Resolved(gid) => Ok(gid),
            GroupIdResolution::Invalid => Err(Self::execution(format!("Invalid GID {gid}"))),
            GroupIdResolution::NotUnique => {
                Err(Self::execution(format!("GID {gid} is not unique")))
            }
            GroupIdResolution::NotFound => Err(Self::execution(format!("GID {gid} is not found"))),
        }
    }

    fn send(&self, command: EngineCommand) -> Result<(), BackendError> {
        self.engine_cmd_tx.send(command).map_err(|error| {
            BackendError::Internal(format!("Failed to send engine command: {error}"))
        })
    }

    async fn global_options(&self) -> HashMap<String, serde_json::Value> {
        self.config
            .read()
            .await
            .get_all_global_options()
            .await
            .into_iter()
            .map(|(key, value)| {
                let value = match value {
                    OptionValue::List(values) => serde_json::Value::String(values.join("\n")),
                    value => (&value).into(),
                };
                (key, value)
            })
            .collect()
    }

    async fn merged_task_options(
        &self,
        request_options: HashMap<String, serde_json::Value>,
    ) -> Result<(DownloadOptions, HashMap<String, serde_json::Value>), BackendError> {
        let mut options = self.global_options().await;
        options.extend(request_options);
        let download_options =
            DownloadOptions::try_from_rpc_options(&options).map_err(Self::invalid)?;
        let snapshot = project_initial_options(options);
        if let Some((algorithm, value)) = &download_options.checksum {
            Checksum::from_type_and_value(algorithm, value)
                .map_err(|error| Self::invalid(format!("Invalid checksum: {error}")))?;
        }
        Ok((download_options, snapshot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_core::request::request_group::RequestGroup;

    #[tokio::test]
    async fn add_uri_options_inherit_continue_default_and_accept_false() {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let (sender, _receiver) = aria2_core::engine::engine_command::channel();
        let backend = CoreRpcBackend::new(
            Arc::new(RequestGroupMan::new()),
            sender,
            config,
            None,
            "test",
        );

        let (options, _) = backend
            .merged_task_options(HashMap::new())
            .await
            .expect("RPC task options should inherit defaults");
        assert!(options.continue_download);

        let (options, _) = backend
            .merged_task_options(HashMap::from([(
                "continue".to_string(),
                serde_json::Value::Bool(false),
            )]))
            .await
            .expect("RPC continue=false should be accepted");
        assert!(!options.continue_download);
    }

    #[tokio::test]
    async fn add_uri_options_pass_user_agent_to_download_options() {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let (sender, _receiver) = aria2_core::engine::engine_command::channel();
        let backend = CoreRpcBackend::new(
            Arc::new(RequestGroupMan::new()),
            sender,
            Arc::clone(&config),
            None,
            "test",
        );

        config
            .write()
            .await
            .set_global_option(
                "user-agent",
                aria2_core::config::OptionValue::Str("global-agent/1.0".to_string()),
            )
            .await
            .expect("global User-Agent option should be accepted");
        let (global_options, _) = backend
            .merged_task_options(HashMap::new())
            .await
            .expect("global User-Agent should reach DownloadOptions");
        assert_eq!(
            global_options.user_agent.as_deref(),
            Some("global-agent/1.0")
        );

        let (task_options, _) = backend
            .merged_task_options(HashMap::from([(
                "user-agent".to_string(),
                serde_json::Value::String("task-agent/1.0".to_string()),
            )]))
            .await
            .expect("per-task User-Agent should be accepted");
        assert_eq!(task_options.user_agent.as_deref(), Some("task-agent/1.0"));
    }

    #[tokio::test]
    async fn add_uri_options_pass_http_auth_credentials_and_headers() {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let (sender, _receiver) = aria2_core::engine::engine_command::channel();
        let backend = CoreRpcBackend::new(
            Arc::new(RequestGroupMan::new()),
            sender,
            Arc::clone(&config),
            None,
            "test",
        );

        {
            let mut config = config.write().await;
            config
                .set_global_option(
                    "http-user",
                    aria2_core::config::OptionValue::Str("global-user".into()),
                )
                .await
                .expect("global HTTP user should be accepted");
            config
                .set_global_option(
                    "http-passwd",
                    aria2_core::config::OptionValue::Str("global-pass".into()),
                )
                .await
                .expect("global HTTP password should be accepted");
            config
                .set_global_option(
                    "http-auth-challenge",
                    aria2_core::config::OptionValue::Bool(true),
                )
                .await
                .expect("HTTP challenge mode should be accepted");
            config
                .set_global_option("no-netrc", aria2_core::config::OptionValue::Bool(true))
                .await
                .expect("no-netrc should be accepted");
            config
                .set_global_option(
                    "header",
                    aria2_core::config::OptionValue::List(vec![
                        "Authorization: Bearer global".into(),
                    ]),
                )
                .await
                .expect("global Authorization header should be accepted");
        }

        let (global, _) = backend
            .merged_task_options(HashMap::new())
            .await
            .expect("global HTTP auth options should reach DownloadOptions");
        assert_eq!(global.http_user.as_deref(), Some("global-user"));
        assert_eq!(global.http_passwd.as_deref(), Some("global-pass"));
        assert!(global.http_auth_challenge);
        assert!(global.no_netrc);
        assert_eq!(global.header, ["Authorization: Bearer global"]);

        let (task, _) = backend
            .merged_task_options(HashMap::from([
                (
                    "http-user".to_string(),
                    serde_json::Value::String("task-user".into()),
                ),
                (
                    "http-passwd".to_string(),
                    serde_json::Value::String("task-pass".into()),
                ),
                (
                    "header".to_string(),
                    serde_json::json!(["Authorization: Bearer task"]),
                ),
            ]))
            .await
            .expect("task HTTP auth options should override global values");
        assert_eq!(task.http_user.as_deref(), Some("task-user"));
        assert_eq!(task.http_passwd.as_deref(), Some("task-pass"));
        assert_eq!(task.header, ["Authorization: Bearer task"]);
        assert!(task.http_auth_challenge);
        assert!(task.no_netrc);
    }

    #[tokio::test]
    async fn add_uri_options_pass_all_proxy_to_download_options() {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let (sender, _receiver) = aria2_core::engine::engine_command::channel();
        let backend = CoreRpcBackend::new(
            Arc::new(RequestGroupMan::new()),
            sender,
            Arc::clone(&config),
            None,
            "test",
        );

        config
            .write()
            .await
            .set_global_option(
                "all-proxy",
                aria2_core::config::OptionValue::Str("socks5://global-proxy.invalid:1080".into()),
            )
            .await
            .expect("global all-proxy option should be accepted");
        {
            let mut config = config.write().await;
            config
                .set_global_option(
                    "all-proxy-user",
                    aria2_core::config::OptionValue::Str("global-proxy-user".into()),
                )
                .await
                .expect("global proxy username should be accepted");
            config
                .set_global_option(
                    "all-proxy-passwd",
                    aria2_core::config::OptionValue::Str("global-proxy-pass".into()),
                )
                .await
                .expect("global proxy password should be accepted");
            config
                .set_global_option(
                    "http-proxy",
                    aria2_core::config::OptionValue::Str(
                        "http://global-http-proxy.invalid:3128".into(),
                    ),
                )
                .await
                .expect("global HTTP proxy should be accepted");
            config
                .set_global_option(
                    "http-proxy-user",
                    aria2_core::config::OptionValue::Str("global-http-user".into()),
                )
                .await
                .expect("global HTTP proxy username should be accepted");
            config
                .set_global_option(
                    "http-proxy-passwd",
                    aria2_core::config::OptionValue::Str("global-http-pass".into()),
                )
                .await
                .expect("global HTTP proxy password should be accepted");
        }
        config
            .write()
            .await
            .set_global_option(
                "no-proxy",
                aria2_core::config::OptionValue::List(vec![
                    "first.example".into(),
                    "second.example".into(),
                ]),
            )
            .await
            .expect("global no-proxy option should be accepted");
        let (global_options, _) = backend
            .merged_task_options(HashMap::new())
            .await
            .expect("global proxy options should reach DownloadOptions");
        assert_eq!(
            global_options.all_proxy.as_deref(),
            Some("socks5://global-proxy.invalid:1080")
        );
        assert_eq!(
            global_options.all_proxy_user.as_deref(),
            Some("global-proxy-user")
        );
        assert_eq!(
            global_options.all_proxy_passwd.as_deref(),
            Some("global-proxy-pass")
        );
        assert_eq!(
            global_options.http_proxy.as_deref(),
            Some("http://global-http-proxy.invalid:3128")
        );
        assert_eq!(
            global_options.http_proxy_user.as_deref(),
            Some("global-http-user")
        );
        assert_eq!(
            global_options.http_proxy_passwd.as_deref(),
            Some("global-http-pass")
        );
        assert_eq!(
            global_options.no_proxy.as_deref(),
            Some("first.example\nsecond.example")
        );

        let (task_options, _) = backend
            .merged_task_options(HashMap::from([(
                "all-proxy".to_string(),
                serde_json::Value::String("socks4://task-proxy.invalid:1080".to_string()),
            )]))
            .await
            .expect("per-task all-proxy override should be accepted");
        assert_eq!(
            task_options.all_proxy.as_deref(),
            Some("socks4://task-proxy.invalid:1080")
        );

        let (task_options, _) = backend
            .merged_task_options(HashMap::from([
                (
                    "http-proxy".to_string(),
                    serde_json::Value::String("http://task-http-proxy.invalid:8080".into()),
                ),
                (
                    "http-proxy-user".to_string(),
                    serde_json::Value::String("task-http-user".into()),
                ),
                (
                    "http-proxy-passwd".to_string(),
                    serde_json::Value::String("task-http-pass".into()),
                ),
            ]))
            .await
            .expect("task proxy URL and credentials should override global values");
        assert_eq!(
            task_options.http_proxy.as_deref(),
            Some("http://task-http-proxy.invalid:8080")
        );
        assert_eq!(
            task_options.http_proxy_user.as_deref(),
            Some("task-http-user")
        );
        assert_eq!(
            task_options.http_proxy_passwd.as_deref(),
            Some("task-http-pass")
        );
        assert_eq!(
            task_options.all_proxy_user.as_deref(),
            Some("global-proxy-user")
        );
        assert_eq!(
            task_options.all_proxy_passwd.as_deref(),
            Some("global-proxy-pass")
        );

        let (task_options, _) = backend
            .merged_task_options(HashMap::from([(
                "no-proxy".to_string(),
                serde_json::Value::String("task-first.example\ntask-second.example".to_string()),
            )]))
            .await
            .expect("per-task no-proxy list should reach DownloadOptions");
        assert_eq!(
            task_options.no_proxy.as_deref(),
            Some("task-first.example\ntask-second.example")
        );
    }

    #[tokio::test]
    async fn ftp_reuse_option_flows_from_global_and_task_rpc_options() {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let (sender, _receiver) = aria2_core::engine::engine_command::channel();
        let backend = CoreRpcBackend::new(
            Arc::new(RequestGroupMan::new()),
            sender,
            Arc::clone(&config),
            None,
            "test",
        );

        backend
            .change_global_option(HashMap::from([(
                "ftp-reuse-connection".to_string(),
                serde_json::Value::Bool(false),
            )]))
            .await
            .expect("RPC should accept a global FTP reuse setting");
        let (global, _) = backend
            .merged_task_options(HashMap::new())
            .await
            .expect("global FTP reuse setting should reach download options");
        assert!(!global.ftp_reuse_connection);

        let (task, _) = backend
            .merged_task_options(HashMap::from([(
                "ftp-reuse-connection".to_string(),
                serde_json::Value::Bool(true),
            )]))
            .await
            .expect("per-task FTP reuse setting should reach download options");
        assert!(task.ftp_reuse_connection);
    }

    #[tokio::test]
    async fn sftp_connection_options_flow_from_global_and_task_rpc_options() {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let (sender, _receiver) = aria2_core::engine::engine_command::channel();
        let backend = CoreRpcBackend::new(
            Arc::new(RequestGroupMan::new()),
            sender,
            Arc::clone(&config),
            None,
            "test",
        );

        backend
            .change_global_option(HashMap::from([
                (
                    "ssh-host-key-md".to_string(),
                    serde_json::json!("sha-1=global-fingerprint"),
                ),
                ("sftp-connect-timeout".to_string(), serde_json::json!(41)),
                ("sftp-read-timeout".to_string(), serde_json::json!(73)),
                ("sftp-compression".to_string(), serde_json::json!(true)),
                ("sftp-keepalive-interval".to_string(), serde_json::json!(0)),
                (
                    "sftp-preferred-ciphers".to_string(),
                    serde_json::json!("aes256-gcm@openssh.com,chacha20-poly1305@openssh.com"),
                ),
            ]))
            .await
            .expect("RPC should accept global SFTP transport options");
        let (global, _) = backend
            .merged_task_options(HashMap::new())
            .await
            .expect("global SFTP options should reach download options");
        assert_eq!(global.sftp_connect_timeout, Some(41));
        assert_eq!(
            global.ssh_host_key_md.as_deref(),
            Some("sha-1=global-fingerprint")
        );
        assert_eq!(global.sftp_read_timeout, Some(73));
        assert!(global.sftp_compression);
        assert_eq!(global.sftp_keepalive_interval, Some(0));
        assert_eq!(
            global.sftp_preferred_ciphers.unwrap(),
            vec![
                "aes256-gcm@openssh.com".to_string(),
                "chacha20-poly1305@openssh.com".to_string()
            ]
        );

        let (task, _) = backend
            .merged_task_options(HashMap::from([
                (
                    "ssh-host-key-md".to_string(),
                    serde_json::json!("md5=task-fingerprint"),
                ),
                ("sftp-connect-timeout".to_string(), serde_json::json!(19)),
                ("sftp-read-timeout".to_string(), serde_json::json!(27)),
                ("sftp-compression".to_string(), serde_json::json!(false)),
                ("sftp-keepalive-interval".to_string(), serde_json::json!(15)),
                (
                    "sftp-preferred-ciphers".to_string(),
                    serde_json::json!("aes128-ctr"),
                ),
            ]))
            .await
            .expect("per-task SFTP options should override global values");
        assert_eq!(task.sftp_connect_timeout, Some(19));
        assert_eq!(
            task.ssh_host_key_md.as_deref(),
            Some("md5=task-fingerprint")
        );
        assert_eq!(task.sftp_read_timeout, Some(27));
        assert!(!task.sftp_compression);
        assert_eq!(task.sftp_keepalive_interval, Some(15));
        assert_eq!(
            task.sftp_preferred_ciphers.unwrap(),
            vec!["aes128-ctr".to_string()]
        );
    }

    #[tokio::test]
    async fn add_uri_options_apply_http_adaptive_settings_per_download() {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let (sender, _receiver) = aria2_core::engine::engine_command::channel();
        let backend = CoreRpcBackend::new(
            Arc::new(RequestGroupMan::new()),
            sender,
            config,
            None,
            "test",
        );

        let (options, _) = backend
            .merged_task_options(HashMap::from([
                (
                    "http-adaptive-enabled".to_string(),
                    serde_json::Value::Bool(true),
                ),
                (
                    "http-adaptive-reduction-percent".to_string(),
                    serde_json::json!(33),
                ),
                (
                    "http-adaptive-recovery-interval".to_string(),
                    serde_json::json!(17),
                ),
                (
                    "http-adaptive-recovery-successes-min".to_string(),
                    serde_json::json!(40),
                ),
                (
                    "http-adaptive-recovery-successes-max".to_string(),
                    serde_json::json!(20),
                ),
                (
                    "http-adaptive-start-interval-ms".to_string(),
                    serde_json::json!(250),
                ),
                (
                    "http-adaptive-backoff-base".to_string(),
                    serde_json::json!(30),
                ),
                (
                    "http-adaptive-backoff-max".to_string(),
                    serde_json::json!(10),
                ),
                (
                    "http-adaptive-capacity-start-interval-ms".to_string(),
                    serde_json::json!(2500),
                ),
                (
                    "http-adaptive-capacity-start-interval-max-ms".to_string(),
                    serde_json::json!(1500),
                ),
                (
                    "http-adaptive-rate-recovery-successes".to_string(),
                    serde_json::json!(128),
                ),
                (
                    "http-adaptive-rate-min-interval-ms".to_string(),
                    serde_json::json!(25),
                ),
                (
                    "http-adaptive-backoff-reset-after".to_string(),
                    serde_json::json!(70),
                ),
            ]))
            .await
            .expect("RPC task options should accept per-download adaptive settings");

        assert!(options.http_adaptive.enabled);
        assert_eq!(options.http_adaptive.reduction_percent, 33);
        assert_eq!(options.http_adaptive.recovery_interval_secs, 17);
        assert_eq!(options.http_adaptive.recovery_successes_min, 40);
        assert_eq!(options.http_adaptive.recovery_successes_max, 40);
        assert_eq!(options.http_adaptive.start_interval_ms, 250);
        assert_eq!(options.http_adaptive.backoff_base_secs, 30);
        assert_eq!(options.http_adaptive.backoff_max_secs, 30);
        assert_eq!(options.http_adaptive.capacity_start_interval_ms, 2500);
        assert_eq!(options.http_adaptive.capacity_start_interval_max_ms, 2500);
        assert_eq!(options.http_adaptive.rate_recovery_successes, 128);
        assert_eq!(options.http_adaptive.rate_min_interval_ms, 25);
        assert_eq!(options.http_adaptive.backoff_reset_after_secs, 70);
    }

    #[test]
    fn status_uses_real_non_bt_connection_count_instead_of_split() {
        let options = DownloadOptions {
            split: Some(16),
            ..DownloadOptions::default()
        };
        let group = RequestGroup::new(GroupId::new(0x101), Vec::new(), options);
        group.set_stream_connection_count(1);

        let status = CoreRpcBackend::status_from_group(&group, "0000000000000101");

        assert_eq!(status.connections, Some(1));
    }

    #[test]
    fn incoming_bt_peer_reports_aria2_compatible_zero_port() {
        let incoming_addr = "127.0.0.1:7673".parse().expect("valid socket address");
        assert_eq!(rpc_peer_port(incoming_addr, true), 0);
        assert_eq!(rpc_peer_port(incoming_addr, false), 7673);
    }
}
