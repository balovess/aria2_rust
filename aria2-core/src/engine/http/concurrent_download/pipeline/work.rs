use std::time::Instant;

use crate::engine::work_scheduler::{RetryOutcome, WorkLease, WorkScheduleError, WorkScheduler};
use crate::error::{Aria2Error, Result};

pub(super) fn http_work_scheduler_error(context: &str, error: WorkScheduleError) -> Aria2Error {
    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
        "HTTP work scheduler could not {context}: {error}"
    )))
}

pub(super) fn record_http_work_failure(
    work_queue: &mut WorkScheduler<u32>,
    lease: WorkLease<u32>,
    retry: bool,
) -> Result<bool> {
    let retry_at = retry.then(Instant::now);
    let outcome = work_queue
        .fail(lease, retry_at)
        .map_err(|error| http_work_scheduler_error("record a failed attempt", error))?;
    match (retry, outcome) {
        (true, RetryOutcome::Scheduled) => Ok(true),
        (true, RetryOutcome::Exhausted(_)) | (false, RetryOutcome::Exhausted(_)) => Ok(false),
        (false, RetryOutcome::Scheduled) => {
            Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "HTTP terminal work failure was unexpectedly retried".into(),
            )))
        }
    }
}
