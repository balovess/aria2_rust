use aria2_rpc::backend::{
    BackendError, BackendMetadata, BackendRequest, BackendResponse, BackendResult, RpcBackend,
};
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::{JsonRpcRequest, JsonRpcResponse, parse_aria2_wire_document};
use aria2_rpc::server::AuthConfig;
use aria2_rpc::types::GlobalStat;
use aria2_rpc::xml_rpc::{XmlRpcRequest, XmlRpcResponse};
use async_trait::async_trait;
use base64::Engine;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group};
use futures::future::join_all;
use std::sync::Arc;
use std::time::Duration;

struct BenchBackend {
    mutation_delay: Duration,
}

#[async_trait]
impl RpcBackend for BenchBackend {
    fn metadata(&self) -> BackendMetadata {
        BackendMetadata::base(env!("CARGO_PKG_VERSION"))
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResult, BackendError> {
        match request {
            BackendRequest::ChangeGlobalOption { .. } => {
                if !self.mutation_delay.is_zero() {
                    tokio::time::sleep(self.mutation_delay).await;
                }
                Ok(BackendResult::response(BackendResponse::Ok))
            }
            BackendRequest::AddUri { .. } => Ok(BackendResult::response(BackendResponse::Gid(
                "0123456789abcdef".into(),
            ))),
            BackendRequest::TellActive { .. }
            | BackendRequest::TellWaiting { .. }
            | BackendRequest::TellStopped { .. } => Ok(BackendResult::response(
                BackendResponse::Statuses(Vec::new()),
            )),
            BackendRequest::GetGlobalStat => Ok(BackendResult::response(
                BackendResponse::GlobalStat(GlobalStat::default()),
            )),
            request => Err(BackendError::Unsupported(format!(
                "benchmark request: {request:?}"
            ))),
        }
    }
}

fn benchmark_engine() -> RpcEngine {
    RpcEngine::with_backend(Arc::new(BenchBackend {
        mutation_delay: Duration::ZERO,
    }))
}

fn benchmark_mutation_engine() -> RpcEngine {
    RpcEngine::with_backend(Arc::new(BenchBackend {
        mutation_delay: Duration::from_micros(100),
    }))
}

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

fn make_change_global_option_req(id: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.changeGlobalOption".into(),
        params: serde_json::json!([{"max-overall-download-limit": "1M"}]),
        id: Some(serde_json::Value::String(id.into())),
    }
}

fn bench_mutation_gate_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("rpc_mutation_gate");
    for count in [8usize, 32, 64] {
        group.bench_with_input(
            BenchmarkId::new("sequential", count),
            &count,
            |b, &count| {
                b.iter(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let engine = benchmark_mutation_engine();
                    rt.block_on(async {
                        for index in 0..count {
                            let response = engine
                                .handle_request(&make_change_global_option_req(&index.to_string()))
                                .await;
                            assert!(response.is_success());
                        }
                    });
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("concurrent", count),
            &count,
            |b, &count| {
                b.iter(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let engine = benchmark_mutation_engine();
                    rt.block_on(async {
                        let requests = (0..count)
                            .map(|index| make_change_global_option_req(&index.to_string()))
                            .collect::<Vec<_>>();
                        let responses = join_all(
                            requests
                                .iter()
                                .map(|request| engine.handle_request(request)),
                        )
                        .await;
                        black_box(responses.iter().all(JsonRpcResponse::is_success));
                    });
                });
            },
        );
    }
    group.finish();
}

fn bench_add_uri_qps(c: &mut Criterion) {
    let engine = benchmark_engine();
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
    let engine = benchmark_engine();
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
    let engine = benchmark_engine();
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

fn bench_read_only_multicall_poll(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let engine = benchmark_engine();
    rt.block_on(async {
        for index in 0..100 {
            let req = make_add_req(
                &format!("setup-{index}"),
                &format!("http://example.com/file-{index}.bin"),
            );
            let response = engine.handle_request(&req).await;
            assert!(response.is_success());
        }
    });
    let req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.tellActive", "params": []},
            {"methodName": "aria2.tellWaiting", "params": [0, 100]},
            {"methodName": "aria2.tellStopped", "params": [0, 100]},
            {"methodName": "aria2.getGlobalStat", "params": []}
        ]]),
    )
    .with_id("poll");

    c.bench_function("read_only_multicall_poll_100_tasks", |b| {
        b.iter(|| {
            let response = rt.block_on(engine.handle_request(&req));
            black_box(response.is_success());
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

fn bench_jsonrpc_streaming_wire_parse(c: &mut Criterion) {
    let json = br#"{"jsonrpc":"2.0","method":"aria2.tellActive","params":[],"id":"req-1"}"#;
    c.bench_function("jsonrpc_streaming_wire_parse", |b| {
        b.iter(|| black_box(parse_aria2_wire_document(json).is_ok()));
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

fn bench_jsonrpc_serialize_bytes(c: &mut Criterion) {
    let response = JsonRpcResponse {
        version: "2.0".into(),
        id: serde_json::Value::String("req-1".into()),
        result: Some(serde_json::Value::String("gid-001".into())),
        error: None,
    };
    c.bench_function("jsonrpc_serialize_response_bytes", |b| {
        b.iter(|| black_box(response.to_bytes().ok()));
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
    bench_mutation_gate_queue,
    bench_tell_active_empty,
    bench_get_global_stat,
    bench_read_only_multicall_poll,
    bench_jsonrpc_parse,
    bench_jsonrpc_streaming_wire_parse,
    bench_jsonrpc_serialize,
    bench_jsonrpc_serialize_bytes,
    bench_xmlrpc_build_serialize,
    bench_xmlrpc_response,
    bench_base64_encode_decode,
    bench_auth_token_verify,
);

fn main() {
    rpc_benches();
}
