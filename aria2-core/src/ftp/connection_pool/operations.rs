use std::collections::HashSet;
use std::time::Instant;

use tracing::{debug, info};

use crate::error::Result;
use crate::ftp::connection::FtpMode;

use super::{
    ConnectionKey, ControlSessionKey, FtpConnectionPool, PoolKey, PoolState, PooledConnection,
    PooledControlSession,
};

impl FtpConnectionPool {
    fn find_matching_raw_key(
        state: &PoolState,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        base_working_dir: Option<&str>,
    ) -> Option<ConnectionKey> {
        let mut preauthenticated = None;
        for key in state.connections.keys() {
            if key.host != host
                || key.port != port
                || key.username != username
                || base_working_dir.is_some_and(|base| key.base_working_dir != base)
            {
                continue;
            }

            if key.password == password {
                return Some(key.clone());
            }
            if key.password.is_empty() {
                preauthenticated = Some(key.clone());
            }
        }
        preauthenticated
    }

    fn evict_oldest(state: &mut PoolState, max_connections: usize) -> usize {
        let mut evicted = 0;
        while state.connections.len() + state.control_sessions.len() >= max_connections {
            let Some((index, key)) = state
                .lru_order
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(index, entry)| (index, entry.key.clone()))
            else {
                break;
            };
            state.lru_order.remove(index);
            match key {
                PoolKey::Raw(key) => {
                    state.connections.remove(&key);
                }
                PoolKey::Control(key) => {
                    state.control_sessions.remove(&key);
                }
            }
            evicted += 1;
        }
        evicted
    }

    fn touch_lru(state: &mut PoolState, key: PoolKey) {
        state.lru_order.retain(|entry| entry.key != key);
        state.lru_order.push(super::LruEntry {
            key,
            last_access: Instant::now(),
        });
    }

    fn remove_lru(state: &mut PoolState, key: &PoolKey) {
        state.lru_order.retain(|entry| &entry.key != key);
    }

    fn current_size(state: &PoolState) -> usize {
        state.connections.len() + state.control_sessions.len()
    }

    /// Try to get a healthy raw TCP connection from the standalone pool API.
    ///
    /// The connection is removed from the pool and must be returned via
    /// `return_connection()` when done.
    pub async fn try_get(
        &self,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        base_working_dir: &str,
    ) -> Option<PooledConnection> {
        let mut state = self.state.lock().await;
        let key = Self::find_matching_raw_key(
            &state,
            host,
            port,
            username,
            password,
            Some(base_working_dir),
        )?;
        let reusable = state.connections.get(&key).is_some_and(|connection| {
            connection.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        });
        if !reusable {
            state.connections.remove(&key);
            Self::remove_lru(&mut state, &PoolKey::Raw(key));
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            stats.current_size = Self::current_size(&state);
            return None;
        }

        let mut connection = state.connections.remove(&key)?;
        connection.mark_used();
        Self::remove_lru(&mut state, &PoolKey::Raw(key));
        let mut stats = self.stats.lock().await;
        stats.connections_reused += 1;
        stats.current_size = Self::current_size(&state);
        Some(connection)
    }

    /// Try to get a raw connection matching endpoint and user while allowing
    /// the caller to handle any working-directory transition.
    pub async fn try_get_relaxed(
        &self,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> Option<PooledConnection> {
        let mut state = self.state.lock().await;
        let key = Self::find_matching_raw_key(&state, host, port, username, password, None)?;
        let reusable = state.connections.get(&key).is_some_and(|connection| {
            connection.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        });
        if !reusable {
            state.connections.remove(&key);
            Self::remove_lru(&mut state, &PoolKey::Raw(key));
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            stats.current_size = Self::current_size(&state);
            return None;
        }

        let mut connection = state.connections.remove(&key)?;
        connection.mark_used();
        Self::remove_lru(&mut state, &PoolKey::Raw(key));
        let mut stats = self.stats.lock().await;
        stats.connections_reused += 1;
        stats.current_size = Self::current_size(&state);
        Some(connection)
    }

    /// Lease a protocol-owned FTP control session using its complete transport
    /// identity. Authentication, TLS, proxy routing, and source policy are
    /// retained with the session and cannot be crossed between keys.
    pub(crate) async fn try_get_control_session(
        &self,
        key: &ControlSessionKey,
    ) -> Option<PooledControlSession> {
        let mut state = self.state.lock().await;
        let reusable = state.control_sessions.get(key).is_some_and(|session| {
            session.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        });
        if !reusable {
            state.control_sessions.remove(key);
            Self::remove_lru(&mut state, &PoolKey::Control(key.clone()));
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            stats.current_size = Self::current_size(&state);
            return None;
        }

        let mut session = state.control_sessions.remove(key)?;
        session.mark_used();
        let reuse_count = session.reuse_count();
        Self::remove_lru(&mut state, &PoolKey::Control(key.clone()));
        let mut stats = self.stats.lock().await;
        stats.connections_reused += 1;
        stats.current_size = Self::current_size(&state);
        debug!(reuse_count, "Leased reusable FTP control session");
        Some(session)
    }

    /// Return an authenticated protocol control session to the process pool.
    pub(crate) async fn return_control_session(
        &self,
        key: ControlSessionKey,
        session: PooledControlSession,
    ) {
        if self.config.max_connections == 0
            || !session.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        {
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            return;
        }

        let mut state = self.state.lock().await;
        let is_new_session = session.reuse_count() == 0;
        state.control_sessions.remove(&key);
        Self::remove_lru(&mut state, &PoolKey::Control(key.clone()));
        let evicted = Self::evict_oldest(&mut state, self.config.max_connections);
        state.control_sessions.insert(key.clone(), session);
        Self::touch_lru(&mut state, PoolKey::Control(key));
        let current_size = Self::current_size(&state);
        let mut stats = self.stats.lock().await;
        stats.connections_created += u64::from(is_new_session);
        stats.connections_evicted += evicted as u64;
        stats.current_size = current_size;
        stats.peak_size = stats.peak_size.max(current_size);
    }

    /// Return a raw TCP control connection to the pool.
    ///
    /// Matches the standalone API's historical raw-stream contract.
    pub async fn return_raw_connection(
        &self,
        stream: tokio::net::TcpStream,
        host: &str,
        port: u16,
        username: &str,
        mode: FtpMode,
        base_working_dir: &str,
    ) -> Result<()> {
        if self.config.max_connections == 0 {
            return Ok(());
        }

        let key = ConnectionKey::new(host, port, username, "", base_working_dir);
        let connection = PooledConnection::new(stream, key.clone(), mode, self.config.read_timeout);
        let mut state = self.state.lock().await;
        state.connections.remove(&key);
        Self::remove_lru(&mut state, &PoolKey::Raw(key.clone()));
        let evicted = Self::evict_oldest(&mut state, self.config.max_connections);
        state.connections.insert(key.clone(), connection);
        Self::touch_lru(&mut state, PoolKey::Raw(key.clone()));
        let current_size = Self::current_size(&state);
        let mut stats = self.stats.lock().await;
        stats.connections_created += 1;
        stats.connections_evicted += evicted as u64;
        stats.current_size = current_size;
        stats.peak_size = stats.peak_size.max(current_size);
        info!(key = %key.to_pool_key_string(), "FTP raw control connection pooled");
        Ok(())
    }

    /// Return a leased standalone raw connection to the pool.
    pub async fn return_connection(&self, mut connection: PooledConnection) {
        if self.config.max_connections == 0
            || !connection.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        {
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            return;
        }

        connection.mark_used();
        let key = connection.key.clone();
        let mut state = self.state.lock().await;
        state.connections.remove(&key);
        Self::remove_lru(&mut state, &PoolKey::Raw(key.clone()));
        let evicted = Self::evict_oldest(&mut state, self.config.max_connections);
        state.connections.insert(key.clone(), connection);
        Self::touch_lru(&mut state, PoolKey::Raw(key.clone()));
        let current_size = Self::current_size(&state);
        let mut stats = self.stats.lock().await;
        stats.connections_evicted += evicted as u64;
        stats.current_size = current_size;
        stats.peak_size = stats.peak_size.max(current_size);
        debug!(key = %key.to_pool_key_string(), "Returned FTP raw connection to pool");
    }

    /// Remove stale raw and protocol-owned sessions.
    pub async fn cleanup_stale(&self) {
        let mut state = self.state.lock().await;
        let before = Self::current_size(&state);
        state.connections.retain(|_, connection| {
            connection.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        });
        state.control_sessions.retain(|_, session| {
            session.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        });
        let current_size = Self::current_size(&state);
        let live_keys: HashSet<_> = state
            .connections
            .keys()
            .cloned()
            .map(PoolKey::Raw)
            .chain(state.control_sessions.keys().cloned().map(PoolKey::Control))
            .collect();
        state
            .lru_order
            .retain(|entry| live_keys.contains(&entry.key));
        let removed = before.saturating_sub(current_size);
        let mut stats = self.stats.lock().await;
        stats.connections_evicted += removed as u64;
        stats.current_size = current_size;
        debug!(
            current_size,
            removed, "FTP connection pool cleanup completed"
        );
    }

    /// Get current raw and protocol-session pool size.
    pub async fn size(&self) -> usize {
        let state = self.state.lock().await;
        Self::current_size(&state)
    }

    /// Clear every pooled raw stream and protocol control session.
    pub async fn clear(&self) {
        let mut state = self.state.lock().await;
        let count = Self::current_size(&state);
        state.connections.clear();
        state.control_sessions.clear();
        state.lru_order.clear();
        let mut stats = self.stats.lock().await;
        stats.connections_evicted += count as u64;
        stats.current_size = 0;
        info!(count, "FTP connection pool cleared");
    }

    /// Check whether a reusable raw or protocol control session exists for
    /// the endpoint and username.
    pub async fn has_connection(&self, host: &str, port: u16, username: &str) -> bool {
        let state = self.state.lock().await;
        state.connections.iter().any(|(key, connection)| {
            key.host == host
                && key.port == port
                && key.username == username
                && connection.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        }) || state.control_sessions.iter().any(|(key, session)| {
            key.host == host
                && key.port == port
                && key.username == username
                && session.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
        })
    }
}
