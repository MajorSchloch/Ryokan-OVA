use std::path::Path;

use sqlx::SqlitePool;

use crate::models::{grabbed_torrents::GrabbedTorrent, series::Series};
use crate::services::auto_search;
use crate::services::download_client::normalize_info_hash;
use crate::services::release_numbering::{
    NumberingContext, ReleaseMapping, source_identity_name, validated_source_file_span,
};

pub(super) fn canonical_season(series: &Series, basename: &str) -> Option<i32> {
    let heading = source_identity_name(basename, true).ok()?;
    let aliases = [
        &series.title_romaji,
        &series.title_english,
        &series.title_native,
        &series.title,
    ];
    if !aliases
        .iter()
        .any(|title| source_identity_name(title, false).is_ok_and(|canonical| canonical == heading))
    {
        return None;
    }

    let mut seasons = aliases
        .iter()
        .map(|title| auto_search::parse_release_season(title))
        .filter(|season| *season > 0);
    let season = seasons.next().unwrap_or(1);
    if seasons.any(|other| other != season) {
        return None;
    }
    // A canonical season suffix is removed by source_identity_name. The
    // remaining words alone cannot certify an unqualified later season.
    let (_, named_season) = validated_source_file_span(basename).ok()?;
    if named_season.is_some_and(|named| named != season) || (season > 1 && named_season.is_none()) {
        return None;
    }
    Some(season)
}

pub(super) async fn mapping(
    grab: &GrabbedTorrent,
    series: &Series,
    name: &str,
) -> Option<ReleaseMapping> {
    let [episode] = grab.episode_numbers.as_slice() else {
        return None;
    };
    let basename = Path::new(name).file_name()?.to_str()?;
    let season = canonical_season(series, basename)?;
    // Interactive titles and the misgrab verdict can match related titles.
    // Require the actual file and listing to name this exact library entry.
    if source_identity_name(&grab.torrent_name, true).ok()?
        != source_identity_name(basename, true).ok()?
        || canonical_season(series, &grab.torrent_name)? != season
    {
        return None;
    }
    let context = NumberingContext::load(
        series.anilist_id,
        series.mal_id,
        &series.format,
        series.episodes,
        series.cumulative_prior_episodes,
        season,
    )
    .await
    .ok()?;
    let listing = context.choose(&grab.torrent_name, false).ok()?;
    let file = context.choose(basename, false).ok()?;
    if listing.episodes() != [*episode] || file.episodes() != [*episode] {
        return None;
    }
    listing.resolve_named_file(basename).ok()?;
    Some(listing)
}

pub(super) async fn record(db: &SqlitePool, grab: &GrabbedTorrent, series: &Series, name: &str) {
    if normalize_info_hash(&grab.hash).is_none() {
        return;
    }
    let Some(mapping) = mapping(grab, series, name).await else {
        return;
    };
    if let Err(error) =
        crate::models::grabbed_torrents::record_imported_single_mapping(db, grab, &mapping).await
    {
        tracing::warn!(grab_id = grab.id, %error, "Could not save imported source mapping");
    }
}
