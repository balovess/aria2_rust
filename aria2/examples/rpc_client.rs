use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use colored::Colorize;

#[tokio::main]
async fn main() {
    println!("{}", "=== aria2-rust RPC 客户端示例 ===".cyan().bold());
    println!();

    let engine = RpcEngine::new();

    println!("1. 添加下载任务...");
    let req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.addUri".into(),
        params: serde_json::json!([["http://example.com/sample.zip"], {"dir": "/tmp", "split": 4}]),
        id: Some(serde_json::Value::String("req-1".into())),
    };

    let resp = engine.handle_request(&req).await;
    let gid = if resp.is_success() {
        resp.result.clone().unwrap_or(serde_json::Value::Null)
            .as_str().unwrap_or("")
            .to_string()
    } else {
        eprintln!("   错误: 无法添加任务");
        return;
    };

    println!("   任务已添加, GID: {}", gid.yellow());

    println!("\n2. 查询全局统计...");
    let stat_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.getGlobalStat".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("req-3".into())),
    };
    let stat_resp = engine.handle_request(&stat_req).await;
    if stat_resp.is_success() {
        let result = stat_resp.result.as_ref().unwrap();
        let stat = result.as_object().unwrap();
        println!("   活跃任务: {}", stat.get("numActive").unwrap());
        println!("   等待任务: {}", stat.get("numWaiting").unwrap());
    }

    println!("\n3. 暂停并移除任务...");
    let pause_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.pause".into(),
        params: serde_json::json!([gid]),
        id: Some(serde_json::Value::String("req-4".into())),
    };
    engine.handle_request(&pause_req).await;

    let remove_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.remove".into(),
        params: serde_json::json!([gid]),
        id: Some(serde_json::Value::String("req-5".into())),
    };
    engine.handle_request(&remove_req).await;

    println!("   任务已移除");
    println!("\n{} RPC 示例完成!", "✓".green().bold());
}
