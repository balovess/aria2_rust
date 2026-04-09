use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::xml_rpc::XmlRpcRequest;
use base64::Engine;
use criterion::{black_box, criterion_group, Criterion};

fn make_add_req(id: &str, uri: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.addUri".into(),
        params: serde_json::json!([[uri]]),
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
                .enable_all()
                .build()
                .unwrap();
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
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let resp = engine.handle_request(&req).await;
                black_box(resp.is_success());
            });
        });
    });
}

fn bench_get_global_stat(c: &mut Criterion) {
    let engine = RpcEngine::new();
    let req = make_generic_req("gs", "aria2.getGlobalStat");

    c.bench_function("get_global_stat", |b| {
        b.iter(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let resp = engine.handle_request(&req).await;
                black_box(resp.is_success());
            });
        });
    });
}

fn bench_jsonrpc_parse(c: &mut Criterion) {
    let json_str: String = r#"{"jsonrpc":"2.0","method":"aria2.addUri","params":[["http://example.com/file.zip"]],"id":"req-1"}"#.to_string();
    c.bench_function("jsonrpc_parse_request", |b| {
        b.iter(|| {
            let req: Result<JsonRpcRequest, _> = serde_json::from_str(&json_str);
            black_box(req.is_ok());
        });
    });
}

fn bench_jsonrpc_serialize(c: &mut Criterion) {
    let response = JsonRpcResponse {
        version: "2.0".into(),
        id: serde_json::Value::String("req-1".into()),
        result: Some(serde_json::Value::String("gid-001".into())),
        error: None,
    };
    c.bench_function("jsonrpc_serialize_response", |b| {
        b.iter(|| {
            let s = serde_json::to_string(&response);
            black_box(s.ok());
        });
    });
}

fn bench_xmlrpc_build_serialize(c: &mut Criterion) {
    c.bench_function("xmlrpc_build_and_serialize", |b| {
        b.iter(|| {
            let req = XmlRpcRequest::new("system.listMethods", vec![]);
            let xml = req.to_xml();
            black_box(xml.len());
        });
    });
}

fn bench_xmlrpc_response(c: &mut Criterion) {
    c.bench_function("xmlrpc_response_single", |b| {
        b.iter(|| {
            let resp = XmlRpcResponse::string_val("result-data");
            black_box(!resp.to_xml().is_empty());
        });
    });
}

fn bench_base64_encode_decode(c: &mut Criterion) {
    let data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
    c.bench_with_input(
        BenchmarkId::new("base64_encode_decode_1KB", 1024),
        &data,
        |b, d| {
            b.iter(|| {
                let encoded = base64::engine::general_purpose::STANDARD.encode(d);
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&encoded)
                    .ok();
                black_box(decoded.map_or(0, |v| v.len()));
            });
        },
    );
}

fn bench_auth_token_verify(c: &mut Criterion) {
    let auth = AuthConfig::default().with_token("my-secret-token-12345678");
    let valid_token = "my-secret-token-12345678";
    let invalid_tokens: Vec<String> = (0..100).map(|i| format!("wrong-token-{}", i)).collect();

    c.bench_with_input(
        BenchmarkId::new("auth_token_verify_101_calls", 101),
        &invalid_tokens,
        |b, tokens| {
            b.iter(|| {
                let ok = auth.verify_token(valid_token);
                for t in tokens.iter() {
                    let bad = auth.verify_token(t);
                    std::hint::black_box(bad);
                }
                black_box(ok);
            });
        },
    );
}

criterion_group!(
    rpc_benches,
    bench_add_uri_qps,
    bench_tell_active_empty,
    bench_get_global_stat,
    bench_jsonrpc_parse,
    bench_jsonrpc_serialize,
    bench_xmlrpc_build_serialize,
    bench_xmlrpc_response,
    bench_base64_encode_decode,
    bench_auth_token_verify,
);

fn main() {
    rpc_benches();
}
