# Review an existing library

The `ova` branch uses the AniList entry and episode number to identify a library episode. A release can use episode zero for a prologue that has library number one. Ryokan can apply this change when it has a verified mapping. An episode count alone cannot identify the prologue.

New series have upgrades off by default. You can change the default under **Settings → Quality**. Existing series retain their own setting. With upgrades off, a batch can fill missing episodes and keep files that already occupy other slots.

## Collect and review

Select the internal Ryokan series IDs. The following example reads series 1 and 2. The collector reads the database, file names, file properties, and NFO text. Store its output privately.

```sh
umask 077
python3 scripts/collect_migration_snapshot.py \
  --db /config/ryokan.db --root /data/anime --series 1 2 > snapshot.json
```

Prepare a versioned manifest for the snapshot. The Rust type `services::migration_preview::Manifest` defines its fields. Bind the manifest to the SHA-256 of the exact snapshot. Review source ranges, special episodes, episode titles, and file ownership. To allow replay of a previous download, supply the exact source and library paths for each imported file.

```sh
cargo run --locked --example migration_preview -- snapshot.json manifest.json > preview.json
```

Review the proposed files, records, and blockers. Missing evidence or a conflict requires review. A source number, title, or file count alone cannot select a file for replay.

## Rehearse on a copy

Use a new workspace and an unused path for the test database.

```sh
cargo build --locked --examples
./target/debug/examples/migration_rehearsal_db /tmp/ryokan-empty-test.sqlite
python3 scripts/rehearse_migration_snapshot.py \
  --snapshot snapshot.json --manifest manifest.json \
  --schema /tmp/ryokan-empty-test.sqlite \
  --planner "$PWD/target/debug/examples/migration_preview" \
  --workspace /tmp/ryokan-test-library
```

The rehearsal builds a test library with placeholder media. It applies the plan and tests rollback. The journal and staged media must support hard links on the same file system. Later changes to the database or files can prevent rollback from that journal.

Keep an independent copy of the original configuration and media before a live migration. The rehearsal does not provide a production cutover command. Check metadata refresh, playback, and file conversion with real media before you retire the original copy.

## Clean up underscores in series folders

New library names use spaces for underscores. To review existing series folders, run the next-release binary with the same database and media mounts as Ryokan.

```sh
/app/ryokan --preview-folder-cleanup
```

The preview lists the Ryokan series ID and both paths. When an old folder matches a stored title, Ryokan can recover the original punctuation. For example, `Kizumonogatari Part 1_ Tekketsu (2016)` becomes `Kizumonogatari Part 1 - Tekketsu (2016)`. A custom name with underscores uses spaces.

Before applying a rename, check the media mount and any Jellyfin, WatchGuard or Tdarr references to the old path. Stop Ryokan and other writers to that folder. Run the command with the internal series ID from the preview. In this example the ID is 155.

```sh
/app/ryokan --apply-folder-cleanup 155
```

The command renames the series directory and updates its stored path. Files and subdirectories travel with it. If Ryokan has tracked the release but the folder does not exist yet, the command updates only the stored path. A conflicting destination or a source shared by two series blocks the operation. If the process stops during the rename, run the same command again before starting Ryokan.
