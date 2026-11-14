//! Read-only scoped downloads. Never reconcile or alter client ownership here.
use crate::{
    AppState,
    models::{library_layout, series},
    services::{download_client::DownloadItem, media},
};
use askama::Template;
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Default, Deserialize)]
#[serde(default)]
struct Scope {
    series_id: Option<i64>,
    franchise_id: Option<String>,
}
struct DownloadRow {
    name: String,
    state: String,
    progress: String,
    size: String,
    shared: bool,
}
#[derive(Template)]
#[template(path = "partials/library/downloads.html")]
struct Downloads {
    rows: Vec<DownloadRow>,
    errors: Vec<String>,
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/library/title-downloads", get(downloads))
        .route("/library/activity", get(activity_page))
}

#[derive(Template)]
#[template(path = "library_activity.html")]
struct ActivityPage {
    page: &'static str,
    title_language: String,
    activity: Vec<crate::models::library_activity::Activity>,
}
async fn activity_page(State(state): State<AppState>) -> Response {
    let rows = crate::models::library_activity::recent_all(&state.db).await;
    let language = crate::models::config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
        .title_language;
    match rows {
        Ok(activity) => match (ActivityPage {
            page: "library",
            title_language: language,
            activity,
        })
        .render()
        {
            Ok(html) => Html(html).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Activity could not be rendered.",
            )
                .into_response(),
        },
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Activity is unavailable. Try again.",
        )
            .into_response(),
    }
}

async fn downloads(State(state): State<AppState>, Query(scope): Query<Scope>) -> Response {
    match load(&state, scope).await {
        Ok(view) => match view.render() {
            Ok(h) => Html(h).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not render downloads.",
            )
                .into_response(),
        },
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
    }
}
async fn load(state: &AppState, scope: Scope) -> Result<Downloads, String> {
    let ids = if let Some(id) = scope.series_id {
        if series::get_by_id(&state.db, id)
            .await
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("Release not found.".into());
        }
        vec![id]
    } else if let Some(id) = scope.franchise_id {
        let id = library_layout::resolve_page_id(&state.db, &id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Franchise not found.")?;
        sqlx::query_scalar::<_,i64>(sqlx::AssertSqlSafe(format!("{} SELECT DISTINCT s.id FROM series s LEFT JOIN local_anilist_ids l ON l.series_id=s.id JOIN library_franchise_members m ON m.provider_id IN(l.anilist_id,s.anilist_id) JOIN library_franchises f ON f.folder=m.folder WHERE f.page_id=?",series::LOCAL_ANILIST_IDS))).bind(id).fetch_all(&state.db).await.map_err(|e|e.to_string())?
    } else {
        return Err("Choose a release or franchise.".into());
    };
    let ids = serde_json::to_string(&ids).map_err(|e| e.to_string())?;
    let grabs=sqlx::query_as::<_,(i64,String,String,String,Option<i64>,i64)>("SELECT g.id,g.hash,g.torrent_name,g.state,g.download_client_id,(SELECT count(DISTINCT series_id) FROM (SELECT series_id FROM grabbed_torrents WHERE id=g.id UNION SELECT series_id FROM grabbed_torrent_series WHERE grab_id=g.id)) FROM grabbed_torrents g WHERE (g.series_id IN(SELECT value FROM json_each(?)) OR EXISTS(SELECT 1 FROM grabbed_torrent_series r WHERE r.grab_id=g.id AND r.series_id IN(SELECT value FROM json_each(?)))) AND g.state IN('pending','imported') ORDER BY g.id DESC LIMIT 100")
        .bind(&ids).bind(&ids).fetch_all(&state.db).await.map_err(|e|e.to_string())?;
    let pool = state.download_clients.read().await.clone();
    let mut native = HashMap::<(i64, String), DownloadItem>::new();
    let mut errors = Vec::new();
    if !grabs.is_empty() {
        let legacy = grabs.iter().any(|g| g.4.is_none());
        let wanted: std::collections::HashSet<_> = grabs.iter().filter_map(|g| g.4).collect();
        let replies = futures_util::future::join_all(
            pool.clients
                .iter()
                .filter(|(id, _)| legacy || wanted.contains(id))
                .map(|(id, client)| async move { (*id, client.list_scoped().await) }),
        )
        .await;
        for (id, result) in replies {
            match result {
                Ok(items) => {
                    for item in items {
                        native.insert((id, item.hash.clone()), item);
                    }
                }
                Err(_) => errors.push(format!(
                    "Download client #{id} is unavailable. Its queue could not be checked."
                )),
            }
        }
    }
    let mut rows = Vec::new();
    for (_, hash, name, recorded, client, owners) in grabs {
        let item = if let Some(id) = client {
            native.get(&(id, hash.clone()))
        } else {
            let mut matches = native
                .iter()
                .filter(|((_, h), _)| h == &hash)
                .map(|(_, v)| v);
            let first = matches.next();
            if matches.next().is_none() {
                first
            } else {
                None
            }
        };
        if let Some(item) = item {
            rows.push(DownloadRow {
                name,
                state: item.state.clone(),
                progress: format!("{:.1}%", item.progress.clamp(0.0, 1.0) * 100.0),
                size: media::format_size(item.size.max(0) as u64),
                shared: owners > 1,
            });
        } else if recorded == "pending" {
            rows.push(DownloadRow {
                name,
                state: "Recorded pending · client status unavailable".into(),
                progress: "Unknown".into(),
                size: "Unknown".into(),
                shared: owners > 1,
            });
        }
    }
    Ok(Downloads { rows, errors })
}
