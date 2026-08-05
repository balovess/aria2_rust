pub mod active_output_registry;
pub mod choking_algorithm;
pub mod command;
pub mod concurrent_download;
#[cfg(feature = "metalink")]
pub mod concurrent_download_command;
pub mod concurrent_segment_manager;
pub mod download_command;
pub mod download_cookie;
pub mod download_engine;
pub mod download_event_hooks;
pub mod download_progress;
pub mod engine_command;
pub mod engine_loop;
pub mod halt_watchers;
pub mod http_segment_downloader;
pub mod http_tail_reclaim;
pub mod http_tracker_client;
pub mod mirror_coordinator;
#[cfg(feature = "bittorrent")]
pub mod multi_file_layout;
pub mod peer_stats;
#[cfg(test)]
pub mod peer_stats_tests;
pub mod post_download_handler;
pub mod range_prober;
pub mod resume_data;
pub mod retry_policy;
pub mod sequential_download;
pub mod task_spawner;
pub mod timer;

// ── BitTorrent feature-gated modules ──────────────────────────────────
#[cfg(feature = "bittorrent")]
pub mod bt_choke_hooks;
#[cfg(feature = "bittorrent")]
pub mod bt_choke_manager;
#[cfg(all(test, feature = "bittorrent"))]
pub mod bt_choke_manager_tests;
#[cfg(feature = "bittorrent")]
pub mod bt_connection_pool;
#[cfg(feature = "bittorrent")]
pub mod bt_download_command;
#[cfg(all(test, feature = "bittorrent"))]
pub mod bt_download_command_tests;
#[cfg(feature = "bittorrent")]
pub mod bt_download_execute;
#[cfg(feature = "bittorrent")]
pub mod bt_download_seeding;
#[cfg(feature = "bittorrent")]
pub mod bt_handshake_validation;
#[cfg(feature = "bittorrent")]
pub mod bt_message_dispatcher;
#[cfg(feature = "bittorrent")]
pub mod bt_message_handler;
#[cfg(all(test, feature = "bittorrent"))]
pub mod bt_message_handler_tests;
#[cfg(feature = "bittorrent")]
pub mod bt_message_receiver;
#[cfg(all(test, feature = "bittorrent"))]
pub mod bt_message_receiver_tests;
#[cfg(feature = "bittorrent")]
pub mod bt_peer_blocklist;
#[cfg(feature = "bittorrent")]
pub mod bt_peer_connection;
#[cfg(feature = "bittorrent")]
pub mod bt_peer_interaction;
#[cfg(feature = "bittorrent")]
pub mod bt_peer_storage;
#[cfg(feature = "bittorrent")]
pub mod bt_piece_downloader;
#[cfg(feature = "bittorrent")]
pub mod bt_piece_selector;
#[cfg(feature = "bittorrent")]
pub mod bt_progress_info_file;
#[cfg(all(test, feature = "bittorrent"))]
pub mod bt_progress_info_file_tests;
#[cfg(feature = "bittorrent")]
pub mod bt_registry;
#[cfg(feature = "bittorrent")]
pub mod bt_request_factory;
#[cfg(feature = "bittorrent")]
pub mod bt_seed_manager;
pub mod bt_setup;
#[cfg(feature = "bittorrent")]
pub mod bt_torrent_post_download_handler;
#[cfg(feature = "bittorrent")]
pub mod bt_tracker_comm;
#[cfg(feature = "bittorrent")]
pub mod bt_upload_session;
#[cfg(feature = "bittorrent")]
pub mod bt_web_seed;
#[cfg(feature = "bittorrent")]
pub mod extension_registry;
#[cfg(feature = "bittorrent")]
pub mod hook_manager;
#[cfg(feature = "bittorrent")]
pub mod lpd_manager;
#[cfg(feature = "bittorrent")]
pub mod lpd_receive_loop;
#[cfg(feature = "bittorrent")]
pub mod magnet_download_command;
#[cfg(feature = "bittorrent")]
pub mod metadata_exchange;
#[cfg(feature = "bittorrent")]
pub mod udp_tracker_client;
#[cfg(feature = "bittorrent")]
pub mod udp_tracker_manager;

#[cfg(all(test, feature = "bittorrent"))]
pub mod bt_integration_tests;

// ── Metalink feature-gated modules ────────────────────────────────────
#[cfg(feature = "metalink")]
pub mod metalink_download_command;
#[cfg(feature = "metalink")]
pub mod metalink_post_download_handler;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
pub mod metalink_request_graph;
#[cfg(feature = "metalink")]
pub mod metalink_to_request_group;

// ── SFTP feature-gated modules ───────────────────────────────────────
#[cfg(feature = "sftp")]
pub mod sftp_download_command;

// ── FTP (always included for now; can be gated later) ─────────────────
pub mod ftp_download_command;
