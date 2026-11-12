//! Automatic torrent availability. Usenet has no peer counts; interactive
//! searches remain unfiltered so a person may explicitly choose a stalled item.
use std::collections::HashMap;

use super::{
    indexers::Indexer,
    nyaa::{self, SearchResult},
    rss::{RssItem, RssSource},
};
use crate::{IndexerCache, models::config::Config};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Torrent,
    Usenet,
    Unknown,
}

impl Protocol {
    pub fn from_indexer_kind(kind: &str) -> Self {
        match kind {
            "torznab" => Self::Torrent,
            "newznab" => Self::Usenet,
            _ => Self::Unknown,
        }
    }
    pub fn from_client(protocol: &str) -> Self {
        match protocol {
            "torrent" => Self::Torrent,
            "usenet" => Self::Usenet,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug)]
pub struct Policy {
    minimum: i32,
    protocols: HashMap<i64, Protocol>,
}

impl Policy {
    pub fn from_indexers(cfg: &Config, indexers: &[std::sync::Arc<dyn Indexer>]) -> Self {
        Self {
            minimum: cfg.minimum_torrent_seeders,
            protocols: indexers
                .iter()
                .map(|indexer| (indexer.id(), Protocol::from_indexer_kind(indexer.kind())))
                .collect(),
        }
    }
    pub async fn load(cfg: &Config, cache: &IndexerCache) -> Self {
        let snapshot = cache.read().await.clone();
        Self::from_indexers(cfg, &snapshot)
    }
    pub fn check(&self, protocol: Protocol, seeders: Option<i32>) -> Result<(), String> {
        if protocol == Protocol::Usenet {
            return Ok(());
        }
        if protocol == Protocol::Unknown {
            return Err("Release protocol is unknown; review it in interactive search.".into());
        }
        if !(0..=10_000).contains(&self.minimum) {
            return Err("Minimum torrent seeders is invalid; review Quality settings.".into());
        }
        if self.minimum == 0 {
            return Ok(());
        }
        match seeders.filter(|n| *n >= 0) {
            Some(n) if n >= self.minimum => Ok(()),
            Some(n) => Err(format!(
                "Torrent has {n} seeders; automatic downloads require at least {}.",
                self.minimum
            )),
            None => Err("Torrent seeder count is unknown; review it in interactive search.".into()),
        }
    }
    pub fn check_result(&self, result: &SearchResult) -> Result<(), String> {
        let protocol = result.indexer_id.map_or(Protocol::Torrent, |id| {
            self.protocols
                .get(&id)
                .copied()
                .unwrap_or(Protocol::Unknown)
        });
        self.check(protocol, Some(result.seeders))
    }
    pub fn allows_result(&self, result: &SearchResult) -> bool {
        self.check_result(result).is_ok()
    }

    /// Filter before cross-indexer attribution as well as before scoring.
    /// A preferred source reporting no seeds must not own and hide a duplicate
    /// from another source with sufficient availability.
    pub fn check_indexer_release(&self, release: &super::indexers::Release) -> Result<(), String> {
        let protocol = self
            .protocols
            .get(&release.indexer_id)
            .copied()
            .unwrap_or(Protocol::Unknown);
        self.check(protocol, Some(release.seeders))
    }

    /// A missing feed statistic is not zero. Nyaa-backed direct feeds may
    /// resolve it from one bounded, origin-checked listing request. A failed
    /// lookup stays unknown and can be reconsidered on the next RSS poll.
    pub async fn check_rss(&self, item: &mut RssItem, client_protocol: &str) -> Result<(), String> {
        let protocol = match &item.source {
            RssSource::Indexer { kind, .. } => Protocol::from_indexer_kind(kind),
            RssSource::Nyaa => Protocol::Torrent,
            RssSource::UserFeed { .. } => Protocol::from_client(client_protocol),
        };
        if protocol == Protocol::Torrent && self.minimum > 0 && item.seeders.is_none() {
            let page = [&item.link, &item.torrent]
                .into_iter()
                .find_map(|link| nyaa::view_page_url(link));
            if let Some(page) = page {
                let fetched = tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    nyaa::fetch_view_result(&page, &nyaa::SearchOptions::default()),
                )
                .await;
                if let Ok(Ok(result)) = fetched {
                    let expected =
                        crate::services::download_client::normalize_info_hash(&item.info_hash)
                            .or_else(|| {
                                crate::services::download_client::normalize_info_hash(
                                    &nyaa::extract_hash(item.grab_url()),
                                )
                            });
                    let same_source = expected
                        .as_ref()
                        .is_some_and(|hash| hash == &result.info_hash)
                        || (expected.is_none()
                            && nyaa::view_page_url(item.grab_url()).as_deref()
                                == Some(page.as_str()));
                    if same_source {
                        item.seeders = Some(result.seeders);
                    }
                }
            }
        }
        self.check(protocol, item.seeders)
    }
}

#[cfg(test)]
mod tests;
