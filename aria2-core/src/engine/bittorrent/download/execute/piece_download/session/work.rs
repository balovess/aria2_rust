use std::time::Instant;

use crate::engine::bittorrent::piece::PiecePicker;
use crate::engine::work_scheduler::{RetryOutcome, WorkLease, WorkScheduler};
use crate::error::{Aria2Error, Result};

use super::bt_work_scheduler_error;

pub(super) fn retry_bt_piece_work(
    scheduler: &mut WorkScheduler<u32>,
    lease: WorkLease<u32>,
) -> Result<()> {
    match scheduler
        .fail(lease, Some(Instant::now()))
        .map_err(|error| bt_work_scheduler_error("record a failed piece", error))?
    {
        RetryOutcome::Scheduled => Ok(()),
        RetryOutcome::Exhausted(_) => Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "BitTorrent piece retry was rejected by the core scheduler".into(),
        ))),
    }
}

pub(super) fn requeue_interrupted_piece_work(
    scheduler: &mut WorkScheduler<u32>,
    picker: &mut PiecePicker,
    active_work: &mut std::collections::HashMap<u32, WorkLease<u32>>,
) -> Result<()> {
    for (piece_index, lease) in active_work.drain() {
        picker.mark_in_progress(piece_index, false);
        scheduler
            .requeue_unstarted(lease)
            .map_err(|error| bt_work_scheduler_error("requeue an interrupted piece", error))?;
    }
    Ok(())
}
