use crate::error::Result;
use std::path::{Path, PathBuf};

use super::control_file::ControlFile;

pub struct ResumeHelper {
    output_path: PathBuf,
    control_path: PathBuf,
    continue_opt: bool,
}

#[derive(Debug, Clone)]
pub struct ResumeState {
    pub existing_length: u64,
    pub control_file: Option<ControlFile>,
    pub start_offset: u64,
    pub should_resume: bool,
    pub is_complete: bool,
}

impl ResumeHelper {
    pub fn new(output_path: &Path, continue_opt: bool) -> Self {
        let control_path = ControlFile::control_path_for(output_path);
        Self {
            output_path: output_path.to_path_buf(),
            control_path,
            continue_opt,
        }
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    pub async fn detect(&self, total_length: u64) -> Result<ResumeState> {
        if !self.continue_opt || total_length == 0 {
            return Ok(ResumeState {
                existing_length: 0,
                control_file: None,
                start_offset: 0,
                should_resume: false,
                is_complete: false,
            });
        }

        let file_exists = self.output_path.exists();
        let existing_length = if file_exists {
            tokio::fs::metadata(&self.output_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        if !file_exists && !self.control_path.exists() {
            return Ok(ResumeState {
                existing_length: 0,
                control_file: None,
                start_offset: 0,
                should_resume: false,
                is_complete: false,
            });
        }

        let ctrl = ControlFile::load(&self.control_path).await?;
        let (start_offset, should_resume, is_complete) = match (&ctrl, existing_length) {
            (Some(cf), _) if cf.completed_pieces() > 0 => {
                let offset = std::cmp::max(existing_length, cf.completed_length());
                let complete = existing_length >= total_length && cf.total_length() == total_length;
                (offset, true, complete)
            }
            (_, len) if len >= total_length => (0, false, true),
            (_, len) if len > 0 => (len, true, false),
            _ => (0, false, false),
        };

        Ok(ResumeState {
            existing_length,
            control_file: ctrl,
            start_offset,
            should_resume,
            is_complete,
        })
    }

    pub fn build_range_header(state: &ResumeState) -> Option<String> {
        if state.should_resume && state.start_offset > 0 {
            Some(format!("bytes={}-", state.start_offset))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_detect_no_continue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");

        tokio::fs::write(&path, b"partial data").await.unwrap();

        let helper = ResumeHelper::new(&path, false);
        let state = helper.detect(1000).await.unwrap();
        assert!(!state.should_resume);
        assert_eq!(state.start_offset, 0);
    }

    #[tokio::test]
    async fn test_detect_no_file_no_control() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.bin");

        let helper = ResumeHelper::new(&path, true);
        let state = helper.detect(10000).await.unwrap();
        assert!(!state.should_resume);
        assert_eq!(state.start_offset, 0);
    }

    #[tokio::test]
    async fn test_detect_partial_file_no_control() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.bin");
        tokio::fs::write(&path, vec![0xAB; 500]).await.unwrap();

        let helper = ResumeHelper::new(&path, true);
        let state = helper.detect(2000).await.unwrap();
        assert!(state.should_resume);
        assert_eq!(state.start_offset, 500);
        assert_eq!(state.existing_length, 500);
        assert!(!state.is_complete);
    }

    #[tokio::test]
    async fn test_detect_with_control_file() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("download.iso");
        let ctrl_path = ControlFile::control_path_for(&out_path);

        tokio::fs::write(&out_path, vec![0xCD; 300]).await.unwrap();

        let mut cf = ControlFile::open_or_create(&ctrl_path, 1000, 10)
            .await
            .unwrap();
        cf.mark_piece_done(0);
        cf.mark_piece_done(1);
        cf.mark_piece_done(2);
        cf.save().await.unwrap();

        let helper = ResumeHelper::new(&out_path, true);
        let state = helper.detect(1000).await.unwrap();
        assert!(state.should_resume);
        assert!(state.control_file.is_some());
        assert_eq!(state.start_offset, 300);
        assert!(!state.is_complete);
    }

    #[tokio::test]
    async fn test_detect_file_already_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("complete.bin");
        tokio::fs::write(&path, vec![0x42; 1024]).await.unwrap();

        let helper = ResumeHelper::new(&path, true);
        let state = helper.detect(1024).await.unwrap();
        assert!(!state.should_resume);
        assert!(state.is_complete);
    }

    #[tokio::test]
    async fn test_build_range_header() {
        let state = ResumeState {
            existing_length: 500,
            control_file: None,
            start_offset: 500,
            should_resume: true,
            is_complete: false,
        };
        let header = ResumeHelper::build_range_header(&state);
        assert_eq!(header, Some("bytes=500-".to_string()));
    }

    #[tokio::test]
    async fn test_build_range_header_no_resume() {
        let state = ResumeState {
            existing_length: 0,
            control_file: None,
            start_offset: 0,
            should_resume: false,
            is_complete: false,
        };
        let header = ResumeHelper::build_range_header(&state);
        assert!(header.is_none());
    }

    #[tokio::test]
    async fn test_build_range_header_zero_offset() {
        let state = ResumeState {
            existing_length: 0,
            control_file: None,
            start_offset: 0,
            should_resume: true,
            is_complete: false,
        };
        let header = ResumeHelper::build_range_header(&state);
        assert!(header.is_none());
    }

    #[tokio::test]
    async fn test_control_path_generation() {
        let helper = ResumeHelper::new(Path::new("/downloads/bigfile.iso"), true);
        let cp = helper.control_path();
        assert!(cp.to_str().unwrap().ends_with(".aria2"));
    }
}
