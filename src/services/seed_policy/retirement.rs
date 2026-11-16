//! Retire exact completed source jobs. Payloads are deliberately retained.
//! There is no library delete, episode failure, acquisition or auto-resume here.
use super::*;
use crate::models::seed_policy;
use crate::services::download_client::source_guard::SOURCE_ACTION_LOCK;

pub(super) async fn run(state: &AppState) -> Result<(), String> {
    let cfg = crate::models::config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Settings unavailable")?;
    if !cfg.accepted_only_seeding || !cfg.retire_unaccepted_sources {
        return Ok(());
    }
    let pool = state.download_clients.read().await.clone();
    for (id, client) in &pool.clients {
        if client.protocol() != "torrent" {
            continue;
        }
        let items = match client.list_scoped().await {
            Ok(items) => items,
            Err(_) => continue, // Unavailable is never an empty client.
        };
        for item in items {
            let Some(hash) = normalize_info_hash(&item.hash) else {
                continue;
            };
            if !item.state_kind.is_complete() || item.progress != 1.0 {
                continue;
            }
            let owned = owners(&state.db, &hash).await?;
            if owned.len() != 1
                || owned[0].download_client_id != Some(*id)
                || !matches!(
                    owned[0].state.as_str(),
                    "failed" | "removed" | "cancelled" | "imported" | "replaced"
                )
            {
                continue;
            }
            let observed = review(state, &cfg, *id, &item, client, false).await;
            if observed.decision == Decision::Accepted {
                continue;
            }
            // The first pass has already stopped unknown active completions.
            // A successful source inspection can clear a cache-only hold. Errors
            // stay retryable and never become a permanent exclusion.
            let observed = if observed.can_retire {
                observed
            } else {
                review(state, &cfg, *id, &item, client, true).await
            };
            if !observed.can_retire {
                continue;
            }
            if let Err(e) = retire_one(state, *id, client, &item, &owned).await {
                let (verdict, reason) = observed.decision.fields();
                seed_policy::save(
                    &state.db,
                    &Review {
                        client_id: *id,
                        hash,
                        name: item.name,
                        verdict: verdict.into(),
                        reason,
                        control: format!("Removal deferred; files retained: {e}"),
                        checked_at: String::new(),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        // Finish a requested removal whose acknowledgement was lost, or whose
        // bookkeeping failed after delete. Never infer absence from a list error.
        let pending = seed_policy::retirements(&state.db)
            .await
            .map_err(|e| e.to_string())?;
        for row in pending
            .into_iter()
            .filter(|r| r.client_id == *id && r.retired_at.is_none())
        {
            let _post_processing = post_processing::POST_PROC_LOCK.lock().await;
            let _config = crate::handlers::settings::CONFIG_WRITE_LOCK.lock().await;
            let _source = SOURCE_ACTION_LOCK.write().await;
            if !enabled(state, *id, client).await? {
                continue;
            }
            let current = owners(&state.db, &row.hash).await?;
            if current.len() != 1
                || current[0].id != row.retirement_owner
                || current[0].grabbed_at != row.retirement_attempt
            {
                continue;
            }
            if client
                .list_scoped()
                .await?
                .iter()
                .all(|i| !i.hash.eq_ignore_ascii_case(&row.hash))
            {
                seed_policy::finish_retirement(&state.db, &row).await?;
            }
        }
    }
    Ok(())
}

async fn enabled(
    state: &AppState,
    id: i64,
    client: &Arc<dyn DownloadClient>,
) -> Result<bool, String> {
    let cfg = crate::models::config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Settings unavailable")?;
    let row = download_clients::get_by_id(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(cfg.accepted_only_seeding
        && cfg.retire_unaccepted_sources
        && row.is_some_and(|r| r.enabled && !r.is_usenet())
        && state
            .download_clients
            .read()
            .await
            .clients
            .get(&id)
            .is_some_and(|c| Arc::ptr_eq(c, client)))
}

async fn retire_one(
    state: &AppState,
    id: i64,
    client: &Arc<dyn DownloadClient>,
    observed: &DownloadItem,
    owned: &[Owner],
) -> Result<(), String> {
    let hash = normalize_info_hash(&observed.hash).ok_or("Invalid source identity")?;
    let _post_processing = post_processing::POST_PROC_LOCK.lock().await;
    let _config = crate::handlers::settings::CONFIG_WRITE_LOCK.lock().await;
    let _source = SOURCE_ACTION_LOCK.write().await;
    if !enabled(state, id, client).await? {
        return Err("Policy or client changed".into());
    }
    let in_flight: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_grabs WHERE lower(info_hash)=?)")
            .bind(&hash)
            .fetch_one(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    if in_flight || owners(&state.db, &hash).await? != owned {
        return Err("Source has an in-flight selection or changed ownership".into());
    }
    let items = client.list_scoped().await?;
    let mut item = items
        .into_iter()
        .find(|i| i.hash.eq_ignore_ascii_case(&hash))
        .ok_or("Source no longer in the client scope")?;
    if item.save_path != observed.save_path
        || item.content_path != observed.content_path
        || item.size != observed.size
        || item.progress != 1.0
        || !item.state_kind.is_complete()
        || item.state_kind == DownloadItemState::CheckingSeed
    {
        return Err("Native source identity or completion changed".into());
    }
    if item.state_kind != DownloadItemState::PausedComplete {
        client.pause(&hash).await?;
        item = client
            .list_scoped()
            .await?
            .into_iter()
            .find(|i| i.hash.eq_ignore_ascii_case(&hash))
            .ok_or("Source left the client during stop")?;
    }
    if item.state_kind != DownloadItemState::PausedComplete || item.progress != 1.0 {
        return Err("Native stop has not been confirmed".into());
    }
    let cfg = crate::models::config::get_config(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Settings unavailable")?;
    let files = client.get_files(&hash).await?;
    if files.is_empty() {
        return Err("Source manifest unavailable".into());
    }
    let checked = review(state, &cfg, id, &item, client, false).await;
    if !checked.can_retire {
        return Err("Current source evidence requires a hold".into());
    }
    let again = client.get_files(&hash).await?;
    if !same_manifest(&files, &again) || owners(&state.db, &hash).await? != owned {
        return Err("Source manifest or ownership changed".into());
    }
    let in_flight: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_grabs WHERE lower(info_hash)=?)")
            .bind(&hash)
            .fetch_one(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    if in_flight {
        return Err("Source has an in-flight selection".into());
    }
    let (verdict, reason) = checked.decision.fields();
    seed_policy::save(
        &state.db,
        &Review {
            client_id: id,
            hash: hash.clone(),
            name: item.name.clone(),
            verdict: verdict.into(),
            reason: reason.clone(),
            control: "Removal requested; source files retained".into(),
            checked_at: String::new(),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let snapshot = serde_json::to_string(&serde_json::json!({"owner":owned[0],"item":{
        "hash":hash,"name":item.name,"save_path":item.save_path,"content_path":item.content_path,
        "size":item.size,"category":item.category},"files":files}))
    .map_err(|e| e.to_string())?;
    seed_policy::request_retirement(
        &state.db,
        id,
        &hash,
        owned[0].id,
        &owned[0].grabbed_at,
        &reason,
        &snapshot,
    )
    .await?;
    // Never use native payload deletion: it bypasses Ryokan recovery and may
    // remove a shared path. Source-file cleanup needs its own guarded operation.
    client.delete(&hash, false).await?;
    if client
        .list_scoped()
        .await?
        .iter()
        .any(|i| i.hash.eq_ignore_ascii_case(&hash))
    {
        return Err("Awaiting native removal confirmation".into());
    }
    let row = seed_policy::retirements(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|r| r.client_id == id && r.hash == hash)
        .ok_or("Retirement request unavailable")?;
    seed_policy::finish_retirement(&state.db, &row).await
}
