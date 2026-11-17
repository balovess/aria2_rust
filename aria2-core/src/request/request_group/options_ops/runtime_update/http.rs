use super::{
    rpc_option_bool, rpc_option_size, rpc_option_string, rpc_option_u16, rpc_option_u32,
    rpc_option_u64,
};

use crate::request::request_group::DownloadOptions;

pub(super) fn apply(
    options: &mut DownloadOptions,
    key: &str,
    value: &serde_json::Value,
) -> Result<bool, String> {
    match key {
        "max-connection-per-server" => {
            let value = rpc_option_u16(value, key)?;
            if value == 0 {
                return Err(format!("Option '{}' must be greater than zero", key));
            }
            options.max_connection_per_server = Some(value);
            Ok(true)
        }
        "http-adaptive-enabled" => {
            options.http_adaptive.enabled = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "http-adaptive-reduction-percent"
        | "http-adaptive-recovery-interval"
        | "http-adaptive-recovery-successes-min"
        | "http-adaptive-recovery-successes-max"
        | "http-adaptive-start-interval-ms"
        | "http-adaptive-backoff-base"
        | "http-adaptive-backoff-max"
        | "http-adaptive-capacity-start-interval-ms"
        | "http-adaptive-capacity-start-interval-max-ms"
        | "http-adaptive-rate-recovery-successes"
        | "http-adaptive-rate-min-interval-ms"
        | "http-adaptive-backoff-reset-after" => {
            let value = rpc_option_u64(value, key)?;
            let (min, max) = match key {
                "http-adaptive-reduction-percent" => (1, 90),
                "http-adaptive-recovery-interval" => (1, 3600),
                "http-adaptive-recovery-successes-min" | "http-adaptive-recovery-successes-max" => {
                    (1, 100_000)
                }
                "http-adaptive-start-interval-ms" | "http-adaptive-rate-min-interval-ms" => {
                    (0, 60_000)
                }
                "http-adaptive-backoff-base" => (1, 3600),
                "http-adaptive-backoff-max" | "http-adaptive-backoff-reset-after" => (1, 86_400),
                "http-adaptive-capacity-start-interval-ms"
                | "http-adaptive-capacity-start-interval-max-ms" => (1, 60_000),
                "http-adaptive-rate-recovery-successes" => (1, 1_000_000),
                _ => unreachable!("adaptive option handled above"),
            };
            if !(min..=max).contains(&value) {
                return Err(format!(
                    "Option '{}' must be between {} and {}",
                    key, min, max
                ));
            }
            let adaptive = &mut options.http_adaptive;
            match key {
                "http-adaptive-reduction-percent" => adaptive.reduction_percent = value as u8,
                "http-adaptive-recovery-interval" => adaptive.recovery_interval_secs = value,
                "http-adaptive-recovery-successes-min" => {
                    adaptive.recovery_successes_min = value as u32
                }
                "http-adaptive-recovery-successes-max" => {
                    adaptive.recovery_successes_max = value as u32
                }
                "http-adaptive-start-interval-ms" => adaptive.start_interval_ms = value,
                "http-adaptive-backoff-base" => adaptive.backoff_base_secs = value,
                "http-adaptive-backoff-max" => adaptive.backoff_max_secs = value,
                "http-adaptive-capacity-start-interval-ms" => {
                    adaptive.capacity_start_interval_ms = value
                }
                "http-adaptive-capacity-start-interval-max-ms" => {
                    adaptive.capacity_start_interval_max_ms = value
                }
                "http-adaptive-rate-recovery-successes" => {
                    adaptive.rate_recovery_successes = value as u32
                }
                "http-adaptive-rate-min-interval-ms" => adaptive.rate_min_interval_ms = value,
                "http-adaptive-backoff-reset-after" => adaptive.backoff_reset_after_secs = value,
                _ => unreachable!("adaptive option handled above"),
            }
            options.http_adaptive = adaptive.normalized();
            Ok(true)
        }
        "max-http2-sessions-per-server" => {
            let value = rpc_option_u16(value, key)?;
            if !(1..=crate::constants::MAX_HTTP2_SESSIONS_PER_SERVER as u16).contains(&value) {
                return Err(format!(
                    "Option '{}' must be between 1 and {}",
                    key,
                    crate::constants::MAX_HTTP2_SESSIONS_PER_SERVER
                ));
            }
            options.max_http2_sessions_per_server = Some(value);
            Ok(true)
        }
        "max-http2-streams-per-session" => {
            let value = rpc_option_u16(value, key)?;
            if !(1..=256).contains(&value) {
                return Err(format!("Option '{}' must be between 1 and 256", key));
            }
            options.max_http2_streams_per_session = Some(value);
            Ok(true)
        }
        "min-http-range-size" => {
            let value = rpc_option_size(value, key)?;
            if !(crate::constants::HTTP_RANGE_SIZE_FLOOR_MIN_BYTES
                ..=crate::constants::HTTP_RANGE_SIZE_FLOOR_MAX_BYTES)
                .contains(&value)
            {
                return Err(format!(
                    "Option '{}' must be between {} and {} bytes",
                    key,
                    crate::constants::HTTP_RANGE_SIZE_FLOOR_MIN_BYTES,
                    crate::constants::HTTP_RANGE_SIZE_FLOOR_MAX_BYTES
                ));
            }
            options.min_http_range_size = Some(value);
            Ok(true)
        }
        "http-version" => {
            let value = rpc_option_string(value, key)?;
            let Some(version) = crate::http::HttpVersion::parse_option(&value) else {
                return Err(format!("Option '{}' must be one of: auto, 1.1, 2", key));
            };
            options.http_version = version;
            Ok(true)
        }
        "max-file-not-found" => {
            options.max_file_not_found = rpc_option_u32(value, key)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}
