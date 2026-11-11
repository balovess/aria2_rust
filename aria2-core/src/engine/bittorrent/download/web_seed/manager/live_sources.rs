//! Live per-file URI resolution and WebSeed client reuse.

use std::sync::Arc;

use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::super::client::WebSeedClient;
use super::super::uri_state::UriProbeAdmission;
use super::WebSeedManager;

impl WebSeedManager {
    pub(super) async fn request_live_piece(
        &self,
        group: &Arc<std::sync::RwLock<RequestGroup>>,
        piece_index: u32,
        piece_data_length: u64,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<Vec<u8>, String> {
        if piece_data_length == 0 {
            return Err("Cannot request an empty BitTorrent piece".to_string());
        }
        let piece_start = piece_index as u64 * self.piece_length as u64;
        let piece_end = piece_start.saturating_add(piece_data_length);
        let (uri_generation, context, file_ranges) = {
            let group = group.recover();
            let uri_generation = group.uri_generation();
            let context = group
                .get_download_context()
                .ok_or_else(|| "BitTorrent download context is unavailable".to_string())?;
            let entries = context.get_file_entries();
            let first = entries.partition_point(|entry| entry.last_offset() <= piece_start);
            let file_ranges = entries[first..]
                .iter()
                .enumerate()
                .take_while(|(_, entry)| entry.offset() < piece_end)
                .filter_map(|(index, entry)| {
                    let start = piece_start.max(entry.offset());
                    let end = piece_end.min(entry.last_offset());
                    (start < end).then(|| (first + index, start, end, entry.offset(), entry.uris()))
                })
                .collect::<Vec<_>>();
            (uri_generation, context, file_ranges)
        };
        self.uri_state.observe_generation(uri_generation);

        if file_ranges.is_empty() {
            return Err(format!("Piece {piece_index} has no file-backed byte range"));
        }

        let piece_length = usize::try_from(piece_data_length)
            .map_err(|_| "WebSeed piece exceeds addressable memory".to_string())?;
        let mut piece = vec![0; piece_length];
        let mut last_error = None;
        for (file_entry_index, start, end, file_offset, uris) in file_ranges {
            if uris.is_empty() {
                return Err(format!(
                    "No WebSeed URI is configured for piece {piece_index} file range {start}..{end}"
                ));
            }
            let length = end - start;
            let output_start = usize::try_from(start - piece_start)
                .map_err(|_| "WebSeed piece offset exceeds addressable memory".to_string())?;
            let output_length = usize::try_from(length)
                .map_err(|_| "WebSeed file range exceeds addressable memory".to_string())?;
            let output_end = output_start
                .checked_add(output_length)
                .filter(|&end| end <= piece.len())
                .ok_or_else(|| "WebSeed file range exceeds piece buffer".to_string())?;
            let mut range_received = false;
            for uri in &uris {
                let mut probe_guard = match self
                    .uri_state
                    .admit_request(uri, Some(uri_generation))
                    .await
                {
                    UriProbeAdmission::Unavailable => continue,
                    UriProbeAdmission::Proceed => None,
                    UriProbeAdmission::Probe(guard) => Some(guard),
                };
                let client = match self.live_client(uri).await {
                    Ok(client) => client,
                    Err(error) => {
                        if probe_guard.is_some() {
                            self.uri_state.mark_probed(uri, Some(uri_generation));
                        }
                        last_error = Some(format!("{uri}: {error}"));
                        continue;
                    }
                };
                if !context
                    .get_file_entries()
                    .get(file_entry_index)
                    .is_some_and(|entry| entry.mark_uri_dispatched(uri))
                {
                    if probe_guard.is_some() {
                        self.uri_state.mark_probed(uri, Some(uri_generation));
                    }
                    last_error = Some(format!("{uri}: URI was removed before the request started"));
                    continue;
                }
                let result = client
                    .download_piece_into_with_response_status(
                        piece_index,
                        start - file_offset,
                        &mut piece[output_start..output_end],
                        network_activity,
                        |status| {
                            if status == 404 {
                                self.uri_state.mark_unavailable(uri, Some(uri_generation));
                            } else if probe_guard.is_some() {
                                self.uri_state.mark_probed(uri, Some(uri_generation));
                            }
                            drop(probe_guard.take());
                        },
                    )
                    .await;
                match result {
                    Ok(received) if received as u64 == length => {
                        if probe_guard.is_some() {
                            self.uri_state.mark_probed(uri, Some(uri_generation));
                        }
                        range_received = true;
                        break;
                    }
                    Ok(received) => {
                        if probe_guard.is_some() {
                            self.uri_state.mark_probed(uri, Some(uri_generation));
                        }
                        last_error = Some(format!(
                            "{uri}: expected {length} bytes, received {}",
                            received
                        ));
                    }
                    Err(error) => {
                        if probe_guard.take().is_some() {
                            self.uri_state.mark_probed(uri, Some(uri_generation));
                        }
                        last_error = Some(format!("{uri}: {error}"));
                    }
                }
            }

            if !range_received {
                return Err(format!(
                    "All WebSeeds failed for piece {piece_index} range {start}..{end}: {}",
                    last_error.unwrap_or_else(|| "no usable endpoint".to_string())
                ));
            }
        }
        Ok(piece)
    }

    async fn live_client(&self, uri: &str) -> Result<WebSeedClient, String> {
        let parsed = reqwest::Url::parse(uri).map_err(|error| format!("invalid URI: {error}"))?;
        let scheme = parsed.scheme();
        if !matches!(scheme, "http" | "https") {
            return Err(format!("unsupported WebSeed scheme: {scheme}"));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| "WebSeed URI has no host".to_string())?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| "WebSeed URI has no port".to_string())?;
        let policy = self
            .network_policy
            .as_ref()
            .ok_or_else(|| "WebSeed network policy is unavailable".to_string())?;
        let local_address = if policy.is_direct() {
            None
        } else {
            policy
                .source_for_host(host, port)
                .await
                .map_err(|error| format!("WebSeed source selection failed: {error}"))?
        };
        let origin = format!("{scheme}://{host}:{port}/{local_address:?}");
        let client = {
            let mut clients = self.http_clients.lock().await;
            if let Some(client) = clients.get(&origin) {
                client.clone()
            } else {
                let client = super::super::client::build_client(
                    &self.tls,
                    local_address,
                    self.connect_timeout,
                )?;
                clients.insert(origin, client.clone());
                client
            }
        };
        Ok(WebSeedClient::with_shared_http_client(
            uri,
            Arc::clone(&self.stats),
            client,
            self.request_timeout,
        ))
    }
}
