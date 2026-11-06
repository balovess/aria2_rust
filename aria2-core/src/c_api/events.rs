use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::engine::download_event_hooks::{
    DownloadEvent, DownloadEventHooks, DownloadEventListener, DownloadEventListenerId,
};

use super::{
    ARIA2_RUST_EVENT_BT_DOWNLOAD_COMPLETE, ARIA2_RUST_EVENT_DOWNLOAD_COMPLETE,
    ARIA2_RUST_EVENT_DOWNLOAD_ERROR, ARIA2_RUST_EVENT_DOWNLOAD_PAUSE,
    ARIA2_RUST_EVENT_DOWNLOAD_START, ARIA2_RUST_EVENT_DOWNLOAD_STOP,
    Aria2RustDownloadEventCallback, Aria2RustSession,
};

struct CallbackState {
    enabled: bool,
    in_flight: usize,
}

struct CDownloadEventListener {
    session: usize,
    scope_id: u64,
    callback: Aria2RustDownloadEventCallback,
    user_data: usize,
    state: Mutex<CallbackState>,
    quiesced: Condvar,
}

impl CDownloadEventListener {
    fn lock_state(&self) -> MutexGuard<'_, CallbackState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn begin_callback(&self) -> bool {
        let mut state = self.lock_state();
        if !state.enabled {
            return false;
        }
        state.in_flight += 1;
        true
    }

    fn end_callback(&self) {
        let mut state = self.lock_state();
        state.in_flight -= 1;
        if state.in_flight == 0 {
            self.quiesced.notify_all();
        }
    }

    fn disable(&self) {
        self.lock_state().enabled = false;
    }

    fn wait_for_callbacks(&self) {
        let mut state = self.lock_state();
        while state.in_flight != 0 {
            state = self
                .quiesced
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl DownloadEventListener for CDownloadEventListener {
    fn on_download_event(&self, _event: DownloadEvent, _gid: &str) {}

    fn on_download_event_scoped(&self, event: DownloadEvent, gid: &str, scope_id: u64) {
        if scope_id != self.scope_id {
            return;
        }
        let Ok(gid) = u64::from_str_radix(gid, 16) else {
            return;
        };
        if !self.begin_callback() {
            return;
        }

        struct CallbackGuard<'a>(&'a CDownloadEventListener);
        impl Drop for CallbackGuard<'_> {
            fn drop(&mut self) {
                self.0.end_callback();
            }
        }
        let _guard = CallbackGuard(self);

        // The session and user-data pointers are opaque to the engine. The C
        // caller guarantees their lifetime through session finalization.
        unsafe {
            (self.callback)(
                self.session as *mut Aria2RustSession,
                event_code(event),
                gid,
                self.user_data as *mut c_void,
            );
        }
    }

    fn is_alive(&self) -> bool {
        self.lock_state().enabled
    }
}

/// Owns one process-wide event-bus registration for a C session.
pub(super) struct DownloadEventCallbackRegistration {
    id: DownloadEventListenerId,
    listener: Arc<CDownloadEventListener>,
}

impl DownloadEventCallbackRegistration {
    pub(super) fn new(
        session: *mut Aria2RustSession,
        scope_id: u64,
        user_data: *mut c_void,
        callback: Aria2RustDownloadEventCallback,
    ) -> Self {
        let listener = Arc::new(CDownloadEventListener {
            session: session as usize,
            scope_id,
            callback,
            user_data: user_data as usize,
            state: Mutex::new(CallbackState {
                enabled: true,
                in_flight: 0,
            }),
            quiesced: Condvar::new(),
        });
        let id = DownloadEventHooks::shared().add_listener_with_id(listener.clone());
        Self { id, listener }
    }
}

impl Drop for DownloadEventCallbackRegistration {
    fn drop(&mut self) {
        self.listener.disable();
        DownloadEventHooks::shared().remove_listener(self.id);
        self.listener.wait_for_callbacks();
    }
}

fn event_code(event: DownloadEvent) -> u32 {
    match event {
        DownloadEvent::Start => ARIA2_RUST_EVENT_DOWNLOAD_START,
        DownloadEvent::Pause => ARIA2_RUST_EVENT_DOWNLOAD_PAUSE,
        DownloadEvent::Stop => ARIA2_RUST_EVENT_DOWNLOAD_STOP,
        DownloadEvent::Complete => ARIA2_RUST_EVENT_DOWNLOAD_COMPLETE,
        DownloadEvent::Error => ARIA2_RUST_EVENT_DOWNLOAD_ERROR,
        DownloadEvent::BtComplete => ARIA2_RUST_EVENT_BT_DOWNLOAD_COMPLETE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_codes_match_the_original_aria2_contract() {
        assert_eq!(event_code(DownloadEvent::Start), 1);
        assert_eq!(event_code(DownloadEvent::Pause), 2);
        assert_eq!(event_code(DownloadEvent::Stop), 3);
        assert_eq!(event_code(DownloadEvent::Complete), 4);
        assert_eq!(event_code(DownloadEvent::Error), 5);
        assert_eq!(event_code(DownloadEvent::BtComplete), 6);
    }
}
