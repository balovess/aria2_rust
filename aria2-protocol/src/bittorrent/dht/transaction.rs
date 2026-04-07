use std::collections::HashMap;
use std::time::{Duration, Instant};

const TRANSACTION_TIMEOUT_SECS: u64 = 30;

struct PendingTransaction {
    created_at: Instant,
    callback: Box<dyn FnOnce(&[u8]) + Send>,
}

pub struct TransactionManager {
    transactions: HashMap<Vec<u8>, PendingTransaction>,
    next_tx_id: u32,
}

impl TransactionManager {
    pub fn new() -> Self {
        Self {
            transactions: HashMap::new(),
            next_tx_id: 1,
        }
    }

    pub fn allocate<F>(&mut self, callback: F) -> Vec<u8>
    where
        F: FnOnce(&[u8]) + Send + 'static,
    {
        let tx_id = self.next_tx_id;
        self.next_tx_id = self.next_tx_id.wrapping_add(1);
        let key = tx_id.to_be_bytes().to_vec();

        self.transactions.insert(key.clone(), PendingTransaction {
            created_at: Instant::now(),
            callback: Box::new(callback),
        });

        key
    }

    pub fn handle_response(&mut self, tx_id: &[u8], response_data: &[u8]) -> bool {
        let key = tx_id.to_vec();
        if let Some(pending) = self.transactions.remove(&key) {
            (pending.callback)(response_data);
            true
        } else {
            false
        }
    }

    pub fn cleanup_expired(&mut self) -> usize {
        let before = self.transactions.len();
        self.transactions.retain(|_, pending| {
            pending.created_at.elapsed().as_secs() < TRANSACTION_TIMEOUT_SECS
        });
        before - self.transactions.len()
    }

    pub fn pending_count(&self) -> usize {
        self.transactions.len()
    }
}

impl Default for TransactionManager {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_allocate_and_handle() {
        let mut mgr = TransactionManager::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        let tx_id = mgr.allocate(move |_data| {
            called_clone.store(true, Ordering::SeqCst);
        });

        assert_eq!(mgr.pending_count(), 1);
        mgr.handle_response(&tx_id, &[0xFF]);
        assert!(called.load(Ordering::SeqCst));
        assert_eq!(mgr.pending_count(), 0);
    }

    #[test]
    fn test_unknown_transaction() {
        let mut mgr = TransactionManager::new();
        assert!(!mgr.handle_response(&[0, 0, 0, 99], &[]));
    }

    #[test]
    fn test_unique_ids() {
        let mut mgr = TransactionManager::new();
        let id1 = mgr.allocate(|_| {});
        let id2 = mgr.allocate(|_| {});
        assert_ne!(id1, id2);
    }
}
