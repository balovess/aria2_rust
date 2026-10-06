use super::*;

impl TrackerAnnouncer {
    /// Attach the shared catalog and the public URLs owned by this command.
    pub fn set_public_tracker_catalog(
        &mut self,
        catalog: Arc<PublicTrackerList>,
        public_tracker_urls: HashSet<String>,
    ) {
        self.public_tracker_catalog = Some(catalog);
        self.public_tracker_urls = public_tracker_urls;
    }

    /// Return up to `limit` public catalog URLs that are not already in the
    /// torrent's own announce list or excluded by this download's options.
    pub async fn public_tracker_urls(&self, limit: usize) -> Vec<String> {
        let Some(catalog) = self.public_tracker_catalog.as_ref() else {
            return Vec::new();
        };
        let torrent_urls = &self.announce.announce_list();
        catalog
            .snapshot()
            .await
            .iter()
            .map(|entry| entry.url.clone())
            .filter(|url| {
                !torrent_urls.contains_url(url)
                    && !self
                        .excluded_tracker_urls
                        .iter()
                        .any(|excluded| excluded == "*" || excluded == url)
            })
            .take(limit)
            .collect()
    }

    /// Return currently available public URLs not already active in this
    /// torrent. The caller owns the bounded fan-out policy.
    pub async fn available_public_tracker_urls(
        &self,
        active_urls: &HashSet<String>,
        limit: usize,
    ) -> Vec<String> {
        let Some(catalog) = self.public_tracker_catalog.as_ref() else {
            return Vec::new();
        };
        let torrent_urls = &self.announce.announce_list();
        catalog
            .available_snapshot()
            .await
            .iter()
            .map(|entry| entry.url.clone())
            .filter(|url| {
                !active_urls.contains(url)
                    && !torrent_urls.contains_url(url)
                    && !self
                        .excluded_tracker_urls
                        .iter()
                        .any(|excluded| excluded == "*" || excluded == url)
            })
            .take(limit)
            .collect()
    }

    /// Construct an independently timed announcer for one public URL while
    /// preserving this download's transport, privacy, and announce settings.
    pub fn fork_public_tracker(&self, url: &str) -> Self {
        let mut fork = Self::new(&[vec![url.to_owned()]], &None);
        fork.http_tls.clone_from(&self.http_tls);
        fork.outbound_network_policy = Arc::clone(&self.outbound_network_policy);
        fork.websocket_options.clone_from(&self.websocket_options);
        fork.tracker_timeout_secs = self.tracker_timeout_secs;
        fork.tracker_connect_timeout_secs = self.tracker_connect_timeout_secs;
        fork.stopped_timeout = self.stopped_timeout;
        fork.excluded_tracker_urls
            .clone_from(&self.excluded_tracker_urls);
        fork.set_user_defined_interval(self.user_defined_interval);
        fork.set_announce_options(self.force_encryption, self.external_ip.clone());
        fork.set_tcp_port(self.tcp_port());
        if let Some(catalog) = self.public_tracker_catalog.as_ref() {
            fork.set_public_tracker_catalog(Arc::clone(catalog), HashSet::from([url.to_owned()]));
        }
        fork.runtime_state.clone_from(&self.runtime_state);
        fork.publish_runtime_snapshot();
        fork
    }

    pub(crate) fn subscribe_public_tracker_updates(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.public_tracker_catalog
            .as_ref()
            .map(|catalog| catalog.subscribe_updates())
    }

    /// Apply the download's tracker exclusion policy to future catalog merges.
    pub fn set_excluded_tracker_urls(&mut self, excluded: Vec<String>) {
        self.excluded_tracker_urls = excluded;
    }
}
