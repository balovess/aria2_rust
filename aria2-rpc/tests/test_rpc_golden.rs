//! Golden wire cases for the public RPC library boundary.
//!
//! The fixture is deliberately small and literal: it locks the protocol
//! shapes which are easy to accidentally change while allowing live GIDs and
//! the package version to remain dynamic in the engine-level checks below.

mod common;

#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
use aria2_rpc::engine::RpcEngine;
#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::json_rpc::JsonRpcResponse;
use aria2_rpc::types::{
    DhtStatus, FileInfo, GlobalStat, PeerInfo, ServerInfoIndex, TrackerInfo, UriEntry,
};
use aria2_rpc::websocket::DownloadEvent;
use aria2_rpc::xml_rpc::XmlRpcResponse;
#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
use common::test_engine;
use serde_json::Value;
#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
use serde_json::json;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/rpc_wire_golden.json"))
        .expect("golden fixture must be valid JSON")
}

#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
async fn call(engine: &RpcEngine, method: &str, params: Value, id: &str) -> Value {
    let request = JsonRpcRequest::new(method, params).with_id(id);
    serde_json::to_value(engine.handle_request(&request).await).expect("response is serializable")
}

#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
fn result<'a>(response: &'a Value, method: &str) -> &'a Value {
    assert_eq!(response["jsonrpc"], "2.0", "{method} must use JSON-RPC 2.0");
    assert!(
        response.get("error").is_none(),
        "{method} failed: {response}"
    );
    response
        .get("result")
        .unwrap_or_else(|| panic!("{method} omitted result: {response}"))
}

#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
fn gid(value: &Value, method: &str) -> String {
    let gid = result(value, method)
        .as_str()
        .unwrap_or_else(|| panic!("{method} must return a string GID: {value}"));
    assert_eq!(gid.len(), 16, "{method} GID length changed");
    assert!(
        gid.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{method} GID is not lowercase hexadecimal: {gid}"
    );
    gid.to_owned()
}

#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
fn assert_error(response: &Value, code: i64, message: &str) {
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], code);
    assert_eq!(response["error"]["message"], message);
    assert!(response.get("result").is_none());
}

#[test]
fn primitive_wire_models_match_golden_fixture() {
    let golden = fixture();

    let success = JsonRpcResponse::success("golden-success", "OK");
    assert_eq!(
        serde_json::to_value(success).unwrap(),
        golden["json_success_text"]
    );

    let error = JsonRpcResponse::error("golden-error", 1, "Unauthorized");
    assert_eq!(
        serde_json::to_value(error).unwrap(),
        golden["json_error_unauthorized"]
    );

    assert_eq!(
        serde_json::to_value(GlobalStat::default()).unwrap(),
        golden["global_stat_zero"]
    );
    assert_eq!(
        serde_json::to_value(DhtStatus {
            state: "stopped".into(),
            total_nodes: 0,
            good_nodes: 0,
            pending_transactions: 0,
        })
        .unwrap(),
        golden["dht_status_stopped"]
    );

    let file = FileInfo::new("/downloads/file.bin", 1024)
        .with_index(1)
        .with_completed(512)
        .with_uris(vec![UriEntry::new("https://example.test/file.bin")]);
    assert_eq!(serde_json::to_value(file).unwrap(), golden["file_info"]);
    assert_eq!(
        serde_json::to_value(UriEntry::new("https://example.test/file.bin")).unwrap(),
        golden["uri_entry"]
    );
    assert_eq!(
        serde_json::to_value(ServerInfoIndex {
            index: 1,
            servers: Vec::new(),
        })
        .unwrap(),
        golden["server_index"]
    );

    let peer = PeerInfo {
        peer_id: "peer-1".into(),
        ip: "192.0.2.10".into(),
        source: "golden-test".into(),
        port: 6881,
        bitfield: Some("ff".into()),
        am_choking: true,
        peer_choking: false,
        download_speed: 12,
        upload_speed: 3,
        seeder: Some("true".into()),
    };
    assert_eq!(serde_json::to_value(peer).unwrap(), golden["peer_info"]);

    let tracker = TrackerInfo {
        uri: "https://tracker.example.test/announce".into(),
        tier: 1,
        current: true,
        last_attempt: false,
        announce_ready: true,
        all_failed: false,
        in_flight: 0,
        interval: 1800,
        min_interval: 0,
        seeders: 0,
        leechers: 0,
        tracker_id: String::new(),
        seconds_since_last_success: Some(12),
    };
    assert_eq!(
        serde_json::to_value(tracker).unwrap(),
        golden["tracker_info"]
    );

    assert_eq!(
        serde_json::from_str::<Value>(
            &DownloadEvent::download_complete("0123456789abcdef")
                .to_json()
                .unwrap()
        )
        .unwrap(),
        golden["download_complete_notification"]
    );
    assert_eq!(
        XmlRpcResponse::fault(1, "Unauthorized").to_xml(),
        golden["xml_fault_unauthorized"]
    );
}

#[tokio::test]
#[cfg(all(feature = "bittorrent", feature = "metalink", feature = "sftp"))]
async fn all_enabled_rpc_method_families_match_golden_contracts() {
    let golden = fixture();
    let engine = test_engine();
    let task_path = std::path::Path::new(".")
        .join("file.bin")
        .to_string_lossy()
        .into_owned();

    let version = call(&engine, "aria2.getVersion", json!([]), "version").await;
    let version_result = result(&version, "getVersion");
    assert_eq!(version_result["enabledFeatures"], golden["features_all"]);
    assert_eq!(version_result["version"], env!("CARGO_PKG_VERSION"));

    let methods = call(&engine, "system.listMethods", json!([]), "methods").await;
    let notifications = call(
        &engine,
        "system.listNotifications",
        json!([]),
        "notifications",
    )
    .await;
    assert_eq!(
        result(&methods, "system.listMethods"),
        &golden["methods_all"]
    );
    assert_eq!(
        result(&notifications, "system.listNotifications"),
        &golden["notifications_all"]
    );

    let add = call(
        &engine,
        "aria2.addUri",
        json!([["https://example.test/file.bin"]]),
        "add",
    )
    .await;
    let main_gid = gid(&add, "addUri");

    let status = call(
        &engine,
        "aria2.tellStatus",
        json!([
            main_gid,
            ["gid", "status", "dir", "files", "completedLength"]
        ]),
        "status",
    )
    .await;
    assert_eq!(
        result(&status, "tellStatus"),
        &json!({
            "gid": main_gid,
            "status": "active",
            "dir": ".",
            "completedLength": "0",
            "files": [{
                "index": "1",
                "path": task_path,
                "length": "0",
                "completedLength": "0",
                "selected": "true",
                "uris": [golden["uri_entry"]]
            }]
        })
    );

    let uris = call(&engine, "aria2.getUris", json!([main_gid]), "uris").await;
    assert_eq!(result(&uris, "getUris"), &json!([golden["uri_entry"]]));

    let files = call(&engine, "aria2.getFiles", json!([main_gid]), "files").await;
    assert_eq!(
        result(&files, "getFiles"),
        &json!([{
            "index": "1",
            "path": std::path::Path::new(".")
                .join("file.bin")
                .to_string_lossy(),
            "length": "0",
            "completedLength": "0",
            "selected": "true",
            "uris": [golden["uri_entry"]]
        }])
    );

    let servers = call(&engine, "aria2.getServers", json!([main_gid]), "servers").await;
    assert_eq!(
        result(&servers, "getServers"),
        &json!([golden["server_index"]])
    );

    let active = call(&engine, "aria2.tellActive", json!([]), "active").await;
    assert_eq!(result(&active, "tellActive").as_array().unwrap().len(), 1);
    assert_eq!(result(&active, "tellActive")[0]["gid"], main_gid);
    assert_eq!(
        result(
            &call(&engine, "aria2.tellWaiting", json!([0, 10]), "waiting").await,
            "tellWaiting"
        ),
        &json!([])
    );
    assert_eq!(
        result(
            &call(&engine, "aria2.tellStopped", json!([0, 10]), "stopped").await,
            "tellStopped"
        ),
        &json!([])
    );

    let stat = call(&engine, "aria2.getGlobalStat", json!([]), "stat").await;
    assert_eq!(
        result(&stat, "getGlobalStat"),
        &json!({
            "downloadSpeed": "0",
            "uploadSpeed": "0",
            "numActive": "1",
            "numWaiting": "0",
            "numStopped": "0",
            "numStoppedTotal": "0"
        })
    );

    let global_options = call(
        &engine,
        "aria2.getGlobalOption",
        json!([]),
        "global-options",
    )
    .await;
    assert_eq!(result(&global_options, "getGlobalOption")["dir"], ".");
    let changed_global = call(
        &engine,
        "aria2.changeGlobalOption",
        json!([{"max-overall-download-limit": "1K"}]),
        "change-global",
    )
    .await;
    assert_eq!(result(&changed_global, "changeGlobalOption"), "OK");

    let changed_option = call(
        &engine,
        "aria2.changeOption",
        json!([main_gid, {"max-download-limit": "1K"}]),
        "change-option",
    )
    .await;
    assert_eq!(result(&changed_option, "changeOption"), "OK");
    let options = call(&engine, "aria2.getOption", json!([main_gid]), "options").await;
    assert_eq!(result(&options, "getOption")["max-download-limit"], "1K");

    let changed_uri = call(
        &engine,
        "aria2.changeUri",
        json!([
            main_gid,
            1,
            ["https://example.test/file.bin"],
            ["https://mirror.example.test/file.bin"]
        ]),
        "change-uri",
    )
    .await;
    assert_eq!(result(&changed_uri, "changeUri"), &golden["change_counts"]);

    let changed_position = call(
        &engine,
        "aria2.changePosition",
        json!([main_gid, 0, "POS_SET"]),
        "change-position",
    )
    .await;
    assert_eq!(result(&changed_position, "changePosition"), 0);

    assert_eq!(
        result(
            &call(&engine, "aria2.pause", json!([main_gid]), "pause").await,
            "pause"
        ),
        &json!(main_gid)
    );
    assert_eq!(
        result(
            &call(&engine, "aria2.unpause", json!([main_gid]), "unpause").await,
            "unpause"
        ),
        &json!(main_gid)
    );
    assert_eq!(
        result(
            &call(
                &engine,
                "aria2.forcePause",
                json!([main_gid]),
                "force-pause"
            )
            .await,
            "forcePause"
        ),
        &json!(main_gid)
    );
    assert_eq!(
        result(
            &call(&engine, "aria2.unpause", json!([main_gid]), "unpause-again").await,
            "unpause"
        ),
        &json!(main_gid)
    );
    for method in ["aria2.pauseAll", "aria2.forcePauseAll", "aria2.unpauseAll"] {
        assert_eq!(
            result(&call(&engine, method, json!([]), method).await, method),
            "OK"
        );
    }
    assert_eq!(
        result(
            &call(
                &engine,
                "aria2.updateBrowserContext",
                json!([{"cookie": "a=b"}]),
                "context",
            )
            .await,
            "updateBrowserContext"
        ),
        "OK"
    );
    assert_eq!(
        result(
            &call(
                &engine,
                "aria2.clearBrowserContext",
                json!([]),
                "clear-context"
            )
            .await,
            "clearBrowserContext"
        ),
        "OK"
    );

    let multicall = call(
        &engine,
        "system.multicall",
        json!([[{
            "methodName": "aria2.getGlobalStat",
            "params": []
        }, {
            "methodName": "aria2.getUris",
            "params": [main_gid]
        }]]),
        "multicall",
    )
    .await;
    let multicall_result = result(&multicall, "system.multicall");
    assert_eq!(multicall_result[0][0]["numActive"], "1");
    assert_eq!(multicall_result[1][0], json!([golden["uri_entry_mirror"]]));

    let save_session = call(&engine, "aria2.saveSession", json!([]), "save-session").await;
    assert_error(
        &save_session,
        golden["save_session_without_path_error"]["code"]
            .as_i64()
            .unwrap(),
        golden["save_session_without_path_error"]["message"]
            .as_str()
            .unwrap(),
    );

    let session = call(&engine, "aria2.getSessionInfo", json!([]), "session").await;
    let session_id = result(&session, "getSessionInfo")["sessionId"]
        .as_str()
        .unwrap();
    assert_eq!(session_id.len(), 40);
    assert!(session_id.bytes().all(|byte| byte.is_ascii_hexdigit()));

    #[cfg(feature = "bittorrent")]
    {
        assert_eq!(
            result(
                &call(&engine, "aria2.getPeers", json!([main_gid]), "peers").await,
                "getPeers"
            ),
            &json!([])
        );
        assert_eq!(
            result(
                &call(&engine, "aria2.getTrackers", json!([main_gid]), "trackers").await,
                "getTrackers"
            ),
            &json!([])
        );
        assert_eq!(
            result(
                &call(&engine, "aria2.getDhtStatus", json!([]), "dht").await,
                "getDhtStatus"
            ),
            &golden["dht_status_default"]
        );

        let torrent = call(
            &engine,
            "aria2.addTorrent",
            json!([
                "bm90IGEgdG9ycmVudA==",
                ["https://example.test/torrent-file"]
            ]),
            "torrent",
        )
        .await;
        let torrent_gid = gid(&torrent, "addTorrent");
        let _ = call(
            &engine,
            "aria2.forceRemove",
            json!([[torrent_gid]]),
            "remove-torrent",
        )
        .await;
    }

    #[cfg(feature = "metalink")]
    {
        let metalink = call(
            &engine,
            "aria2.addMetalink",
            json!(["bm90IGEg bWV0YWxpbms=".replace(' ', "")]),
            "metalink",
        )
        .await;
        let metalink_gid = result(&metalink, "addMetalink")[0]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(metalink_gid.len(), 16);
        let _ = call(
            &engine,
            "aria2.forceRemove",
            json!([[metalink_gid]]),
            "remove-metalink",
        )
        .await;
    }

    let removed = call(&engine, "aria2.remove", json!([main_gid]), "remove").await;
    assert_eq!(result(&removed, "remove"), &json!(main_gid));
    assert_eq!(
        result(
            &call(
                &engine,
                "aria2.removeDownloadResult",
                json!([main_gid]),
                "remove-result"
            )
            .await,
            "removeDownloadResult"
        ),
        "OK"
    );
    assert_eq!(
        result(
            &call(&engine, "aria2.purgeDownloadResult", json!([]), "purge").await,
            "purgeDownloadResult"
        ),
        "OK"
    );

    assert_eq!(
        result(
            &call(&engine, "aria2.shutdown", json!([]), "shutdown").await,
            "shutdown"
        ),
        "OK. 0 active downloads paused."
    );
    assert_eq!(
        result(
            &call(&engine, "aria2.forceShutdown", json!([]), "force-shutdown").await,
            "forceShutdown"
        ),
        "OK. 0 downloads forcibly terminated."
    );
}
