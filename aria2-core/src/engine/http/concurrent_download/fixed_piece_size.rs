use crate::constants;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::resume_helper::ResumeState;

/// Select the durable HTTP parent piece length.
///
/// A compatible control file owns the existing layout. New downloads use the
/// configured `piece-length`; the fallback matches aria2's 1 MiB default.
pub(super) fn resolve_http_piece_length(
    total_length: u64,
    resume_state: &ResumeState,
    configured_piece_length: Option<u64>,
) -> Result<u32> {
    let resumed_piece_length = resume_state
        .control_file
        .as_ref()
        .filter(|control_file| {
            control_file.total_length() == total_length && !control_file.is_torrent_checkpoint()
        })
        .and_then(ControlFile::piece_length)
        .filter(|&piece_size| piece_size > 0 && u64::from(piece_size) < total_length);
    select_piece_length(
        resumed_piece_length,
        configured_piece_length.filter(|&piece_size| piece_size > 0),
    )
}

fn select_piece_length(
    resumed_piece_length: Option<u32>,
    configured_piece_length: Option<u64>,
) -> Result<u32> {
    let piece_size = resumed_piece_length
        .map(u64::from)
        .or(configured_piece_length)
        .unwrap_or(constants::DEFAULT_PIECE_LENGTH);
    u32::try_from(piece_size).map_err(|_| {
        Aria2Error::InvalidArgument(format!(
            "HTTP fixed piece length is not representable: {piece_size}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_piece_length_overrides_the_default_for_new_downloads() {
        assert_eq!(
            select_piece_length(None, Some(4 * 1024 * 1024)).unwrap(),
            4 * 1024 * 1024
        );
        assert_eq!(
            select_piece_length(None, None).unwrap(),
            constants::DEFAULT_PIECE_LENGTH as u32
        );
    }

    #[test]
    fn compatible_resume_layout_wins_over_a_new_configuration() {
        assert_eq!(
            select_piece_length(Some(2 * 1024 * 1024), Some(4 * 1024 * 1024)).unwrap(),
            2 * 1024 * 1024
        );
    }

    #[tokio::test]
    async fn resolves_the_control_file_piece_length_for_resume() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let control_file = ControlFile::open_or_create_with_piece_length(
            &directory.path().join("resume.aria2"),
            8 * 1024 * 1024,
            2 * 1024 * 1024,
        )
        .await
        .expect("create control file");
        let resume_state = ResumeState {
            existing_length: 0,
            control_file: Some(control_file),
            start_offset: 0,
            should_resume: true,
            is_complete: false,
        };
        assert_eq!(
            resolve_http_piece_length(
                resume_state.control_file.as_ref().unwrap().total_length(),
                &resume_state,
                Some(4 * 1024 * 1024),
            )
            .unwrap(),
            2 * 1024 * 1024
        );
    }

    #[test]
    fn rejects_piece_lengths_that_the_control_file_cannot_encode() {
        let error = select_piece_length(None, Some(u64::from(u32::MAX) + 1)).unwrap_err();
        assert!(matches!(error, Aria2Error::InvalidArgument(_)));
    }
}
