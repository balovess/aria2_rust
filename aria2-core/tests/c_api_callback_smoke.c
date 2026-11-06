#include "aria2_rust.h"

#include <stdatomic.h>
#include <stdint.h>

typedef struct CallbackState {
  _Atomic(uintptr_t) session;
  _Atomic(uint_fast64_t) gid;
  _Atomic(uint32_t) starts;
  _Atomic(uint32_t) errors;
  _Atomic(uint32_t) invalid_callbacks;
} CallbackState;

static int32_t on_download_event(Aria2RustSession *session, uint32_t event,
                                 uint64_t gid, void *user_data) {
  CallbackState *state = (CallbackState *)user_data;
  uintptr_t expected_session = atomic_load_explicit(&state->session,
                                                    memory_order_acquire);
  if ((uintptr_t)session != expected_session || gid == 0) {
    atomic_fetch_add_explicit(&state->invalid_callbacks, 1,
                              memory_order_relaxed);
  }
  atomic_store_explicit(&state->gid, gid, memory_order_relaxed);
  if (event == ARIA2_RUST_EVENT_DOWNLOAD_START) {
    atomic_fetch_add_explicit(&state->starts, 1, memory_order_relaxed);
  } else if (event == ARIA2_RUST_EVENT_DOWNLOAD_ERROR) {
    atomic_fetch_add_explicit(&state->errors, 1, memory_order_relaxed);
  }
  return 0;
}

int main(void) {
  CallbackState state = {0};
  if (aria2_rust_library_init() != 0) {
    return 1;
  }

  Aria2RustKeyValue options[] = {{"max-tries", "1"}};
  Aria2RustSession *session =
      aria2_rust_session_new_with_download_event_callback(
          options, 1, on_download_event, &state);
  if (session == NULL) {
    aria2_rust_library_deinit();
    return 2;
  }
  atomic_store_explicit(&state.session, (uintptr_t)session,
                        memory_order_release);

  const char *uris[] = {"http://127.0.0.1:1/c-api-callback-smoke"};
  uint64_t gid = 0;
  if (aria2_rust_add_uri(session, uris, 1, NULL, 0, &gid) != 0 || gid == 0) {
    aria2_rust_session_final(session);
    aria2_rust_library_deinit();
    return 3;
  }
  if (aria2_rust_run(session, 0) != 0) {
    aria2_rust_session_final(session);
    aria2_rust_library_deinit();
    return 4;
  }

  uint32_t starts = atomic_load_explicit(&state.starts, memory_order_relaxed);
  uint32_t errors = atomic_load_explicit(&state.errors, memory_order_relaxed);
  uint32_t invalid =
      atomic_load_explicit(&state.invalid_callbacks, memory_order_relaxed);
  uint_fast64_t callback_gid =
      atomic_load_explicit(&state.gid, memory_order_relaxed);
  int32_t finalized = aria2_rust_session_final(session);
  int32_t deinitialized = aria2_rust_library_deinit();

  return starts == 1 && errors == 1 && invalid == 0 && callback_gid == gid &&
                 finalized == 0 && deinitialized == 0
             ? 0
             : 5;
}
