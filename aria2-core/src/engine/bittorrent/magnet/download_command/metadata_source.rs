//! Resolve magnet metadata from exact sources, trackers, DHT, and BEP 9 peers.

use super::{MAX_MAGNET_METADATA_PEERS_TO_TRY, MagnetDownloadCommand};
use crate::engine::bittorrent::magnet::metadata_exchange::{
    MetadataExchangeConfig, MetadataExchangeSession,
};
use crate::engine::http::client_config::{ProxyTarget, add_reqwest_proxy};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::client_identity::ClientTlsConfig;
use crate::http::socks_connector::ProxyUrl;
use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
impl MagnetDownloadCommand {
    pub(super) async fn fetch_magnet_exact_source(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        options: &DownloadOptions,
    ) -> Option<Vec<u8>> {
        let sources = magnet.exact_sources.iter().filter_map(|source| {
            let url = reqwest::Url::parse(source).ok()?;
            matches!(url.scheme(), "file" | "http" | "https").then_some((source, url))
        });
        let sources: Vec<_> = sources.collect();
        if sources.is_empty() {
            return None;
        }

        let request_timeout = options.timeout.unwrap_or(options.bt_tracker_timeout).max(1);
        let connect_timeout = options
            .connect_timeout
            .unwrap_or(options.bt_tracker_connect_timeout)
            .max(1);

        let client = if sources
            .iter()
            .any(|(_, url)| matches!(url.scheme(), "http" | "https"))
        {
            match Self::build_magnet_exact_source_client(
                options,
                request_timeout,
                connect_timeout,
                &self.outbound_network_policy,
            ) {
                Ok(client) => Some(client),
                Err(error) => {
                    warn!(%error, "Magnet exact-source HTTP client creation failed");
                    None
                }
            }
        } else {
            None
        };

        for (source, url) in sources {
            let body = if url.scheme() == "file" {
                let path = match url.to_file_path() {
                    Ok(path) => path,
                    Err(()) => {
                        warn!(source = %source, "Magnet exact-source file URL has no local path");
                        continue;
                    }
                };
                match tokio::fs::read(&path).await {
                    Ok(body) => body,
                    Err(error) => {
                        warn!(source = %source, path = %path.display(), %error, "Magnet exact-source file read failed");
                        continue;
                    }
                }
            } else {
                let Some(client) = client.as_ref() else {
                    continue;
                };
                let response = match Self::request_magnet_exact_source(client, &url, options).await
                {
                    Ok(response) => response,
                    Err(error) => {
                        warn!(source = %source, %error, "Magnet exact-source request failed");
                        continue;
                    }
                };
                if !response.status().is_success() {
                    warn!(
                        source = %source,
                        status = %response.status(),
                        "Magnet exact-source request returned an error status"
                    );
                    continue;
                }

                match response.bytes().await {
                    Ok(body) => body.to_vec(),
                    Err(error) => {
                        warn!(source = %source, %error, "Magnet exact-source response read failed");
                        continue;
                    }
                }
            };
            match Self::metadata_matches_magnet(magnet, &body) {
                Ok(()) => {
                    info!(source = %source, bytes = body.len(), "Loaded magnet metadata from exact source");
                    return Some(body);
                }
                Err(error) => {
                    warn!(source = %source, %error, "Ignoring exact-source metadata with mismatched info-hash");
                }
            }
        }

        None
    }

    pub(super) fn build_magnet_exact_source_client(
        options: &DownloadOptions,
        request_timeout: u64,
        connect_timeout: u64,
        policy: &crate::network::OutboundNetworkPolicy,
    ) -> std::result::Result<reqwest::Client, String> {
        crate::http::client_pool::ensure_rustls_provider();
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(request_timeout))
            .connect_timeout(Duration::from_secs(connect_timeout))
            .gzip(false)
            .user_agent(crate::constants::USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(5));
        if let Some(address) = policy.addresses().into_iter().next() {
            builder = builder.local_address(address);
        }
        let no_proxy = options.no_proxy.as_deref();

        if let Some(proxy) = options
            .http_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
        {
            builder = add_reqwest_proxy(
                builder,
                ProxyTarget::Http,
                proxy,
                options.proxy_credentials_for_scheme("http"),
                no_proxy,
            );
        }
        if let Some(proxy) = options
            .https_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
        {
            builder = add_reqwest_proxy(
                builder,
                ProxyTarget::Https,
                proxy,
                options.proxy_credentials_for_scheme("https"),
                no_proxy,
            );
        }
        if let Some(proxy) = options
            .all_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
            && matches!(
                ProxyUrl::parse(proxy).map(|parsed| parsed.protocol),
                Ok(crate::http::socks_connector::ProxyProtocol::Http)
                    | Ok(crate::http::socks_connector::ProxyProtocol::Https)
            )
        {
            builder = add_reqwest_proxy(
                builder,
                ProxyTarget::All,
                proxy,
                options.proxy_credentials_for_scheme("all"),
                no_proxy,
            );
        }

        let tls = ClientTlsConfig::from_download_options(options);
        let builder = crate::http::client_identity::apply(builder, &tls)
            .map_err(|error| error.to_string())?;
        builder
            .build()
            .map_err(|error| format!("failed to build exact-source HTTP client: {error}"))
    }

    pub(super) async fn request_magnet_exact_source(
        client: &reqwest::Client,
        url: &reqwest::Url,
        options: &DownloadOptions,
    ) -> std::result::Result<reqwest::Response, String> {
        let auth_context = crate::engine::http::auth::from_options(options, url.scheme());
        let (mut auth_factory, auth_options) = (auth_context.factory, auth_context.options);
        let mut request = client.get(url.clone());
        if let Some(authorization) = auth_factory.resolve_basic_authorization(url, &auth_options) {
            request = request.header(reqwest::header::AUTHORIZATION, authorization);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED || !options.http_auth_challenge {
            return Ok(response);
        }

        let challenge_header = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let scheme = challenge_header
            .as_deref()
            .and_then(crate::http::AuthScheme::from_header)
            .unwrap_or(crate::http::AuthScheme::Basic);
        let challenge = crate::http::HttpAuthChallenge {
            scheme: scheme.clone(),
            realm: challenge_header
                .as_deref()
                .map(crate::http::HttpSkipResponseHandler::extract_realm)
                .unwrap_or_default(),
            is_proxy: false,
            digest_challenge: if scheme == crate::http::AuthScheme::Digest {
                challenge_header.as_deref().and_then(|header| {
                    crate::http::digest_auth::DigestAuthChallenge::parse(header).ok()
                })
            } else {
                None
            },
        };
        let auth_result = crate::http::handle_auth_challenge(
            &challenge,
            &mut auth_factory,
            url,
            &auth_options,
            crate::http::request::HttpMethod::Get,
            false,
            1,
        );
        let crate::http::AuthChallengeResult::RetryWithAuth {
            authorization_header,
            is_proxy: false,
        } = auth_result
        else {
            return Ok(response);
        };

        client
            .get(url.clone())
            .header(reqwest::header::AUTHORIZATION, authorization_header)
            .send()
            .await
            .map_err(|error| error.to_string())
    }

    /// Add web seeds from the magnet URI to the resolved torrent metadata.
    ///
    /// `ws` is a root-level torrent field, so adding it does not change the
    /// info dictionary or its v1/v2 info-hash. Keeping it in the resolved
    /// metadata lets the existing BitTorrent context and web-seed manager
    /// consume it without introducing a second magnet-only configuration
    /// path.
    pub(super) async fn fetch_magnet_metadata(
        &mut self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    ) -> Result<Vec<u8>> {
        let (enable_dht, options) = {
            let group = self.group.recover();
            (group.options().enable_dht, group.options().clone())
        };

        // `xs` points to complete torrent metadata. Try it before starting
        // DHT so a magnet with a working exact source does not pay the DHT
        // bootstrap/lookup cost at all.
        if let Some(metadata) = self.fetch_magnet_exact_source(magnet, &options).await {
            return Ok(metadata);
        }

        let outbound_network_policy = Arc::clone(&self.outbound_network_policy);
        let metadata_session = |max_peers_to_try| {
            MetadataExchangeSession::new(MetadataExchangeConfig {
                max_peers_to_try,
                connect_timeout: Duration::from_secs(15),
                request_timeout: Duration::from_secs(10),
                piece_size: 16 * 1024,
                ..MetadataExchangeConfig::default()
            })
            .with_outbound_network_policy(Arc::clone(&outbound_network_policy))
        };

        // Magnet links commonly carry tracker URLs, and tracker discovery is
        // available even when DHT bootstrap is blocked by NAT or a firewall.
        // Try those peers first so a working tracker does not wait for a DHT
        // lookup that may never produce a result.
        let tracker_peers = self.discover_magnet_tracker_peers(magnet, &options).await;
        let mut last_error = None;
        if !tracker_peers.is_empty() {
            match metadata_session(tracker_peers.len().min(MAX_MAGNET_METADATA_PEERS_TO_TRY))
                .fetch_metadata(&magnet.info_hash, &tracker_peers)
                .await
            {
                Ok(metadata) => return Ok(metadata),
                Err(error) => {
                    warn!(
                        "Magnet: metadata fetch from tracker peers failed: {}",
                        error
                    );
                    last_error = Some(error.to_string());
                }
            }
        }

        if enable_dht || options.enable_dht6 {
            self.ensure_dht_engines(&options).await?;
        }
        let dht_peers = if !self.dht_engines.is_empty() {
            self.discover_magnet_peers(&self.dht_engines, &magnet.info_hash)
                .await
        } else {
            warn!("Magnet: DHT disabled and tracker discovery returned no usable peers");
            Vec::new()
        };

        if !dht_peers.is_empty() {
            match metadata_session(dht_peers.len().min(MAX_MAGNET_METADATA_PEERS_TO_TRY))
                .fetch_metadata(&magnet.info_hash, &dht_peers)
                .await
            {
                Ok(metadata) => return Ok(metadata),
                Err(error) => last_error = Some(error.to_string()),
            }
        }

        let message = last_error.map_or_else(
            || "No peers found via trackers or DHT".to_string(),
            |error| format!("Metadata fetch failed after tracker and DHT discovery: {error}"),
        );
        Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure { message },
        ))
    }
}
