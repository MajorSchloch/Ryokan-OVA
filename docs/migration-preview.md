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
