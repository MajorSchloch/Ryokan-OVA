# Franchise folders and Jellyfin paths

The library uses a general franchise model. A franchise can contain one release or many releases. Its selected releases retain their AniList IDs, episode numbers, monitoring settings, and individual metadata.

The physical layout is `<media root>/<franchise>/<release>/...`. Existing flat release folders remain supported. Folder names do not define watch order. Release order and a future recommended watch order belong to franchise metadata.

## Default paths and existing libraries

Fresh installations assign a franchise parent to every new release. A one-off uses `Title/Title`. Names contain no generated date, format, or provider ID. A collision can add a year or numeric suffix. Custom release-folder templates still apply to the release name.

Storage membership is separate from download follows and selections. A folder move does not create a follow or start a download. Stored membership keeps the library group intact when relation caches expire.

The path planner uses provider IDs, cached anime relations, and explicit storage choices. It can reuse a known franchise through an untracked intermediate release. Main releases take priority over promotional specials, music, and identified side stories when a new parent needs a name. It does not compare similar title text. An ambiguous link between two saved franchise folders requires review. A later metadata refresh never renames a stored path.

Existing installations retain their path policy on upgrade. After a preview and backup, stop Ryokan and enable the new default with the existing data and media mounts:

```text
/app/ryokan --enable-franchise-folders
```

This command records existing nested paths and changes future assignments. It moves no files and changes no download selections. It replaces the old default `{series.title} ({series.year})` template with `{series.title}`. It preserves custom templates. The command is safe to repeat.

Add Series, automatic tracking, manual-import previews, and external request lookups use the same planner. A new folder assignment must use `Franchise/Release` after adoption. Existing flat paths remain valid until an explicit migration.

The eventual Jellyfin view should show one franchise entry with its releases and watch order inside. This change supplies storage and compatibility support for that view. It does not implement the franchise presentation layer.

## Browse releases before adding them

Release detail pages use provider identities: `/anime/anilist/130003` for an AniList release, or `/anime/mal/5680` for a MAL release. The same URL works before and after the release enters the library. Browsing can cache metadata, but does not create a library entry, follow, media folder, or download.

Search result covers and titles open these Ryokan pages. Provider links remain available separately. Relation cards, library groups, and franchise previews also link to release details. A tracked release shows its library controls; an untracked release offers **Add to Library** and **Explore franchise**.

Legacy `/series/<library row id>` bookmarks redirect to the provider page. The existing `?by=anilist` links still identify provider IDs explicitly. Mutation and playback APIs retain their library row IDs. An AniList ID that equals another release's library row ID cannot select that release.

A franchise remains a Ryokan group of releases. Its follow owner is a release, not a provider identity for the whole franchise. Browsing a release or its franchise preview does not save a follow or move files.

## Jellyfin compatibility view

Jellyfin's TV library can interpret a franchise folder as a series and its release folders as seasons. A private Jellyfin 12.4.0 rehearsal confirmed that this loses the releases' separate provider identities.

The optional compatibility view gives Jellyfin its original flat paths through managed directory links. Ryokan retains the physical franchise paths. The view copies no media. A persisted alias keeps a release's Jellyfin path stable after its physical folder moves.

Set both variables in Ryokan:

```text
RYOKAN_JELLYFIN_VIEW_ROOT=/data/anime-jellyfin
RYOKAN_JELLYFIN_VIEW_SOURCE=/data/ryokan-anime
```

The first path must be writable inside Ryokan. The second names the real-media mount inside Jellyfin. It can differ from the real-media mount inside Ryokan.

For a media root at `/srv/media/anime` on the host, the required mounts are:

| Consumer | Host directory | Container path |
| --- | --- | --- |
| Ryokan | `/srv/media/anime` | `/data/anime` |
| Ryokan | `/srv/media/anime-jellyfin` | `/data/anime-jellyfin` |
| Jellyfin | `/srv/media/anime-jellyfin` | `/data/anime` |
| Jellyfin | `/srv/media/anime` | `/data/ryokan-anime` |

Keep Jellyfin's configured library path at `/data/anime`. Keep Tdarr on the real media root. Preserve each consumer's existing access permissions.

Ryokan builds the view at startup, before and after automatic imports, after manual imports, and during file removal. Periodic post-processing also reconciles the view. It retains untracked files and releases removed from Ryokan with their files kept. It removes links whose media folders disappear.

The view refuses foreign files, changed links, overlapping owners, and another root's state file. Its pending state supports interrupted link updates. Keep `.ryokan-library-view.json` and the `series_library_aliases` database table with the recovery material.

## Review and move a release

The commands below use placeholders. Replace them with IDs and paths from the reviewed franchise preview.

```text
/app/ryokan --preview-library-folder SERIES_ID 'Franchise/Release'
/app/ryokan --move-library-folder SERIES_ID 'Franchise/Release'
```

The preview is read-only. The move requires Ryokan to be stopped because it takes the application's library lease. Stop other writers and consumer scans for the cutover. Keep torrents seeding. Preserve any existing Tdarr pause.

Back up both applications' metadata and Compose files before the cutover. Record every old and new path. Initialize and verify the compatibility view before changing Jellyfin's mount. Check current file observations again before applying the reviewed moves.

The move renames the whole release directory on the same filesystem. It preserves media, subtitles, artwork, NFO bytes, and directory identity. Linked paths, mount changes, occupied destinations, and overlapping owners require review. Resolve affected recycle-bin entries first.

An interrupted filesystem move leaves a durable recovery marker beside the database. Normal startup refuses that marker. Repeat the same command to finish the move and database update. The marker binds recovery to the original directory inode. The same completed command makes no further change.

One-off titles can move below their own name, such as `Title` to `Title/Title`. The journal protects the intermediate staging step. The reverse command can restore the original flat layout. It refuses to remove a franchise parent that contains other files.

A tracked release without a current directory receives only its future import path. No empty release directory is created.

For a saved download follow, after every selected release uses its destination, save that follow's future path:

```text
/app/ryokan --set-franchise-library-folder OWNER_ANILIST_ID 'Franchise'
```

New selected releases then receive paths below that franchise. Existing release paths stay fixed. Selected releases must have a verified AniList identity. Saving the same franchise path again is a no-op. A storage-only migration does not need a download follow. The move command records its franchise membership directly.

## Cutover checks and recovery

Start Ryokan first and verify its compatibility view. Then start Jellyfin with the reviewed mounts. Verify the existing item IDs, AniList IDs, artwork search, playback, and watch history for every profile. Check WatchGuard references and Tdarr's cached paths before ending maintenance.

The private compatibility rehearsal kept two releases, two seasons, and three episodes separate. All seven Jellyfin item IDs survived the simulated physical move and a server restart. A custom overview and premiere date survived too.

To reverse a move, stop the writers and use the same move command with the original path. Move all selected releases back before clearing a franchise's saved folder with an empty string. Keep the new binary available for recovery. Restore flat paths before returning to an older Ryokan binary that cannot read nested paths.

## Verification

```sh
cargo nextest run --features test-support --lib -E 'test(library_layout) or test(library_move) or test(jellyfin_view) or test(library_paths_tests) or test(franchise_preview)'
```

Coverage includes legacy paths, nested scans, subsequent imports, sibling-safe deletion, future follows, stable aliases, kept files, conflict refusal, interrupted moves, one-off staging, repeat runs, and reverse moves.
