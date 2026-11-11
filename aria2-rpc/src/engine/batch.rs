use std::sync::Arc;

use futures::StreamExt;

use crate::backend::BackendReadSnapshot;
use crate::handlers::backend_error;
use crate::json_rpc::{JsonRpcResponse, JsonRpcWireEntry};

use super::{RpcEngine, rpc_method_is_read_only, rpc_method_uses_read_snapshot};

const MAX_RPC_BATCH_CONCURRENCY: usize = 64;

fn read_run_needs_snapshot(entries: &[JsonRpcWireEntry]) -> bool {
    entries
        .iter()
        .filter_map(|entry| match entry {
            JsonRpcWireEntry::Request(request) => Some(request.method.as_str()),
            JsonRpcWireEntry::Error(_) => None,
        })
        .filter(|method| rpc_method_uses_read_snapshot(method))
        .count()
        > 1
}

async fn dispatch_wire_entry(
    engine: &RpcEngine,
    entry: JsonRpcWireEntry,
    snapshot: Option<Arc<BackendReadSnapshot>>,
) -> JsonRpcResponse {
    match entry {
        JsonRpcWireEntry::Request(request) => {
            engine
                .handle_request_owned_with_snapshot(request, snapshot)
                .await
        }
        JsonRpcWireEntry::Error(response) => response,
    }
}

async fn dispatch_read_only_run(
    engine: &RpcEngine,
    entries: Vec<JsonRpcWireEntry>,
) -> Vec<JsonRpcResponse> {
    let snapshot = if read_run_needs_snapshot(&entries) {
        match engine.backend.capture_read_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return entries
                    .into_iter()
                    .map(|entry| {
                        let id = match entry {
                            JsonRpcWireEntry::Request(request) => request.id,
                            JsonRpcWireEntry::Error(response) => Some(response.id),
                        };
                        backend_error(error.clone()).into_response(id)
                    })
                    .collect();
            }
        }
    } else {
        None
    };

    futures::stream::iter(entries)
        .map(|entry| dispatch_wire_entry(engine, entry, snapshot.clone()))
        .buffered(MAX_RPC_BATCH_CONCURRENCY)
        .collect()
        .await
}

pub(crate) async fn dispatch_wire_entries(
    engine: &RpcEngine,
    entries: Vec<JsonRpcWireEntry>,
) -> Vec<JsonRpcResponse> {
    let mut responses = Vec::with_capacity(entries.len());
    let mut read_only_run = Vec::new();
    for entry in entries {
        let read_only = match &entry {
            JsonRpcWireEntry::Request(request) => rpc_method_is_read_only(&request.method),
            JsonRpcWireEntry::Error(_) => true,
        };
        if read_only {
            read_only_run.push(entry);
        } else {
            if !read_only_run.is_empty() {
                responses.extend(
                    dispatch_read_only_run(engine, std::mem::take(&mut read_only_run)).await,
                );
            }
            responses.push(dispatch_wire_entry(engine, entry, None).await);
        }
    }
    if !read_only_run.is_empty() {
        responses.extend(dispatch_read_only_run(engine, read_only_run).await);
    }
    responses
}
