use super::*;

enum ProcessedTaskResult {
    Success,
    Failed(Aria2Error),
    Cancelled,
}

pub(super) fn map_error_code(error: &Aria2Error) -> DownloadResultCode {
    match error {
        Aria2Error::Recoverable(RecoverableError::Timeout) => DownloadResultCode::TimeOut,
        Aria2Error::Checksum(_) => DownloadResultCode::ChecksumError,
        Aria2Error::JsonParse(_) => DownloadResultCode::JsonParseError,
        Aria2Error::MetalinkParse(_) => DownloadResultCode::MetalinkParseError,
        Aria2Error::BencodeParse(_) => DownloadResultCode::BencodeParseError,
        Aria2Error::BittorrentParse(_) => DownloadResultCode::BittorrentParseError,
        Aria2Error::MagnetParse(_) => DownloadResultCode::MagnetParseError,
        Aria2Error::Recoverable(RecoverableError::CannotResume) => DownloadResultCode::CannotResume,
        Aria2Error::FtpProtocol(_) => DownloadResultCode::FtpProtocolError,
        Aria2Error::HttpProtocol(_) => DownloadResultCode::HttpProtocolError,
        Aria2Error::Recoverable(RecoverableError::FtpProtocolError { .. }) => {
            DownloadResultCode::FtpProtocolError
        }
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { .. }) => {
            DownloadResultCode::HttpProtocolError
        }
        Aria2Error::Recoverable(RecoverableError::ResourceNotFound) => {
            DownloadResultCode::ResourceNotFound
        }
        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound) => {
            DownloadResultCode::MaxFileNotFound
        }
        Aria2Error::Recoverable(RecoverableError::HttpAuthFailed { .. }) => {
            DownloadResultCode::HttpAuthFailed
        }
        Aria2Error::Recoverable(RecoverableError::HttpTooManyRedirects { .. }) => {
            DownloadResultCode::HttpTooManyRedirects
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code })
            if *code == 401 || *code == 407 =>
        {
            DownloadResultCode::HttpAuthFailed
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code })
            if matches!(*code, 502..=504) =>
        {
            DownloadResultCode::HttpServiceUnavailable
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code }) if *code == 404 => {
            DownloadResultCode::ResourceNotFound
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code }) if *code == 500 => {
            DownloadResultCode::HttpProtocolError
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code }) if *code == 503 => {
            DownloadResultCode::HttpServiceUnavailable
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { .. }) => {
            DownloadResultCode::NetworkProblem
        }
        Aria2Error::Network(_) => DownloadResultCode::NetworkProblem,
        Aria2Error::FileOpen(_) => DownloadResultCode::FileOpenError,
        Aria2Error::FileCreate(_) => DownloadResultCode::FileCreateError,
        Aria2Error::FileIo(_) => DownloadResultCode::FileIoError,
        Aria2Error::DirCreate(_) => DownloadResultCode::DirCreateError,
        Aria2Error::NameResolve(_) => DownloadResultCode::NameResolveError,
        Aria2Error::Io(_) => DownloadResultCode::FileIoError,
        Aria2Error::InvalidArgument(_) => DownloadResultCode::OptionError,
        Aria2Error::Parse(_) => DownloadResultCode::UnknownError,
        Aria2Error::Fatal(crate::error::FatalError::Config(_)) => DownloadResultCode::OptionError,
        Aria2Error::Fatal(crate::error::FatalError::DiskSpaceExhausted) => {
            DownloadResultCode::NotEnoughDiskSpace
        }
        Aria2Error::Recoverable(_) => DownloadResultCode::NetworkProblem,
        _ => DownloadResultCode::UnknownError,
    }
}

pub(super) trait CompletionQueue {
    fn try_completion(&mut self) -> Result<(GroupId, CommandGeneration, TaskResult), ()>;
}

impl CompletionQueue for mpsc::Receiver<(GroupId, CommandGeneration, TaskResult)> {
    fn try_completion(&mut self) -> Result<(GroupId, CommandGeneration, TaskResult), ()> {
        mpsc::Receiver::try_recv(self).map_err(|_| ())
    }
}

impl CompletionQueue for mpsc::UnboundedReceiver<(GroupId, CommandGeneration, TaskResult)> {
    fn try_completion(&mut self) -> Result<(GroupId, CommandGeneration, TaskResult), ()> {
        mpsc::UnboundedReceiver::try_recv(self).map_err(|_| ())
    }
}

pub(super) struct PrefetchedCompletion<'a, R> {
    pub(super) first: Option<(GroupId, CommandGeneration, TaskResult)>,
    pub(super) receiver: &'a mut R,
}

impl<R: CompletionQueue> CompletionQueue for PrefetchedCompletion<'_, R> {
    fn try_completion(&mut self) -> Result<(GroupId, CommandGeneration, TaskResult), ()> {
        self.first
            .take()
            .map_or_else(|| self.receiver.try_completion(), Ok)
    }
}

/// Apply the lifecycle transition that belongs to one completed command.
///
/// Keeping this decision separate from channel accounting makes the event loop
/// responsible for orchestration while the completion state machine stays local.
fn apply_completion_state(
    group: &std::sync::Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>,
    result: ProcessedTaskResult,
    last_command: bool,
) {
    let group = group.recover();

    match result {
        ProcessedTaskResult::Success if last_command => {
            let had_failure = group
                .command_failure
                .load(std::sync::atomic::Ordering::Acquire);
            if had_failure {
                let message = group.get_last_error_message();
                let code = group.get_last_error_code();
                group.mark_error_with_code(code, message);
            } else {
                let was_pause_requested = group.is_pause_requested() || group.is_paused_flag();
                let halt_reason = group.get_halt_reason();

                if matches!(halt_reason, HaltReason::None) && was_pause_requested {
                    // A pause can race with a command finishing cleanly.
                    // Preserve the resumable state instead of turning
                    // the pause into a terminal completion.
                    group.mark_paused();
                } else {
                    match halt_reason {
                        HaltReason::UserRequest => group.mark_removed(),
                        HaltReason::Timeout => group.mark_timeout(),
                        HaltReason::ShutdownSignal => {}
                        HaltReason::None => group.mark_complete(),
                    }
                }
            }
        }
        ProcessedTaskResult::Success => {
            // C++ removes a RequestGroup only after its final
            // AbstractCommand is destroyed. Keep the group active
            // while other command instances are still running.
        }
        ProcessedTaskResult::Failed(error) if !last_command => {
            // A non-final command failure is recorded for the group,
            // but terminal state is deferred until all commands have
            // exited, matching C++ numCommand_ semantics.
            let message = error.to_string();
            let code = map_error_code(&error);
            group.set_last_error(code, message);
            group
                .command_failure
                .store(true, std::sync::atomic::Ordering::Release);
        }
        ProcessedTaskResult::Failed(error) => {
            // Handle failures from the final command, including pause-induced
            // termination, before treating them as errors.
            // The pause command marks the group Paused and the download loop
            // aborts via check_cancelled; it must remain resumable rather
            // than being recorded as an error that can never be unpaused.
            // Mirrors C++ where pause-requested groups return to the
            // reserved queue.
            let was_pause_requested = group.is_pause_requested() || group.is_paused_flag();
            let is_pause_error = matches!(
                &error,
                Aria2Error::DownloadFailed(message) if message == "Download paused"
            );
            let halt_reason = group.get_halt_reason();

            match halt_reason {
                // A user removal is terminal even when a pause command
                // reached the group first. The halt reason is the
                // stronger lifecycle signal and must win over the
                // resumable Paused status.
                HaltReason::UserRequest => group.mark_removed(),
                HaltReason::Timeout => group.mark_timeout(),
                HaltReason::ShutdownSignal => {
                    // A shutdown halt remains non-terminal so its
                    // result maps to IN_PROGRESS after cleanup.
                }
                HaltReason::None if was_pause_requested => {
                    group.mark_paused();
                }
                HaltReason::None if is_pause_error => {
                    // A pause was requested and then undone before the task
                    // fully exited. Leave the group Waiting so the demotion
                    // layer re-queues it and promotion re-spawns the download.
                    let status = group.status();
                    if !matches!(
                        status,
                        DownloadStatus::Complete
                            | DownloadStatus::Error(_)
                            | DownloadStatus::Removed
                    ) {
                        group.mark_waiting();
                    }
                }
                HaltReason::None => {
                    group.mark_error_with_code(map_error_code(&error), error.to_string());
                }
            }
            group
                .command_failure
                .store(false, std::sync::atomic::Ordering::Release);
        }
        ProcessedTaskResult::Cancelled => {
            // Synthetic cancellation is emitted before Tokio abort, so
            // finalize the group here rather than relying on the
            // cancelled task to mutate its status.
            let was_pause_requested = group.is_pause_requested() || group.is_paused_flag();
            let halt_reason = group.get_halt_reason();

            if matches!(halt_reason, HaltReason::UserRequest | HaltReason::Timeout) {
                // Removal/timeout is terminal even if the task was
                // paused when the force-halt arrived.
                if last_command {
                    match halt_reason {
                        HaltReason::UserRequest => group.mark_removed(),
                        HaltReason::Timeout => group.mark_timeout(),
                        _ => unreachable!(),
                    }
                } else {
                    group
                        .command_failure
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            } else if was_pause_requested {
                group.mark_paused();
            } else if !last_command {
                group
                    .command_failure
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }
}

/// Process all pending task completion notifications.
pub(super) async fn process_task_completions<R: CompletionQueue>(
    ctx: &EngineLoopContext,
    completion_rx: &mut R,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
    completed_generations: &mut HashSet<CommandGeneration>,
) -> bool {
    let mut processed = false;
    while let Ok((gid, generation, result)) = completion_rx.try_completion() {
        // A task may race with force-remove/timeout cleanup and publish more
        // than one terminal notification. Account for exactly one completion.
        if !completed_generations.insert(generation) {
            debug!(
                gid = gid.value(),
                generation, "Ignoring duplicate task completion"
            );
            continue;
        }
        processed = true;

        // A task finished: its status/progress changed, so persist it.
        mark_session_dirty(ctx);

        // Remove only this command instance. A RequestGroup may have several
        // active commands, just as C++ tracks several AbstractCommand objects
        // under one RequestGroup.
        running_downloads.retain(|(id, running)| *id != gid || running.generation != generation);

        // Decrement num_commands and update group status.
        let man = &ctx.group_man;
        if let Some(group) = man.find_group(gid) {
            let prev = group.recover().dec_commands();
            let last_command = prev == 1;
            debug!(
                gid = gid.value(),
                prev, last_command, "Task completed, decremented num_commands"
            );

            let result = match result {
                TaskResult::FailedWithContext {
                    error,
                    connection_context,
                } => {
                    let use_async_dns = group.recover().options().async_dns;
                    mark_failed_connection(
                        &ctx.dns_cache,
                        &error,
                        &connection_context,
                        use_async_dns,
                    )
                    .await;
                    ProcessedTaskResult::Failed(error)
                }
                TaskResult::Success => ProcessedTaskResult::Success,
                TaskResult::Failed(error) => ProcessedTaskResult::Failed(error),
                TaskResult::Cancelled => ProcessedTaskResult::Cancelled,
            };

            apply_completion_state(&group, result, last_command);
        }
    }
    processed
}
