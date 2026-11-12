//! Narrow an exact reviewed torrent before recording its episode ownership.
#[cfg(test)]
#[path = "source_numbering_grab_tests.rs"]
mod tests;

use crate::services::download_client::{AddOutcome, DownloadClient, wait_for_files};
use crate::services::release_numbering::ReleaseMapping;
use crate::services::source_numbering::BoundSourceProof;

pub(crate) const HOLD_PREFIX: &str = "Reviewed source held: ";

pub(crate) async fn mapping_for_selection(
    db: &sqlx::SqlitePool,
    series_id: i64,
    hash: &str,
    filenames: &[String],
) -> Result<Option<ReleaseMapping>, String> {
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let owner = crate::models::series::get_by_id(db, series_id)
        .await
        .map_err(|error| format!("Episode mapping: could not read the selected release: {error}"))?
        .ok_or("Episode mapping: the selected release no longer exists")?;
    let Some(saved) =
        crate::models::source_numbering::lookup_exact(db, hash, owner.anilist_id).await?
    else {
        return Ok(None);
    };
    crate::services::source_numbering::loader::refresh(db, &saved.proof).await?;
    let selected: Vec<_> = filenames
        .iter()
        .filter(|name| crate::services::auto_search::is_media_filename(name))
        .cloned()
        .collect();
    let proof = saved.proof.narrow_to(&selected)?;
    ReleaseMapping::from_bound_source(&proof).map(Some)
}

pub(crate) fn selected_indices(
    proof: &BoundSourceProof,
    names: &[String],
) -> Result<Vec<usize>, String> {
    proof.selected_indices(names)
}

pub(crate) struct PreparedBoundGrab {
    pub kept: Option<Vec<usize>>,
    pub info_hash: String,
    pub newly_added: bool,
    mapping: ReleaseMapping,
}

impl PreparedBoundGrab {
    /// Call only after durable source ownership was recorded. Recheck the
    /// client selection at that boundary; pre-existing jobs keep their state.
    pub async fn resume_after_record(&self, client: &dyn DownloadClient) -> Result<(), String> {
        if self.newly_added {
            verify_client_selection(client, &self.info_hash, &self.mapping).await?;
            client.resume(&self.info_hash).await?;
        }
        Ok(())
    }
}

/// The existing optional selective path may fall back to a whole download.
/// A bound numbering decision cannot: actual wanted files must match its proof.
pub(crate) async fn add_bound(
    client: &dyn DownloadClient,
    url: &str,
    info_hash: &str,
    mapping: &ReleaseMapping,
) -> Result<PreparedBoundGrab, String> {
    let proof = mapping
        .bound_source()
        .ok_or("Episode mapping: missing source proof")?;
    proof.validate()?;
    if client.protocol() != "torrent" || !proof.info_hash.eq_ignore_ascii_case(info_hash) {
        return Err("Episode mapping: the download source differs from its saved decision".into());
    }
    crate::services::download_client::check_release_url(url)?;
    if url.starts_with("magnet:")
        && crate::services::torrent_file::magnet_info_hash(url)
            .is_none_or(|hash| !hash.eq_ignore_ascii_case(info_hash))
    {
        return Err("Episode mapping: magnet identity differs from the reviewed torrent".into());
    }
    // Reviews currently originate from public Nyaa listings. A canonical
    // magnet binds the client's actual add to the reviewed hash even when an
    // indexer supplied an HTTP download URL that later redirects or changes.
    let already_present = client
        .list_scoped()
        .await?
        .iter()
        .any(|item| item.hash.eq_ignore_ascii_case(info_hash));
    if already_present {
        let files = client.get_files(info_hash).await?;
        let actual: Vec<_> = files
            .iter()
            .map(|file| (file.name.as_str(), file.wanted))
            .collect();
        proof.validate_files(info_hash, &actual)?;
        let names: Vec<_> = files.iter().map(|file| file.name.clone()).collect();
        return Ok(PreparedBoundGrab {
            kept: Some(selected_indices(proof, &names)?),
            info_hash: info_hash.to_ascii_lowercase(),
            newly_added: false,
            mapping: mapping.clone(),
        });
    }
    match client.add_torrent_metadata_only(info_hash).await? {
        AddOutcome::AlreadyPresent => {
            // The scoped list may miss another category or a concurrent add.
            // Never pause, reselect, or resume a job we did not create.
            verify_client_selection(client, info_hash, mapping).await?;
            let files = client.get_files(info_hash).await?;
            let names: Vec<_> = files.iter().map(|file| file.name.clone()).collect();
            return Ok(PreparedBoundGrab {
                kept: Some(selected_indices(proof, &names)?),
                info_hash: info_hash.to_ascii_lowercase(),
                newly_added: false,
                mapping: mapping.clone(),
            });
        }
        AddOutcome::Added => {}
    }
    // The strict native method returns only after metadata is ready and the
    // new job is stopped. Verify stop control before changing priorities;
    // a legacy paused-add or whole-download fallback is never permitted.
    let stopped = client.pause(info_hash).await;
    let validated = async {
        stopped?;
        let files = wait_for_files(client, info_hash, std::time::Duration::from_secs(10)).await?;
        let names: Vec<_> = files.iter().map(|file| file.name.clone()).collect();
        let selected = selected_indices(proof, &names)?;
        let unwanted: Vec<_> = (0..names.len())
            .filter(|index| !selected.contains(index))
            .collect();
        if !unwanted.is_empty() {
            client.set_file_wanted(info_hash, &unwanted, false).await?;
        }
        client.set_file_wanted(info_hash, &selected, true).await?;
        verify_client_selection(client, info_hash, mapping).await?;
        Ok(PreparedBoundGrab {
            kept: Some(selected),
            info_hash: info_hash.to_ascii_lowercase(),
            newly_added: true,
            mapping: mapping.clone(),
        })
    }
    .await;
    if validated.is_err() {
        // Keep all partial data. A reviewed source never falls back to a
        // whole download when metadata or a priority operation fails.
        let _ = client.pause(info_hash).await;
    }
    validated
}

pub(crate) async fn verify_client_selection(
    client: &dyn DownloadClient,
    hash: &str,
    mapping: &ReleaseMapping,
) -> Result<(), String> {
    let files = client.get_files(hash).await?;
    let actual: Vec<_> = files
        .iter()
        .map(|file| (file.name.as_str(), file.wanted))
        .collect();
    mapping.validate_bound_files(hash, &actual)
}

/// A picker uses cached indices. Refuse a changed client order before it can
/// apply those indices to different source files.
pub(crate) async fn validate_preview_manifest(
    client: &dyn DownloadClient,
    hash: &str,
    mapping: &ReleaseMapping,
    preview_names: &[String],
) -> Result<(), String> {
    let files = client.get_files(hash).await?;
    let names: Vec<_> = files.iter().map(|file| file.name.clone()).collect();
    if names != preview_names {
        return Err(
            "Episode mapping: the client file list changed. Reopen the file picker.".into(),
        );
    }
    let proof = mapping
        .bound_source()
        .ok_or("Episode mapping: missing source proof")?;
    proof.selected_indices(&names)?;
    Ok(())
}
