//! Add the selected releases from a Discover franchise.
use std::collections::HashSet;

use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use axum_htmx::HxRequest;

use super::{
    AddOptions, AddPage, AppState, Choice, GuideInput, ReleaseOrder, add_entry, add_series_form,
    config, discovery, exact_detail, htmx_aware_redirect, metadata_cache, series,
    supported_provider,
};

struct Selection {
    seed: i64,
    query: String,
    source: String,
    mode: Choice,
    order: ReleaseOrder,
    releases: HashSet<i64>,
    guide_input: Option<GuideInput>,
}

fn provider_id(value: &str) -> Option<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|id| supported_provider(*id))
}

fn parse(fields: Vec<(String, String)>) -> Result<Selection, &'static str> {
    let mut seed = None;
    let mut query = None;
    let mut source = None;
    let mut mode = None;
    let mut order = None;
    let mut guide_name = None;
    let mut guide_entries = None;
    let mut releases = HashSet::new();
    for (name, value) in fields {
        match name.as_str() {
            "franchise_id" if seed.is_none() => {
                seed = Some(provider_id(&value).ok_or("Choose a valid franchise.")?);
            }
            "q" if query.is_none() => query = Some(value),
            "source" if source.is_none() => source = Some(value),
            "order" if order.is_none() => order = Some(ReleaseOrder::from_param(&value)),
            "guide_name" if guide_name.is_none() => guide_name = Some(value),
            "guide_entries" if guide_entries.is_none() => guide_entries = Some(value),
            "monitor_mode" if mode.is_none() => {
                mode = Some(match value.as_str() {
                    "all" => Choice::All,
                    "future" => Choice::Future,
                    "none" => Choice::None,
                    _ => return Err("Choose how to monitor the selected releases."),
                });
            }
            "release" => {
                releases.insert(provider_id(&value).ok_or("Choose a valid release.")?);
            }
            "franchise_id" | "q" | "source" | "monitor_mode" | "order" | "guide_name"
            | "guide_entries" => {
                return Err("Reload the franchise and review your choices.");
            }
            _ => {}
        }
    }
    let seed = seed.ok_or("Choose a valid franchise.")?;
    let guide_input = match (guide_name, guide_entries) {
        (Some(name), Some(entries)) if seed > 0 => Some(GuideInput { name, entries }),
        (None, None) => None,
        _ => return Err("Open an AniList franchise and preview the guide again."),
    };
    Ok(Selection {
        seed,
        query: query.unwrap_or_default(),
        source: source.unwrap_or_default(),
        mode: mode.ok_or("Choose how to monitor the selected releases.")?,
        order: order.unwrap_or_default(),
        releases,
        guide_input,
    })
}

async fn selection_error(
    state: &AppState,
    mut view: AddPage,
    seed: i64,
    resolved: discovery::ResolvedFranchise,
    status: StatusCode,
    message: String,
) -> Response {
    let franchise = discovery::project(state, &view, seed, resolved).await;
    view.select = franchise.can_select && (view.guide_input.is_none() || view.guide.is_some());
    view.franchise = Some(franchise);
    view.selection_notice = message;
    let mut response = view.into_response();
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

pub(in crate::handlers::library) async fn save(
    State(state): State<AppState>,
    HxRequest(htmx): HxRequest,
    Form(fields): Form<Vec<(String, String)>>,
) -> Response {
    let selection = match parse(fields) {
        Ok(selection) => selection,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let cfg = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let seed = selection.seed;
    let mut view = AddPage::new(&cfg, selection.query, &selection.source, Some(seed));
    view.selected_releases = selection.releases;
    view.selection_mode = selection.mode;
    view.order = selection.order;
    view.guide_input = selection.guide_input;
    let mut resolved = match discovery::resolve_group(&state, &view, seed).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(%error, seed, "Could not confirm selected franchise");
            view.error = if view.guide_input.is_some() {
                "Could not load this franchise. Try the preview again."
            } else {
                "Could not load this franchise. Search for it again."
            };
            return (StatusCode::BAD_GATEWAY, view.into_response()).into_response();
        }
    };
    if let Some(input) = &view.guide_input {
        match input.resolve(&resolved.group) {
            Ok(guide) => view.guide = Some(guide),
            Err(message) => {
                return selection_error(
                    &state,
                    view,
                    seed,
                    resolved,
                    StatusCode::CONFLICT,
                    message,
                )
                .await;
            }
        }
    }
    if view.selected_releases.is_empty() {
        return selection_error(
            &state,
            view,
            seed,
            resolved,
            StatusCode::BAD_REQUEST,
            "Select at least one release.".into(),
        )
        .await;
    }
    let known: HashSet<_> = if let Some(guide) = &view.guide {
        guide.releases.iter().map(|release| release.id).collect()
    } else {
        resolved
            .group
            .entries
            .iter()
            .flat_map(|entry| std::iter::once(entry.provider_id).chain(entry.library_provider_id))
            .collect()
    };
    if !view.selected_releases.is_subset(&known) {
        let message = if view.guide.is_some() {
            "Select only releases in this watch guide."
        } else {
            "The franchise changed. Review your choices and try again."
        };
        return selection_error(
            &state,
            view,
            seed,
            resolved,
            StatusCode::CONFLICT,
            message.into(),
        )
        .await;
    }

    let mut details = Vec::new();
    for entry in &resolved.group.entries {
        if entry.series_id.is_some() || !view.selected_releases.contains(&entry.provider_id) {
            continue;
        }
        match exact_detail(&state.db, entry.provider_id).await {
            Ok(detail) => details.push(detail),
            Err(error) => {
                tracing::warn!(%error, provider_id = entry.provider_id, "Could not confirm selected release");
                let message = format!("Could not verify {}. Try again later.", entry.title);
                return selection_error(
                    &state,
                    view,
                    seed,
                    resolved,
                    StatusCode::BAD_GATEWAY,
                    message,
                )
                .await;
            }
        }
    }

    let destination = format!("{}#add-anime-releases-title", view.franchise_path(seed));
    if details.is_empty() {
        return htmx_aware_redirect(htmx, &destination);
    }
    // Once the first write starts, finish the selection if the tab disconnects.
    let mode = view.selection_mode.into();
    let write_state = state.clone();
    let outcome = tokio::spawn(async move {
        if details.len() > 1 {
            // Folder planning must see all verified links between the selected releases
            // before the first release reserves a franchise folder.
            let mut tx = write_state.db.begin().await?;
            for detail in &details {
                metadata_cache::upsert_provider(&mut *tx, detail.id, detail.id_mal, detail).await?;
            }
            tx.commit().await?;
        }
        let mut added = 0;
        let mut saved = Vec::new();
        let mut failed = Vec::new();
        for detail in details {
            let form = add_series_form(&detail);
            let title = form.title.clone();
            let options = AddOptions {
                mode,
                search_now: false,
                detail: &detail,
            };
            match add_entry(write_state.clone(), form, Some(options)).await {
                Ok((id, created, _)) => {
                    added += usize::from(created);
                    saved.push((detail.id, id));
                }
                Err((_, error)) => {
                    tracing::error!(%error, provider_id = detail.id, "Could not add selected release");
                    failed.push(title);
                }
            }
        }
        Ok::<_, sqlx::Error>((added, saved, failed))
    })
    .await;

    match outcome {
        Ok(Ok((_, _, failed))) if failed.is_empty() => htmx_aware_redirect(htmx, &destination),
        Ok(Ok((added, saved, failed))) => {
            for (provider_id, series_id) in saved {
                if let Ok(Some(row)) = series::get_by_id(&state.db, series_id).await
                    && let Some(entry) = resolved
                        .group
                        .entries
                        .iter_mut()
                        .find(|e| e.provider_id == provider_id)
                {
                    entry.series_id = Some(row.id);
                    entry.library_provider_id = Some(row.anilist_id);
                }
            }
            resolved.group.tracked = resolved
                .group
                .entries
                .iter()
                .filter(|entry| entry.series_id.is_some())
                .count();
            resolved.group.outside = resolved.group.entries.len() - resolved.group.tracked;
            selection_error(
                &state,
                view,
                seed,
                resolved,
                StatusCode::CONFLICT,
                format!(
                    "Added {added} of the selected releases. Could not add {}.",
                    failed.join(", ")
                ),
            )
            .await
        }
        Ok(Err(error)) => {
            tracing::error!(%error, "Could not save metadata for the selected releases");
            selection_error(
                &state,
                view,
                seed,
                resolved,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the selected releases. Try again.".into(),
            )
            .await
        }
        Err(error) => {
            tracing::error!(%error, "Selected release add did not finish");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not finish the selection. Reload the franchise to see the current library state.",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::metadata_cache,
        services::{anilist, download_client::qbittorrent::QbitClient},
        test_support::{build_test_app_state, in_memory_pool, seed_series},
    };
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use serde_json::{Value, json};
    use std::{sync::Arc, time::Duration};
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path_regex},
    };

    const MAIN: i64 = 970_001;
    const SEQUEL: i64 = 970_002;
    const EXTRAS: i64 = 970_003;
    const OTHER: i64 = 970_004;
    const COLD_MAIN: i64 = 980_001;
    const COLD_SEQUEL: i64 = 980_002;
    const COLD_FILM: i64 = 980_003;
    const COLD_QUERY: &str = "Frozen Orbit";

    fn media(id: i64, title: &str, year: i32) -> Value {
        json!({
            "id": id, "title": {"english": title, "romaji": title},
            "type": "ANIME", "format": "MOVIE", "status": "FINISHED", "episodes": 1,
            "seasonYear": year, "startDate": {"year": year, "month": 4, "day": 1}
        })
    }

    fn franchise() -> [Value; 3] {
        let mut main = media(MAIN, "Sora", 2020);
        let sequel = media(SEQUEL, "Sora Returns", 2021);
        let extras = media(EXTRAS, "Sora OVA", 2022);
        main["relations"]["edges"] = json!([
            {"relationType": "SEQUEL", "node": sequel.clone()},
            {"relationType": "SIDE_STORY", "node": extras.clone()}
        ]);
        [main, sequel, extras]
    }

    fn cold_franchise() -> [Value; 3] {
        let mut main = media(COLD_MAIN, COLD_QUERY, 2020);
        main["format"] = json!("TV");
        let mut sequel = media(COLD_SEQUEL, "Frozen Orbit Season 2", 2022);
        sequel["format"] = json!("TV");
        let mut film = media(COLD_FILM, "Frozen Orbit Film", 2021);
        film["relations"]["edges"] = json!([{"relationType": "PREQUEL", "node": sequel.clone()}]);
        sequel["relations"]["edges"] = json!([{"relationType": "PREQUEL", "node": main.clone()}]);
        [main, sequel, film]
    }

    async fn submit(state: &AppState, seed: i64, releases: &str) -> Response {
        crate::handlers::library::detail::routes()
            .with_state(state.clone())
            .oneshot(
                Request::post("/discover/add-releases")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "franchise_id={seed}&source=al&{releases}"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn adds_only_selected_releases_without_immediate_acquisition() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("^/nyaa/?$"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        for item in franchise().into_iter().chain(cold_franchise()) {
            Mock::given(method("POST"))
                .and(body_partial_json(json!({"variables": {"id": item["id"]}})))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"data": {"Media": item}})),
                )
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"variables": {"search": COLD_QUERY}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"Page": {"media": cold_franchise()}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Keep process-wide provider overrides local to this fixture.
        let (_, module) = module_path!().split_once("::").unwrap();
        let test = format!("{module}::run_bulk_add_fixture");
        let base = server.uri();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", &test, "--nocapture"])
            .env("RYOKAN_ANILIST_API_BASE", &base)
            .env("JIKAN_API_BASE", &base)
            .env("RYOKAN_KITSU_API_BASE", &base)
            .env("RYOKAN_ANIBRIDGE_MAPPINGS_URL", &base)
            .env("RYOKAN_NYAA_API_BASE", format!("{base}/nyaa"))
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[ignore = "run by adds_only_selected_releases_without_immediate_acquisition"]
    #[tokio::test]
    async fn run_bulk_add_fixture() {
        let db = in_memory_pool().await;
        let config = config::Config {
            auto_grab_on_add: true,
            search_on_monitoring_change: true,
            ..Default::default()
        };
        config::save_config(&db, &config).await.unwrap();
        for item in franchise()
            .into_iter()
            .chain([media(OTHER, "Separate Title", 2020)])
        {
            let detail = anilist::parse_media_node(&item).unwrap();
            metadata_cache::upsert_provider(&db, detail.id, None, &detail)
                .await
                .unwrap();
        }
        let base = std::env::var("RYOKAN_ANILIST_API_BASE").unwrap();
        let client = Arc::new(QbitClient::new(&base, "fixture", "fixture", "anime"));
        let state = build_test_app_state(db.clone(), Some(client));
        let baseline = Arc::strong_count(&state.download_clients);

        let response = submit(&state, MAIN, "monitor_mode=all").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let html = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(
            std::str::from_utf8(&html)
                .unwrap()
                .contains("Select at least one release.")
        );

        let response = submit(
            &state,
            MAIN,
            &format!("monitor_mode=all&release={MAIN}&release={OTHER}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(crate::test_support::count_series(&db).await, 0);

        let form = format!(
            "monitor_mode=all&release={MAIN}&release={SEQUEL}&release={MAIN}&search_now=true"
        );
        let response = submit(&state, MAIN, &form).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            format!("/discover?source=al&q=&franchise_id={MAIN}#add-anime-releases-title")
        );

        let response = submit(
            &state,
            MAIN,
            &format!("monitor_mode=none&release={MAIN}&release={SEQUEL}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let tracked = series::get_all(&db).await.unwrap();
        assert_eq!(
            tracked
                .iter()
                .map(|row| row.anilist_id)
                .collect::<HashSet<_>>(),
            HashSet::from([MAIN, SEQUEL])
        );
        for row in tracked {
            assert_eq!(row.monitor_mode_enum(), super::super::MonitorMode::All);
            assert!(row.monitor_mode_manual_override);
            assert_eq!(
                crate::models::monitoring::get_monitored_episode_numbers(&db, row.id)
                    .await
                    .unwrap(),
                [1]
            );
        }
        let follows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_follows")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(follows, 0);

        // An automatic search would keep a clone of the state until it finishes.
        tokio::time::timeout(Duration::from_secs(10), async {
            while Arc::strong_count(&state.download_clients) > baseline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let grabs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(grabs, 0);

        assert_cold_selection_keeps_the_same_franchise().await;
    }

    async fn assert_cold_selection_keeps_the_same_franchise() {
        let db = in_memory_pool().await;
        crate::models::library_layout::enable(&db).await.unwrap();
        let root = tempfile::tempdir().unwrap();
        config::save_config(
            &db,
            &config::Config {
                media_root: root.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let main = seed_series(&db, COLD_MAIN, COLD_QUERY).await;
        sqlx::query("UPDATE series SET folder_name='Frozen Orbit/Frozen Orbit' WHERE id=?")
            .bind(main)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let form = format!(
            "q=Frozen%20Orbit&order=watch&monitor_mode=all&release={COLD_SEQUEL}&release={COLD_FILM}"
        );
        let response = submit(&state, COLD_MAIN, &form).await;
        let status = response.status();
        let location = response.headers().get(header::LOCATION).cloned();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            location.unwrap(),
            format!(
                "/discover?source=al&q=Frozen%20Orbit&franchise_id={COLD_MAIN}&order=watch#add-anime-releases-title"
            )
        );
        let folders: Vec<(i64, String)> =
            sqlx::query_as("SELECT anilist_id,folder_name FROM series ORDER BY anilist_id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            folders,
            [
                (COLD_MAIN, "Frozen Orbit/Frozen Orbit".into()),
                (COLD_SEQUEL, "Frozen Orbit/Frozen Orbit Season 2".into()),
                (COLD_FILM, "Frozen Orbit/Frozen Orbit Film".into())
            ]
        );
    }
}
