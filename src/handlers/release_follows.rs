use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::services::release_follows::SETTINGS_CHANGED;
use crate::{
    AppState,
    handlers::responses::htmx_aware_redirect,
    models::{
        config,
        release_follows::{self, Follow},
    },
};

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/library/follows", get(page).post(start))
        .route("/library/follows/{id}", post(update))
        .route("/library/follows/{id}/stop", post(stop))
}

#[derive(Template)]
#[template(path = "release_follows.html")]
struct FollowsPage {
    page: &'static str,
    title_language: String,
    follows: Vec<Follow>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Selection {
    anilist_id: i64,
    title: String,
}

impl Selection {
    fn valid(&self) -> bool {
        (1..=i64::from(i32::MAX)).contains(&self.anilist_id)
            && !self.title.trim().is_empty()
            && self.title.len() <= 2048
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Options {
    movies: bool,
    ovas: bool,
    alternatives: bool,
}

#[derive(Deserialize)]
struct NewFollow {
    anilist_id: i64,
    title: String,
    #[serde(default)]
    movies: bool,
    #[serde(default)]
    ovas: bool,
    #[serde(default)]
    alternatives: bool,
}

fn internal_error(err: impl std::fmt::Display) -> (StatusCode, &'static str) {
    tracing::error!("Release follow failed: {err}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not load or save the follow. Try again.",
    )
}

async fn page(
    State(state): State<AppState>,
    Query(selection): Query<Selection>,
) -> Result<Html<String>, (StatusCode, &'static str)> {
    let mut follows = release_follows::list_with_matches(&state.db)
        .await
        .map_err(internal_error)?;
    if selection.valid() {
        let owner:Option<i64> = sqlx::query_scalar("SELECT f.anilist_id FROM franchise_choices c JOIN release_follows f ON f.id=c.follow_id WHERE c.anilist_id=?")
            .bind(selection.anilist_id).fetch_optional(&state.db).await.map_err(internal_error)?;
        if let Some(i) = follows
            .iter()
            .position(|f| f.anilist_id == owner.unwrap_or(selection.anilist_id))
        {
            follows[..=i].rotate_right(1);
        } else {
            follows.insert(
                0,
                Follow {
                    id: 0,
                    anilist_id: selection.anilist_id,
                    title: selection.title,
                    started_on: String::new(),
                    movies: false,
                    ovas: false,
                    alternatives: false,
                    ..Follow::default()
                },
            );
        }
    }
    FollowsPage {
        page: "library",
        title_language: config::get_naming_prefs(&state.db).await.title_language,
        follows,
    }
    .render()
    .map(Html)
    .map_err(internal_error)
}

async fn start(
    State(state): State<AppState>,
    HxRequest(htmx): HxRequest,
    Form(form): Form<NewFollow>,
) -> Result<Response, (StatusCode, &'static str)> {
    let selection = Selection {
        anilist_id: form.anilist_id,
        title: form.title,
    };
    if !selection.valid() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Select an AniList anime from its series page.",
        ));
    }
    release_follows::start(
        &state.db,
        selection.anilist_id,
        selection.title.trim(),
        form.movies,
        form.ovas,
        form.alternatives,
    )
    .await
    .map_err(internal_error)?;
    SETTINGS_CHANGED.notify_one();
    Ok(htmx_aware_redirect(htmx, "/library/follows"))
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    HxRequest(htmx): HxRequest,
    Form(opts): Form<Options>,
) -> Result<Response, (StatusCode, &'static str)> {
    release_follows::update(&state.db, id, opts.movies, opts.ovas, opts.alternatives)
        .await
        .map_err(internal_error)?;
    SETTINGS_CHANGED.notify_one();
    Ok(htmx_aware_redirect(htmx, "/library/follows"))
}

async fn stop(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    HxRequest(htmx): HxRequest,
) -> Result<Response, (StatusCode, &'static str)> {
    release_follows::remove(&state.db, id)
        .await
        .map_err(internal_error)?;
    Ok(htmx_aware_redirect(htmx, "/library/follows"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_test_app_state, seed_series};
    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

    async fn matched_sequel() -> (SqlitePool, i64) {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::models::migrate(&db).await.unwrap();
        let local_id = seed_series(&db, -800, "Library sequel").await;
        sqlx::raw_sql(
            "UPDATE series SET mal_id=800 WHERE anilist_id=-800;
             INSERT INTO release_follows(anilist_id,title,movies,ovas,alternatives) VALUES (10,'Original',0,0,0);
             INSERT INTO followed_releases(follow_id,anilist_id,title)
                 SELECT id,12,'Matched sequel' FROM release_follows;
             INSERT INTO provider_metadata_cache(provider_id,mal_id,detail_json)
                 VALUES (12,800,'{}');",
        )
        .execute(&db)
        .await
        .unwrap();
        (db, local_id)
    }

    async fn render_read_only(db: &SqlitePool) -> String {
        sqlx::query("PRAGMA query_only=ON")
            .execute(db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        page(State(state), Query(Selection::default()))
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn matched_release_links_to_the_mal_entry_with_a_unique_cached_mapping() {
        let (db, local_id) = matched_sequel().await;
        let html = render_read_only(&db).await;
        assert!(html.contains(&format!("href=\"/series/{local_id}\">Library sequel</a>")));
    }

    #[tokio::test]
    async fn matched_release_keeps_the_anilist_link_when_two_parts_share_a_mal_id() {
        let (db, local_id) = matched_sequel().await;
        sqlx::query("INSERT INTO provider_metadata_cache(provider_id,mal_id,detail_json) VALUES (13,800,'{}')")
            .execute(&db).await.unwrap();
        let html = render_read_only(&db).await;
        assert!(html.contains("href=\"/series/12?by=anilist\">Matched sequel</a>"));
        assert!(!html.contains(&format!("href=\"/series/{local_id}\"")));
    }

    #[tokio::test]
    async fn matched_release_keeps_one_lookup_link_when_two_local_entries_claim_it() {
        let (db, first) = matched_sequel().await;
        let second = seed_series(&db, -801, "Other local entry").await;
        sqlx::query("INSERT INTO series_metadata_cache(series_id,provider_id,detail_json) VALUES (?,12,'{}')")
            .bind(second).execute(&db).await.unwrap();
        let html = render_read_only(&db).await;
        assert_eq!(
            html.matches("href=\"/series/12?by=anilist\">Matched sequel</a>")
                .count(),
            1
        );
        for id in [first, second] {
            assert!(!html.contains(&format!("href=\"/series/{id}\"")));
        }
    }

    #[tokio::test]
    async fn matched_release_uses_the_local_series_cache_to_identify_a_part() {
        let (db, local_id) = matched_sequel().await;
        sqlx::query("INSERT INTO provider_metadata_cache(provider_id,mal_id,detail_json) VALUES (13,800,'{}')")
            .execute(&db).await.unwrap();
        sqlx::query("INSERT INTO series_metadata_cache(series_id,provider_id,mal_id,detail_json) VALUES (?,12,800,'{}')")
            .bind(local_id).execute(&db).await.unwrap();
        let html = render_read_only(&db).await;
        assert!(html.contains(&format!("href=\"/series/{local_id}\">Library sequel</a>")));
    }

    #[tokio::test]
    async fn matched_release_prefers_the_exact_entry_to_a_cached_local_link() {
        let (db, mal_entry) = matched_sequel().await;
        let exact = seed_series(&db, 12, "Exact sequel").await;
        sqlx::query("INSERT INTO series_metadata_cache(series_id,provider_id,mal_id,detail_json) VALUES (?,12,800,'{}')")
            .bind(mal_entry).execute(&db).await.unwrap();
        let html = render_read_only(&db).await;
        assert!(html.contains(&format!("href=\"/series/{exact}\">Exact sequel</a>")));
        assert!(!html.contains(&format!("href=\"/series/{mal_entry}\"")));
    }
}
