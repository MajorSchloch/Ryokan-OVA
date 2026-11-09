//! Persisted storage ownership, independent of acquisition follows.
use super::{config, series};
use crate::services::{anilist::AnimeDetail, library_link::pick_title, media, naming};
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS library_path_policy (
           id INTEGER PRIMARY KEY CHECK(id=1), enabled BOOLEAN NOT NULL);
         INSERT OR IGNORE INTO library_path_policy SELECT 1, NOT EXISTS(SELECT 1 FROM series);
         CREATE TABLE IF NOT EXISTS library_franchises (
           folder TEXT PRIMARY KEY COLLATE NOCASE, owner_provider_id INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS library_franchise_members (
           provider_id INTEGER PRIMARY KEY, folder TEXT NOT NULL COLLATE NOCASE
             REFERENCES library_franchises(folder));",
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn enabled<'e>(db: impl sqlx::SqliteExecutor<'e>) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT enabled FROM library_path_policy WHERE id=1")
        .fetch_one(db)
        .await
}

fn invalid(message: &str) -> sqlx::Error {
    sqlx::Error::Protocol(message.into())
}

#[derive(Clone, Default)]
pub struct Catalog {
    pub enabled: bool,
    language: String,
    roots: BTreeMap<String, i64>,
    members: HashMap<i64, String>,
    edges: HashMap<i64, BTreeSet<i64>>,
    names: HashMap<i64, (String, Option<i32>)>,
    extras: HashSet<i64>,
    folders: Vec<(i64, String)>,
    disk: HashSet<String>,
    planned: HashMap<i64, Assignment>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Assignment {
    pub path: String,
    pub root: String,
    pub owner: i64,
}

impl Catalog {
    pub fn failed(error: sqlx::Error) -> Self {
        Self {
            enabled: true,
            error: Some(error.to_string()),
            ..Self::default()
        }
    }
    pub async fn load(db: &mut SqliteConnection) -> Result<Self, sqlx::Error> {
        let mut result = Self {
            enabled: enabled(&mut *db).await?,
            ..Self::default()
        };
        if !result.enabled {
            return Ok(result);
        }
        let prefs = config::get_naming_prefs(&mut *db).await;
        result.language = prefs.title_language.clone();
        for row in sqlx::query("SELECT anilist_id,title,title_english,title_romaji,title_native,season_year,folder_name,format FROM series ORDER BY id")
            .fetch_all(&mut *db).await? {
            let id = row.get("anilist_id");
            result.note_format(id,row.get("format"));
            let name = pick_title(&prefs.title_language,row.get("title_english"),row.get("title_romaji"),row.get("title_native"));
            let name = if name.is_empty() {row.get::<&str,_>("title")} else {name};
            result.names.insert(id,(name.into(),row.get("season_year")));
            let path: String = row.get("folder_name");
            if let Some((root, _)) = path.split_once('/') {
                result.roots.entry(root.into()).or_insert(id);
                result.members.insert(id,root.into());
            }
            result.folders.push((id,path));
        }
        for (root, owner) in sqlx::query_as::<_, (String, i64)>(
            "SELECT folder,owner_provider_id FROM library_franchises",
        )
        .fetch_all(&mut *db)
        .await?
        {
            result.roots.insert(root.clone(), owner);
            result.members.entry(owner).or_insert(root);
        }
        for (id, root) in sqlx::query_as::<_, (i64, String)>(
            "SELECT provider_id,folder FROM library_franchise_members",
        )
        .fetch_all(&mut *db)
        .await?
        {
            if result.members.get(&id).is_some_and(|old| old != &root) {
                return Err(invalid(
                    "Stored franchise ownership differs from its library path. Review the move.",
                ));
            }
            result.members.insert(id, root);
        }
        // Explicit selected members and receipts reserve paths before their series is added.
        for (id,root,owner) in sqlx::query_as::<_,(i64,String,i64)>(
            "SELECT c.anilist_id,f.library_folder,f.anilist_id FROM franchise_choices c JOIN release_follows f ON f.id=c.follow_id WHERE c.selected=1 AND f.library_folder!=''
             UNION SELECT r.anilist_id,f.library_folder,f.anilist_id FROM followed_releases r JOIN release_follows f ON f.id=r.follow_id WHERE f.library_folder!=''
               AND NOT EXISTS(SELECT 1 FROM franchise_choices c WHERE c.anilist_id=r.anilist_id AND (c.selected=0 OR c.follow_id!=f.id))")
             .fetch_all(&mut *db).await? {
            if result.members.get(&id).is_some_and(|old| old != &root) {
                return Err(invalid("Conflicting franchise storage owners"));
            }
            result.roots.insert(root.clone(),owner);
            result.members.insert(id,root);
        }
        // Read both cache representations. A missing intermediate detail must not split a known chain.
        for (owner,peer,relation,kind) in sqlx::query_as::<_,(i64,i64,String,String)>(
            "SELECT s.anilist_id,r.related_provider_id,r.relation_type,r.media_type FROM series_relations_cache r JOIN series s ON s.id=r.series_id
             UNION SELECT provider_id,related_provider_id,relation_type,media_type FROM provider_relations_cache")
             .fetch_all(&mut *db).await? {
            result.edge(owner,peer,&relation,&kind);
        }
        for (id,json) in sqlx::query_as::<_,(i64,String)>(
            "SELECT provider_id,detail_json FROM provider_metadata_cache WHERE provider_id>0
             UNION ALL SELECT provider_id,detail_json FROM series_metadata_cache WHERE provider_id>0")
             .fetch_all(&mut *db).await? {
            let Ok(detail) = serde_json::from_str::<AnimeDetail>(&json) else {continue};
            if detail.id != id {continue}
            result.note_format(id,&detail.format);
            let title = pick_title(&prefs.title_language,&detail.title_english,&detail.title_romaji,&detail.title_native);
            if !title.is_empty() {result.names.insert(id,(title.into(),detail.season_year));}
            for peer in detail.relations {
                result.edge(id,peer.id,&peer.relation_type,&peer.media_type);
                if peer.media_type=="ANIME" {
                    result.note_format(peer.id,&peer.format);
                    let title=pick_title(&prefs.title_language,&peer.title_english,&peer.title_romaji,&peer.title_native);
                    if !title.is_empty() {result.names.entry(peer.id).or_insert((title.into(),peer.season_year));}
                }
            }
        }
        let root: Option<String> = sqlx::query_scalar("SELECT media_root FROM config WHERE id=1")
            .fetch_optional(&mut *db)
            .await?;
        if let Some(root) = root.filter(|r| !r.trim().is_empty()) {
            result.disk.extend(
                media::list_media_folders(&root)
                    .into_iter()
                    .map(|s| s.to_ascii_lowercase()),
            );
            // Include unowned release directories even when they have no NFO yet.
            for parent in std::fs::read_dir(&root).into_iter().flatten().flatten() {
                if !parent.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                for child in std::fs::read_dir(parent.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    if child.path().is_dir() {
                        result.disk.insert(
                            format!(
                                "{}/{}",
                                parent.file_name().to_string_lossy(),
                                child.file_name().to_string_lossy()
                            )
                            .to_ascii_lowercase(),
                        );
                    }
                }
            }
        }
        Ok(result)
    }

    pub fn seed(&mut self, id: i64, names: naming::SeriesNames<'_>) {
        let title = pick_title(&self.language, names.english, names.romaji, names.native);
        let title = if title.is_empty() { names.title } else { title };
        self.names.entry(id).or_insert((title.into(), names.year));
    }

    fn edge(&mut self, owner: i64, peer: i64, relation: &str, kind: &str) {
        if owner <= 0
            || peer <= 0
            || kind != "ANIME"
            || !matches!(
                relation,
                "PREQUEL"
                    | "SEQUEL"
                    | "ALTERNATIVE"
                    | "SIDE_STORY"
                    | "SPIN_OFF"
                    | "PARENT"
                    | "PARENT_STORY"
                    | "SUMMARY"
                    | "FULL_STORY"
            )
        {
            return;
        }
        match relation {
            "SIDE_STORY" | "SPIN_OFF" | "SUMMARY" => {
                self.extras.insert(peer);
            }
            "PARENT" | "PARENT_STORY" | "FULL_STORY" => {
                self.extras.insert(owner);
            }
            _ => {}
        }
        self.edges.entry(owner).or_default().insert(peer);
        self.edges.entry(peer).or_default().insert(owner);
    }

    fn note_format(&mut self, id: i64, format: &str) {
        if matches!(format, "SPECIAL" | "MUSIC") {
            self.extras.insert(id);
        }
    }

    pub fn plan(
        &mut self,
        id: i64,
        leaf: String,
        year: Option<i32>,
        except: Option<i64>,
    ) -> Result<Assignment, sqlx::Error> {
        if let Some(error) = &self.error {
            return Err(invalid(error));
        }
        if let Some(assignment) = self.planned.get(&id) {
            return Ok(assignment.clone());
        }
        if !self.enabled {
            return Err(invalid("Franchise storage is not enabled"));
        }
        if id == 0 || !media::usable_folder_name(&leaf) {
            return Err(invalid(
                "A verified identity and readable release name are required",
            ));
        }
        self.names.entry(id).or_insert((leaf.clone(), year));
        let mut connected = BTreeSet::from([id]);
        let mut pending = vec![id];
        while let Some(current) = pending.pop() {
            for &other in self.edges.get(&current).into_iter().flatten() {
                if connected.insert(other) {
                    pending.push(other);
                }
            }
        }
        let (root, owner) = if let Some(root) = self.members.get(&id) {
            (
                root.clone(),
                *self
                    .roots
                    .get(root)
                    .ok_or_else(|| invalid("Missing franchise owner"))?,
            )
        } else {
            let roots: BTreeSet<_> = connected
                .iter()
                .filter_map(|id| self.members.get(id).cloned())
                .collect();
            if roots.len() > 1 {
                return Err(invalid(
                    "Related releases use different franchise folders. Review their ownership before adding this release.",
                ));
            }
            if let Some(root) = roots.first() {
                (root.clone(), self.roots[root])
            } else {
                let owner = connected
                    .iter()
                    .filter(|id| self.names.contains_key(id))
                    .min_by_key(|id| {
                        let (title, year) = &self.names[id];
                        (
                            self.extras.contains(id),
                            year.is_none(),
                            year.unwrap_or(i32::MAX),
                            title.to_ascii_lowercase(),
                            **id,
                        )
                    })
                    .copied()
                    .unwrap_or(id);
                let (title, root_year) = self
                    .names
                    .get(&owner)
                    .cloned()
                    .unwrap_or((leaf.clone(), year));
                let mut base = media::sanitize_folder_name(&title);
                base.truncate(base.floor_char_boundary(255));
                if base.is_empty() {
                    base = leaf.clone()
                }
                let root = series::unique_folder_among(base, root_year, |candidate| {
                    self.roots.keys().any(|r| r.eq_ignore_ascii_case(candidate))
                        || self.disk.contains(&candidate.to_ascii_lowercase())
                        || self.folders.iter().any(|(other, p)| {
                            Some(*other) != except && media::library_paths_overlap(candidate, p)
                        })
                });
                (root, owner)
            }
        };
        if !media::usable_folder_name(&root)
            || self
                .folders
                .iter()
                .any(|(other, p)| Some(*other) != except && p.eq_ignore_ascii_case(&root))
        {
            return Err(invalid(
                "Another release owns the franchise parent. Migrate it before adding a release.",
            ));
        }
        let path = series::unique_folder_among(format!("{root}/{leaf}"), year, |candidate| {
            self.folders.iter().any(|(other, p)| {
                Some(*other) != except && media::library_paths_overlap(p, candidate)
            }) || (self.disk.contains(&candidate.to_ascii_lowercase())
                && !self
                    .folders
                    .iter()
                    .any(|(other, p)| Some(*other) == except && p.eq_ignore_ascii_case(candidate)))
        });
        self.roots.insert(root.clone(), owner);
        self.members.insert(id, root.clone());
        self.members.entry(owner).or_insert_with(|| root.clone());
        self.folders.push((id, path.clone()));
        let assignment = Assignment { path, root, owner };
        self.planned.insert(id, assignment.clone());
        Ok(assignment)
    }
}

pub async fn record(
    db: &mut SqliteConnection,
    id: i64,
    assignment: &Assignment,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO library_franchises(folder,owner_provider_id) VALUES(?,?) ON CONFLICT(folder) DO NOTHING")
        .bind(&assignment.root).bind(assignment.owner).execute(&mut *db).await?;
    let old: Option<String> =
        sqlx::query_scalar("SELECT folder FROM library_franchise_members WHERE provider_id=?")
            .bind(id)
            .fetch_optional(&mut *db)
            .await?;
    if old.is_some_and(|old| old != assignment.root) {
        return Err(invalid("A different franchise owns this release"));
    }
    sqlx::query("INSERT INTO library_franchise_members(provider_id,folder) VALUES(?,?) ON CONFLICT(provider_id) DO NOTHING")
        .bind(id).bind(&assignment.root).execute(&mut *db).await?;
    Ok(())
}

pub async fn record_move(
    db: &mut SqliteConnection,
    series_id: i64,
    path: &str,
) -> Result<(), sqlx::Error> {
    let provider: i64 = sqlx::query_scalar("SELECT anilist_id FROM series WHERE id=?")
        .bind(series_id)
        .fetch_one(&mut *db)
        .await?;
    sqlx::query("DELETE FROM library_franchise_members WHERE provider_id=?")
        .bind(provider)
        .execute(&mut *db)
        .await?;
    if let Some((root, _)) = path.split_once('/') {
        record(
            db,
            provider,
            &Assignment {
                path: path.into(),
                root: root.into(),
                owner: provider,
            },
        )
        .await?;
    }
    sqlx::query("UPDATE library_franchises SET owner_provider_id=(SELECT MIN(provider_id) FROM library_franchise_members WHERE folder=library_franchises.folder) WHERE NOT EXISTS(SELECT 1 FROM library_franchise_members WHERE provider_id=owner_provider_id AND folder=library_franchises.folder) AND EXISTS(SELECT 1 FROM library_franchise_members WHERE folder=library_franchises.folder)")
        .execute(&mut *db).await?;
    sqlx::query("DELETE FROM library_franchises WHERE folder NOT IN (SELECT folder FROM library_franchise_members)").execute(&mut *db).await?;
    Ok(())
}

pub async fn promote(db: &mut SqliteConnection, old: i64, new: i64) -> Result<(), sqlx::Error> {
    let root: Option<String> =
        sqlx::query_scalar("SELECT folder FROM library_franchise_members WHERE provider_id=?")
            .bind(old)
            .fetch_optional(&mut *db)
            .await?;
    if let Some(root) = root {
        record(
            db,
            new,
            &Assignment {
                path: String::new(),
                root,
                owner: new,
            },
        )
        .await?;
        sqlx::query("DELETE FROM library_franchise_members WHERE provider_id=?")
            .bind(old)
            .execute(&mut *db)
            .await?;
        sqlx::query("UPDATE library_franchises SET owner_provider_id=? WHERE owner_provider_id=?")
            .bind(new)
            .bind(old)
            .execute(db)
            .await?;
    }
    Ok(())
}

/// Explicit adoption never changes a stored path or an acquisition selection.
pub async fn enable(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT id,anilist_id,folder_name FROM series WHERE instr(folder_name,'/')>0 ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (id, _, path) in rows {
        record_move(&mut tx, id, &path).await?;
    }
    sqlx::query("UPDATE library_franchises SET owner_provider_id=COALESCE((SELECT anilist_id FROM release_follows WHERE library_folder=library_franchises.folder LIMIT 1),owner_provider_id)")
        .execute(&mut *tx).await?;
    sqlx::query("UPDATE library_path_policy SET enabled=1 WHERE id=1")
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE config SET series_folder_format=? WHERE series_folder_format='{series.title} ({series.year})'")
        .bind(naming::DEFAULT_SERIES_FOLDER_FORMAT).execute(&mut *tx).await?;
    Catalog::load(&mut tx).await?;
    tx.commit().await
}

pub async fn catalog(db: &SqlitePool) -> Result<Catalog, sqlx::Error> {
    let mut connection = db.acquire().await?;
    Catalog::load(&mut connection).await
}

pub async fn project(
    db: &SqlitePool,
    id: i64,
    leaf: String,
    names: naming::SeriesNames<'_>,
) -> Result<String, sqlx::Error> {
    let mut catalog = catalog(db).await?;
    if catalog.enabled {
        let year = names.year;
        catalog.seed(id, names);
        Ok(catalog.plan(id, leaf, year, None)?.path)
    } else {
        Ok(leaf)
    }
}

/// Shared by repair paths that need to assign a previously unusable folder.
pub async fn generate(db: &SqlitePool, row: &series::Series) -> Result<String, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let prefs = config::get_naming_prefs(&mut *tx).await;
    let leaf = naming::series_folder(
        &prefs.series_folder_format,
        &prefs.title_language,
        &naming::SeriesNames::from_series(row),
    );
    let path = if enabled(&mut *tx).await? {
        let assignment = Catalog::load(&mut tx).await?.plan(
            row.anilist_id,
            leaf,
            row.season_year,
            Some(row.anilist_id),
        )?;
        record(&mut tx, row.anilist_id, &assignment).await?;
        assignment.path
    } else {
        series::unique_series_folder(&mut *tx, leaf, row.season_year, Some(row.id)).await?
    };
    sqlx::query("UPDATE series SET folder_name=? WHERE id=?")
        .bind(&path)
        .bind(row.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(path)
}

#[cfg(test)]
#[path = "library_layout_tests.rs"]
mod tests;
