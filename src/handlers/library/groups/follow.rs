//! Save the reviewed releases and their future-release policy as one follow.
use super::preview::{Error, Preview, draft, read_error};
use crate::{
    handlers::responses::htmx_aware_redirect, models::release_follows,
    services::release_follows::SETTINGS_CHANGED,
};
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::Response,
};
use axum_htmx::HxRequest;
use sqlx::SqlitePool;
use std::collections::HashSet;

const STALE: Error = (
    StatusCode::CONFLICT,
    "The franchise changed. Reload the page and review your choices.",
);

pub async fn save(
    State(db): State<SqlitePool>,
    Path(seed): Path<i64>,
    HxRequest(htmx): HxRequest,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, Error> {
    let mut fields = fields;
    if fields
        .iter()
        .any(|(k, v)| k == "future_controls" && v == "1")
        && !fields.iter().any(|(k, _)| k == "future_enabled")
    {
        fields.retain(|(k, _)| k != "include");
    }
    let plan = draft(&db, seed, &fields).await?;
    let owner = plan.owner.filter(|id| *id > 0).ok_or((
        StatusCode::BAD_REQUEST,
        "Choose the release that owns this franchise.",
    ))?;
    let known: HashSet<_> = plan.rows.iter().map(|row| row.entry.provider_id).collect();
    if !fields
        .iter()
        .any(|(k, v)| k == "version" && v == &plan.version)
        || !fields.iter().any(|(k, v)| k == "selection" && v == "1")
    {
        return Err(STALE);
    }
    if plan.name_override.len() > 2048 {
        return Err((StatusCode::BAD_REQUEST, "Use a shorter franchise name."));
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(read_error)?;
    let follows: Vec<_> = release_follows::list(&mut *tx)
        .await
        .map_err(read_error)?
        .into_iter()
        .filter(|f| known.contains(&f.anilist_id))
        .collect();
    if super::preview::version(&follows) != super::preview::version(&plan.follows) {
        return Err(STALE);
    }
    let layouts: HashSet<_> = follows
        .iter()
        .filter(|f| !f.library_folder.is_empty())
        .map(|f| f.library_folder.as_str())
        .collect();
    if layouts.len() > 1 {
        return Err((
            StatusCode::CONFLICT,
            "These follows use different library folders. Review their paths before combining them.",
        ));
    }
    let library_folder = layouts.iter().next().copied().unwrap_or("");
    let name = if plan.name_override.is_empty() {
        &plan
            .rows
            .iter()
            .find(|r| r.entry.provider_id == owner)
            .ok_or(STALE)?
            .entry
            .title
    } else {
        &plan.name_override
    };
    let policy = &plan.policy;
    if fields.iter().any(|(k, _)| k == "future_enabled")
        && !(policy.tv || policy.movies || policy.ovas)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Choose TV, movies, or OVAs before enabling future releases.",
        ));
    }
    // Prefer the existing owner's row. Re-anchoring a season-two follow keeps its receipts and start date.
    let id = if let Some(primary) = follows
        .iter()
        .find(|f| f.anilist_id == owner)
        .or(follows.first())
    {
        let since = follows.iter().map(|f| &f.started_on).min().unwrap();
        for other in follows.iter().filter(|f| f.id != primary.id) {
            sqlx::query(
                "INSERT OR IGNORE INTO followed_releases(follow_id,anilist_id,title)
                SELECT ?,anilist_id,title FROM followed_releases WHERE follow_id=?",
            )
            .bind(primary.id)
            .bind(other.id)
            .execute(&mut *tx)
            .await
            .map_err(read_error)?;
            sqlx::query("UPDATE franchise_choices SET follow_id=? WHERE follow_id=?")
                .bind(primary.id)
                .bind(other.id)
                .execute(&mut *tx)
                .await
                .map_err(read_error)?;
            sqlx::query("DELETE FROM release_follows WHERE id=?")
                .bind(other.id)
                .execute(&mut *tx)
                .await
                .map_err(read_error)?;
        }
        sqlx::query(
            "UPDATE release_follows SET anilist_id=?,title=?,started_on=?,tv=?,movies=?,ovas=?,
             alternatives=?,library_folder=?,revision=revision+1,check_status='pending' WHERE id=?",
        )
        .bind(owner)
        .bind(name)
        .bind(since)
        .bind(policy.tv)
        .bind(policy.movies)
        .bind(policy.ovas)
        .bind(policy.alternatives)
        .bind(library_folder)
        .bind(primary.id)
        .execute(&mut *tx)
        .await
        .map_err(read_error)?;
        primary.id
    } else {
        sqlx::query_scalar(
            "INSERT INTO release_follows(anilist_id,title,tv,movies,ovas,alternatives,revision,check_status)
             VALUES(?,?,?,?,?,?,1,'pending') RETURNING id",
        )
            .bind(owner).bind(name).bind(policy.tv).bind(policy.movies).bind(policy.ovas).bind(policy.alternatives)
            .fetch_one(&mut *tx).await.map_err(read_error)?
    };
    let future_only = fields
        .iter()
        .any(|(k, v)| k == "future_controls" && v == "1");
    if !future_only {
        save_choices(&mut tx, id, &plan).await?;
    }
    tx.commit().await.map_err(read_error)?;
    SETTINGS_CHANGED.notify_one();
    if future_only {
        let destination = plan.return_to.as_deref().unwrap_or("/");
        return Ok(htmx_aware_redirect(
            htmx,
            &format!(
                "{destination}{}tab=settings",
                if destination.contains('?') { "&" } else { "?" }
            ),
        ));
    }
    let mut destination = format!("/library/groups/{owner}/preview");
    if let Some(return_to) = &plan.return_to {
        destination.push_str("?return_to=");
        destination.push_str(&urlencoding::encode(return_to));
    }
    Ok(htmx_aware_redirect(htmx, &destination))
}

async fn save_choices(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    follow: i64,
    plan: &Preview,
) -> Result<(), Error> {
    for row in &plan.rows {
        let entry = &row.entry;
        let id = entry.provider_id;
        let saved: Option<(i64, bool)> =
            sqlx::query_as("SELECT follow_id,selected FROM franchise_choices WHERE anilist_id=?")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(read_error)?;
        if saved.is_some_and(|(owner, _)| owner != follow) || saved.map(|(_, v)| v) != entry.choice
        {
            return Err(STALE);
        }
        let excluded: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM external_sync_exclusions
             WHERE anilist_id=? OR (anilist_id IS NULL AND mal_id=?))",
        )
        .bind(id)
        .bind(entry.mal_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(read_error)?;
        if excluded != entry.excluded {
            return Err(STALE);
        }
        sqlx::query(
            "INSERT INTO franchise_choices(anilist_id,follow_id,title,selected) VALUES(?,?,?,?)
            ON CONFLICT(anilist_id) DO UPDATE SET title=excluded.title,selected=excluded.selected",
        )
        .bind(id)
        .bind(follow)
        .bind(&entry.title)
        .bind(row.selected)
        .execute(&mut **tx)
        .await
        .map_err(read_error)?;
        if let Some(local) = entry.series_id {
            let present: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM series WHERE id=?)")
                    .bind(local)
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(read_error)?;
            if !present {
                return Err(STALE);
            }
        } else if row.selected {
            let related = entry.related.as_ref().ok_or((
                StatusCode::CONFLICT,
                "Refresh this release’s metadata before adding it.",
            ))?;
            let added = release_follows::track_release(tx, related)
                .await
                .map_err(read_error)?;
            if !added {
                return Err((
                    StatusCode::CONFLICT,
                    "This release already has a library match. Refresh its metadata and review the selection.",
                ));
            }
        }
    }
    Ok(())
}
