//! Runs one Metalink source through the ordinary protocol command adapters.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::protocol_adapter::{
    ProtocolAdapterRegistry, ProtocolCommandRequest, ProtocolServices,
};
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::{GroupId, HaltReason, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

pub(crate) struct ProtocolRuntime {
    pub(super) adapters: Arc<ProtocolAdapterRegistry>,
    pub(super) services: ProtocolServices,
}

const METALINK_HTTP_CONNECT_TIMEOUT_SECS: u64 = 30;

fn options_for_source(
    options: &crate::request::request_group::DownloadOptions,
    url: &str,
) -> crate::request::request_group::DownloadOptions {
    let mut options = options.clone();
    if url.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    }) {
        options.connect_timeout = Some(
            options
                .connect_timeout
                .unwrap_or(METALINK_HTTP_CONNECT_TIMEOUT_SECS),
        );
        // The former Metalink transfer path issued one sequential GET per
        // mirror. Keep that request pattern while reusing the standard HTTP
        // command's DNS, proxy, TLS, and output handling.
        options.force_sequential = true;
    }
    // Metalink owns source retry policy. A child protocol command gets
    // exactly one attempt so retries are not multiplied at two layers.
    options.max_retries = 1;
    options
}

impl super::MetalinkDownloadCommand {
    pub(super) fn stage_path(&self, output_path: &Path, source_index: usize) -> PathBuf {
        let file_name = output_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("payload"))
            .to_string_lossy();
        let gid = self.group.recover().gid().value();
        output_path.with_file_name(format!(".{file_name}.metalink-{gid}-{source_index}.part"))
    }

    pub(super) async fn download_source_to_path(
        &self,
        url: &str,
        output_path: &Path,
    ) -> Result<()> {
        if let Some(error) = self.lifecycle_error() {
            return Err(error);
        }

        let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| Aria2Error::FileIo(error.to_string()))?;

        let mut options = options_for_source(self.group.recover().options(), url);
        options.dir = Some(parent.to_string_lossy().into_owned());
        options.out = Some(
            output_path
                .file_name()
                .ok_or_else(|| {
                    Aria2Error::Fatal(FatalError::Config(
                        "Metalink staging path has no filename".into(),
                    ))
                })?
                .to_string_lossy()
                .into_owned(),
        );
        let child_group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new_random(),
            vec![url.to_owned()],
            options.clone(),
        )));
        let runtime = self.protocol_runtime();
        let mut services = runtime.services;
        if self.global_limiter.is_some() {
            services.global_limiter = self.global_limiter.clone();
        }

        let mut command = runtime
            .adapters
            .create(
                ProtocolCommandRequest {
                    group: Arc::clone(&child_group),
                    first_uri: url.to_owned(),
                    options: Arc::new(options),
                },
                &services,
            )
            .await?;

        let notifier = self.group.recover().lifecycle_notifier();
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.lifecycle_error().is_some() {
            self.relay_lifecycle(&child_group);
        }
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        let result = {
            let mut execution = Box::pin(command.execute());
            loop {
                tokio::select! {
                    result = &mut execution => break result,
                    _ = interval.tick() => self.copy_child_progress(&child_group),
                    _ = &mut notified => {
                        self.relay_lifecycle(&child_group);
                        // A Metalink lifecycle transition ends this source task.
                        // The protocol command owns flushing its partial output.
                        break execution.await;
                    }
                }
            }
        };
        command.shutdown().await;
        self.copy_child_progress(&child_group);
        if let Some(error) = self.lifecycle_error() {
            if self.group.recover().is_removed() {
                discard_stage(output_path).await;
            }
            return Err(error);
        }
        result
    }

    fn protocol_runtime(&self) -> ProtocolRuntime {
        self.protocol_runtime.as_ref().map_or_else(
            || ProtocolRuntime {
                adapters: Arc::new(ProtocolAdapterRegistry::source_protocols(Arc::new(
                    crate::ftp::connection_pool::FtpConnectionPool::new(4),
                ))),
                services: ProtocolServices {
                    dns_cache: Arc::new(tokio::sync::Mutex::new(
                        crate::dns::dns_cache::DnsCache::new(),
                    )),
                    outbound_network_policy: Arc::clone(&self.outbound_network_policy),
                    global_limiter: self.global_limiter.clone(),
                },
            },
            |runtime| ProtocolRuntime {
                adapters: Arc::clone(&runtime.adapters),
                services: runtime.services.clone(),
            },
        )
    }

    fn copy_child_progress(&self, child_group: &std::sync::RwLock<RequestGroup>) {
        let child = child_group.recover();
        let completed = child.progress.completed_length();
        let total = child.progress.total_length();
        drop(child);
        let parent = self.group.recover();
        let progress_offset = self.progress_offset;
        if total > 0 && progress_offset.saturating_add(total) > parent.total_length() {
            parent.set_total_length(progress_offset.saturating_add(total));
        }
        parent.update_progress(progress_offset.saturating_add(completed));
    }

    fn relay_lifecycle(&self, child_group: &std::sync::RwLock<RequestGroup>) {
        let parent = self.group.recover();
        let mut child = child_group.recover_mut();
        if parent.is_removed() {
            child.mark_removed();
        } else if parent.is_paused_flag() {
            if parent.is_force_halt_requested() {
                let _ = child.force_pause();
            } else {
                let _ = child.pause();
            }
        } else if parent.is_force_halt_requested() {
            child.request_force_halt(HaltReason::UserRequest);
        } else if parent.is_halt_requested() {
            child.request_halt(HaltReason::UserRequest);
        }
    }
}

pub(super) async fn discard_stage(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        crate::filesystem::control_file::ControlFile::control_path_for(path),
    ] {
        if let Err(error) = tokio::fs::remove_file(&candidate).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %candidate.display(), %error, "Failed to remove Metalink staging file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::options_for_source;
    use crate::request::request_group::DownloadOptions;

    #[test]
    fn http_source_keeps_metalinks_legacy_connect_timeout_fallback() {
        let defaults = options_for_source(
            &DownloadOptions::default(),
            "https://mirror.example/payload.bin",
        );
        assert_eq!(defaults.connect_timeout, Some(30));
        assert!(defaults.force_sequential);
        assert_eq!(defaults.max_retries, 1);

        let explicit = options_for_source(
            &DownloadOptions {
                connect_timeout: Some(47),
                ..DownloadOptions::default()
            },
            "http://mirror.example/payload.bin",
        );
        assert_eq!(explicit.connect_timeout, Some(47));
        assert!(explicit.force_sequential);
    }

    #[test]
    fn non_http_source_keeps_its_protocol_connect_timeout_defaults() {
        let options = options_for_source(
            &DownloadOptions::default(),
            "ftp://mirror.example/payload.bin",
        );
        assert_eq!(options.connect_timeout, None);
        assert!(!options.force_sequential);
    }
}
