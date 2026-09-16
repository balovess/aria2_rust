use super::*;

/// Process all pending task completion notifications.
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

            match result {
                ProcessedTaskResult::Success if last_command => {
                    let had_failure = group
                        .recover()
                        .command_failure
                        .load(std::sync::atomic::Ordering::Acquire);
                    if had_failure {
                        let message = group.recover().get_last_error_message();
                        let code = group.recover().get_last_error_code();
                        group.recover().mark_error_with_code(code, message);
                    } else {
                        let group_state = group.recover();
                        let was_pause_requested =
                            group_state.is_pause_requested() || group_state.is_paused_flag();
                        let halt_reason = group_state.get_halt_reason();
                        drop(group_state);

                        if matches!(halt_reason, HaltReason::None) && was_pause_requested {
                            // A pause can race with a command finishing cleanly.
                            // Preserve the resumable state instead of turning
                            // the pause into a terminal completion.
                            group.recover().mark_paused();
                        } else {
                            match halt_reason {
                                crate::request::request_group::HaltReason::UserRequest => {
                                    group.recover().mark_removed();
                                }
                                crate::request::request_group::HaltReason::Timeout => {
                                    group.recover().mark_timeout();
                                }
                                crate::request::request_group::HaltReason::ShutdownSignal => {}
                                crate::request::request_group::HaltReason::None => {
                                    group.recover_mut().mark_complete();
                                }
                            }
                        }
                    }
                }
                ProcessedTaskResult::Success => {
                    // C++ removes a RequestGroup only after its final
                    // AbstractCommand is destroyed. Keep the group active
                    // while other command instances are still running.
                }
                ProcessedTaskResult::Failed(e) if !last_command => {
                    // A non-final command failure is recorded for the group,
                    // but terminal state is deferred until all commands have
                    // exited, matching C++ numCommand_ semantics.
                    let message = e.to_string();
                    let code = map_error_code(&e);
                    group.recover().set_last_error(code, message);
                    group
                        .recover()
                        .command_failure
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                ProcessedTaskResult::Failed(e) => {
                    // Handle failures from the final command, including pause-induced
                    // termination, before treating them as errors.
                    // (`aria2.pause` / `aria2.forcePause`
                    // marks the group Paused and the download loop aborts via
                    // check_cancelled) must leave the group Paused — resumable —
                    // rather than recording an Error that can never be unpaused.
                    // Mirrors C++ where pause-requested groups return to the
                    // reserved queue.
                    let group_state = group.recover();
                    let was_pause_requested =
                        group_state.is_pause_requested() || group_state.is_paused_flag();
                    let is_pause_error = matches!(
                        &e,
                        Aria2Error::DownloadFailed(msg) if msg == "Download paused"
                    );
                    let halt_reason = group_state.get_halt_reason();
                    drop(group_state);

                    match halt_reason {
                        // A user removal is terminal even when a pause command
                        // reached the group first. The halt reason is the
                        // stronger lifecycle signal and must win over the
                        // resumable Paused status.
                        HaltReason::UserRequest => group.recover().mark_removed(),
                        HaltReason::Timeout => group.recover().mark_timeout(),
                        HaltReason::ShutdownSignal => {
                            // A shutdown halt remains non-terminal so its
                            // result maps to IN_PROGRESS after cleanup.
                        }
                        HaltReason::None if was_pause_requested => {
                            group.recover_mut().mark_paused();
                        }
                        HaltReason::None if is_pause_error => {
                            // A pause was requested and then undone (`unpause`)
                            // before the task fully exited. Leave the group
                            // Waiting so the demotion layer re-queues it and
                            // promotion re-spawns the download.
                            let status = group.recover().status();
                            if !matches!(
                                status,
                                DownloadStatus::Complete
                                    | DownloadStatus::Error(_)
                                    | DownloadStatus::Removed
                            ) {
                                group.recover().mark_waiting();
                            }
                        }
                        HaltReason::None => group
                            .recover()
                            .mark_error_with_code(map_error_code(&e), e.to_string()),
                    }
                    group
                        .recover()
                        .command_failure
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                ProcessedTaskResult::Cancelled => {
                    // Synthetic cancellation is emitted before Tokio abort, so
                    // finalize the group here rather than relying on the
                    // cancelled task to mutate its status.
                    let group_state = group.recover();
                    let was_pause_requested =
                        group_state.is_pause_requested() || group_state.is_paused_flag();
                    let halt_reason = group_state.get_halt_reason();
                    drop(group_state);

                    if matches!(halt_reason, HaltReason::UserRequest | HaltReason::Timeout) {
                        // Removal/timeout is terminal even if the task was
                        // paused when the force-halt arrived.
                        if last_command {
                            match halt_reason {
                                HaltReason::UserRequest => group.recover().mark_removed(),
                                HaltReason::Timeout => group.recover().mark_timeout(),
                                _ => unreachable!(),
                            }
                        } else {
                            group
                                .recover()
                                .command_failure
                                .store(true, std::sync::atomic::Ordering::Release);
                        }
                    } else if was_pause_requested {
                        group.recover_mut().mark_paused();
                    } else if !last_command {
                        group
                            .recover()
                            .command_failure
                            .store(true, std::sync::atomic::Ordering::Release);
                    }
                }
            }
        }
    }
    processed
}
