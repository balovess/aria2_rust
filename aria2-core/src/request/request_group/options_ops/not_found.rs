impl super::super::RequestGroup {
    /// Return the configured maximum number of 404 responses for this group.
    ///
    /// `max-file-not-found=0` disables 404 retries and therefore reports the
    /// first response as `RESOURCE_NOT_FOUND`, matching aria2's wire behavior.
    pub fn max_file_not_found(&self) -> u32 {
        self.effective_option_snapshot()
            .and_then(|options| options.get("max-file-not-found").cloned())
            .and_then(|value| super::super::options::option_value_to_string(&value))
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(self.options.max_file_not_found)
    }

    /// Record a not-found response and return the terminal or retryable code.
    pub fn record_file_not_found(&self) -> super::super::result_code::DownloadResultCode {
        let count = self
            .file_not_found_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        let max = self.max_file_not_found();

        if max > 0 && count >= max && self.completed_length() == 0 {
            super::super::result_code::DownloadResultCode::MaxFileNotFound
        } else {
            super::super::result_code::DownloadResultCode::ResourceNotFound
        }
    }

    /// Convert the next HTTP 404 response into the public download error.
    pub fn file_not_found_error(&self) -> crate::error::Aria2Error {
        match self.record_file_not_found() {
            super::super::result_code::DownloadResultCode::MaxFileNotFound => {
                crate::error::Aria2Error::Recoverable(
                    crate::error::RecoverableError::MaxFileNotFound,
                )
            }
            _ => crate::error::Aria2Error::Recoverable(
                crate::error::RecoverableError::ResourceNotFound,
            ),
        }
    }

    /// Return whether another 404 request is permitted by the group option.
    pub fn can_retry_file_not_found(&self) -> bool {
        let max = self.max_file_not_found();
        max > 0
            && (self
                .file_not_found_count
                .load(std::sync::atomic::Ordering::Relaxed)
                < max
                || self.completed_length() > 0)
    }
}
