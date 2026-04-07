use criterion::{criterion_group, Criterion, BenchmarkId, black_box};
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::{JsonRpcRequest, JsonRpcResponse};
use aria2_rpc::server::AuthConfig;

fn make_add_req(id: &str, uri: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.addUri".into(),
        params: serde_json::json!([[uri]]),
        id: Some(serde_json::Value::String(id.into())),
    }
}

fn make_tell_status_req(id: &str, gid: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.tellStatus".into(),
        params: serde_json::json!([gid]),
        id: Some(serde_json::Value::String(id.into())),
    }
}

fn make_generic_req(id: &str, method: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        version: Some("2.0".into()),
        method: method.into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String(id.into())),
    }
}

fn bench_add_uri_qps(c: &mut Criterion) {
    let engine = RpcEngine::new();
    let req = make_add_req("bench", "http://example.com/file.zip");

    c.bench_function("add_uri_single", |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async {
                let resp = engine.handle_request(&req).await;
                black_box(resp.is_success());
            });
        });
    });
}

fn bench_add_uri_batch(c: &mut Criterion) {
    c.bench(BenchmarkId::new("add_uri_batch_100"), |b| {
        b.iter(|| {
            let engine = RpcEngine::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async {
                for i in 0..100u32 {
                    let req = make_add_req(&format!("{}", i), &format!("http://example.com/{}.zip", i));
                    let resp = engine.handle_request(&req).await;
                    black_box(resp.is_success());
                }
            });
        });
    });
}

fn bench_tell_status_qps(c: &mut Criterion) {
    let engine = RpcEngine::new();
    let add_resp = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap();
        rt.block_on(async { engine.handle_request(&make_add_req("init", "http://ex.com/f.zip")).await })
    };
    let gid = if add_resp.is_success() {
        add_resp.result.clone().unwrap_or_default().as_str().unwrap_or("gid-001").to_string()
    } else {
        "gid-001".to_string()
    };

    c.bench(BenchmarkId::new("tell_status"), |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            let req = make_tell_status_req("ts", &gid);
            rt.block_on(async {
                let resp = engine.handle_request(&req).await;
                black_box(resp.is_success());
            });
        });
    });
}

fn bench_tell_active_empty(c: &mut Criterion) {
    let engine = RpcEngine::new();
    let req = make_generic_req("ta", "aria2.tellActive");

    c.bench_function("tell_active_empty_engine", |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async {
                let resp = engine.handle_request(&req).await;
                black_box(resp.is_success());
            });
        });
    });
}

fn bench_pause_unpause_cycle(c: &mut Criterion) {
    c.bench(BenchmarkId::new("pause_unpause_100_cycles"), |b| {
        b.iter(|| {
            let engine = RpcEngine::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async {
                for i in 0..100 {
                    let pause_req = JsonRpcRequest {
                        version: Some("2.0".into()),
                        method: "aria2.pause".into(),
                        params: serde_json::json!(["gid-001"]),
                        id: Some(serde_json::Value::String(format!("p{}", i))),
                    };
                    engine.handle_request(&pause_req).await.ok();
                    let unpause_req = JsonRpcRequest {
                        version: Some("2.0".into()),
                        method: "aria2.unpause".into(),
                        params: serde_json::json!(["gid-001"]),
                        id: Some(serde_json::Value::String(format!("u{}", i))),
                    };
                    engine.handle_request(&unpause_req).await.ok();
                }
            });
        });
    });
}

fn bench_get_global_stat_qps(c: &mut Criterion) {
    let engine = RpcEngine::new();
    let req = make_generic_req("gs", "aria2.getGlobalStat");

    c.bench_function("get_global_stat", |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async {
                let resp = engine.handle_request(&req).await;
                black_box(resp.is_success());
            });
        });
    });
}

fn bench_handle_request_generic(c: &mut Criterion) {
    let engine = RpcEngine::new();
    let methods = ["aria2.addUri", "aria2.getVersion", "aria2.getSessionInfo", "aria2.purgeDownloadResult"];

    c.bench(BenchmarkId::new("handle_request_4_methods"), |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async {
                for (i, m) in methods.iter().enumerate() {
                    let req = JsonRpcRequest {
                        version: Some("2.0".into()),
                        method: m.to_string(),
                        params: serde_json::json!([["http://ex.com/f"]]),
                        id: Some(serde_json::Value::String(format!("r{}", i))),
                    };
                    let resp = engine.handle_request(&req).await;
                    black_box(resp.is_success());
                }
            });
        });
    });
}

fn bench_jsonrpc_parse(c: &mut Criterion) {
    let json_str = r#"{"jsonrpc":"2.0","method":"aria2.addUri","params":[["http://example.com/file.zip"]],"id":"req-1"}"#;
    c.bench_function("jsonrpc_parse_request", |b| {
        b.iter_with_black_input(json_str, |s| {
            let req: Result<JsonRpcRequest, _> = serde_json::from_str(s);
            black_box(req.is_ok());
        });
    });
}

fn bench_jsonrpc_serialize(c: &mut Criterion) {
    let response = JsonRpcResponse {
        jsonrpc: "2.0".into(),
        result: Some(serde_json::Value::String("gid-001".into())),
        error: None,
        id: Some(serde_json::Value::String("req-1".into())),
    };
    c.bench_function("jsonrpc_serialize_response", |b| {
        b.iter(|| {
            let s = serde_json::to_string(&response);
            black_box(s.ok());
        });
    });
}

fn bench_xmlrpc_parse(c: &mut Criterion) {
    let xml_str = r#"<methodCall><methodName>system.listMethods</methodName><params></params></methodCall>"#;
    c.bench_function("xmlrpc_parse_methodcall", |b| {
        b.iter_with_black_input(xml_str, |s| {
            let req = aria2_rpc::xml_rpc::XmlRpcRequest::parse(s);
            black_box(req.is_ok());
        });
    });
}

fn bench_xmlrpc_serialize(c: &mut Criterion) {
    use aria2_rpc::xml_rpc::{XmlRpcResponse, XmlRpcValue};
    let resp = XmlRpcResponse::success(XmlRpcValue::String("result-data".into()), None);
    c.bench_function("xmlrpc_serialize_response", |b| {
        b.iter(|| {
            let xml = resp.to_xml();
            black_box(!xml.is_empty());
        });
    });
}

fn bench_base64_encode_decode(c: &mut Criterion) {
    let data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
    c.bench(BenchmarkId::new("base64_encode_decode_1KB"), |b| {
        b.iter_with_black_input(&data, |d| {
            let encoded = base64::engine::general_purpose::STANDARD.encode(d);
            let decoded = base64::engine::general_purpose::STANDARD.decode(&encoded).ok();
            black_box(decoded.map_or(0, |v| v.len()));
        });
    });
}

fn bench_auth_token_verify(c: &mut Criterion) {
    let auth = AuthConfig::default().with_token("my-secret-token-12345678");
    let valid_token = "my-secret-token-12345678";
    let invalid_tokens: Vec<String> = (0..100).map(|i| format!("wrong-token-{}", i)).collect();

    c.bench(BenchmarkId::new("auth_token_verify_101_calls"), |b| {
        b.iter_with_black_input(&invalid_tokens, |tokens| {
            let ok = auth.verify_token(valid_token);
            for t in tokens.iter() {
                let bad = auth.verify_token(t);
                std::hint::black_box(bad);
            }
            black_box(ok);
        });
    });
}

criterion_group!(rpc_benches,
    bench_add_uri_qps,
    bench_add_uri_batch,
    bench_tell_status_qps,
    bench_tell_active_empty,
    bench_pause_unpause_cycle,
    bench_get_global_stat_qps,
    bench_handle_request_generic,
    bench_jsonrpc_parse,
    bench_jsonrpc_serialize,
    bench_xmlrpc_parse,
    bench_xmlrpc_serialize,
    bench_base64_encode_decode,
    bench_auth_token_verify,
);

fn main() {
    let mut c = Criterion::default().sample_size(100).warm_up_time(std::time::Duration::from_millis(200));
    rpc_benches(&mut c);
}
