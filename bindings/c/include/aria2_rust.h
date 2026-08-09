#ifndef ARIA2_RUST_H
#define ARIA2_RUST_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Aria2RustSession Aria2RustSession;

typedef struct Aria2RustKeyValue {
  const char *name;
  const char *value;
} Aria2RustKeyValue;

enum {
  ARIA2_RUST_DOWNLOAD_ACTIVE = 0,
  ARIA2_RUST_DOWNLOAD_WAITING = 1,
  ARIA2_RUST_DOWNLOAD_PAUSED = 2,
  ARIA2_RUST_DOWNLOAD_COMPLETE = 3,
  ARIA2_RUST_DOWNLOAD_ERROR = 4,
  ARIA2_RUST_DOWNLOAD_REMOVED = 5
};

typedef struct Aria2RustDownloadInfo {
  uint32_t status;
  uint64_t total_length;
  uint64_t completed_length;
  uint64_t upload_length;
  uint64_t download_speed;
  uint64_t upload_speed;
  uint32_t error_code;
} Aria2RustDownloadInfo;

typedef struct Aria2RustGlobalStat {
  uint64_t download_speed;
  uint64_t upload_speed;
  uint64_t num_active;
  uint64_t num_waiting;
  uint64_t num_stopped;
} Aria2RustGlobalStat;

int32_t aria2_rust_library_init(void);
int32_t aria2_rust_library_deinit(void);

/* Unknown options are ignored. Invalid known values return NULL. */
Aria2RustSession *aria2_rust_session_new(const Aria2RustKeyValue *options,
                                          size_t option_count,
                                          void *user_data);
int32_t aria2_rust_session_final(Aria2RustSession *session);
void aria2_rust_session_free(Aria2RustSession *session);

/* mode 0 waits for all current downloads; mode 1 performs one poll. */
int32_t aria2_rust_run(Aria2RustSession *session, uint32_t mode);

int32_t aria2_rust_add_uri(Aria2RustSession *session, const char *const *uris,
                            size_t uri_count,
                            const Aria2RustKeyValue *options,
                            size_t option_count, uint64_t *gid_out);
int32_t aria2_rust_remove(Aria2RustSession *session, uint64_t gid,
                          uint8_t force);
int32_t aria2_rust_pause(Aria2RustSession *session, uint64_t gid,
                         uint8_t force);
int32_t aria2_rust_unpause(Aria2RustSession *session, uint64_t gid);
int32_t aria2_rust_change_option(Aria2RustSession *session, uint64_t gid,
                                 const Aria2RustKeyValue *options,
                                 size_t option_count);
int32_t aria2_rust_change_global_option(Aria2RustSession *session,
                                        const Aria2RustKeyValue *options,
                                        size_t option_count);

int32_t aria2_rust_get_download_info(Aria2RustSession *session, uint64_t gid,
                                     Aria2RustDownloadInfo *output);
int32_t aria2_rust_get_global_stat(Aria2RustSession *session,
                                   Aria2RustGlobalStat *output);
/* Returns the required number of entries. Pass NULL/0 to query the size. */
size_t aria2_rust_get_active_downloads(Aria2RustSession *session,
                                       uint64_t *output, size_t capacity);

/* Returns required bytes including NUL; no write occurs on a short buffer. */
size_t aria2_rust_gid_to_hex(uint64_t gid, char *output, size_t capacity);
uint64_t aria2_rust_hex_to_gid(const char *input);
uint8_t aria2_rust_is_null_gid(uint64_t gid);
size_t aria2_rust_last_error(Aria2RustSession *session, char *output,
                             size_t capacity);

#ifdef __cplusplus
}
#endif

#endif /* ARIA2_RUST_H */
