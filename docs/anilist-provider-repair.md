# Repair AniList provider IDs

Ryokan exports the canonical provider name `AniList` in series and season NFO files. Existing files and Jellyfin entries can still contain `anilist`. Jellyfin's AniList plugin identifies its provider as `AniList`. [Plugin source](https://github.com/jellyfin/jellyfin-plugin-anilist/blob/master/Jellyfin.Plugin.AniList/Constants.cs)

The two NFO writers in `src/services/nfo.rs` serve imports, metadata refreshes, and sidecar regeneration. Episode NFOs do not export an AniList provider ID. Internal source labels and account-provider names use their existing contracts.

## Preview

Deploy the export fix before a bulk repair so subsequent sidecar writes use the canonical key.

Run the tool on the machine that can access the media and Jellyfin API. Set `JELLYFIN_API_KEY` in that process's environment. The plan contains paths, titles, hashes, and provider IDs. It contains no API token.

```sh
python3 scripts/repair_anilist_provider_ids.py \
  --root /root/data/anime \
  --jellyfin-url http://127.0.0.1:8096 \
  --jellyfin-root /data/anime \
  --output anilist-preview.json
```

The preview lists NFO attribute changes and Jellyfin provider-map changes independently. It accepts both spellings when reading IDs. Conflicting IDs, unsupported XML, linked paths, or mismatches between NFO and Jellyfin IDs require review.

## Repair one title

Use the exact direct folder name from the preview. Keep backups outside the media root.

```sh
python3 scripts/repair_anilist_provider_ids.py \
  --apply --plan anilist-preview.json \
  --folder '86 EIGHTY-SIX (2021)' \
  --backup-dir /root/docker/servarr/anilist-repair-backup \
  --jellyfin-url http://127.0.0.1:8096
```

The NFO repair replaces only the provider attribute value. It preserves the other bytes, file permissions, and ownership. Each replacement is atomic. Original NFOs and complete Jellyfin metadata payloads go into the backup directory.

The Jellyfin repair reads the complete item through `GET /Users/{adminUserId}/Items/{itemId}`, normalizes its `ProviderIds`, then posts that payload to `POST /Items/{itemId}`. It retains the other provider IDs and verifies the editable metadata after the update. This supported API operation updates the existing item and its persisted provider map. A whole-library scan is unnecessary. [Item update implementation](https://github.com/jellyfin/jellyfin/blob/master/Jellyfin.Api/Controllers/ItemUpdateController.cs)

A file edit alone can leave Jellyfin's stored lowercase key in place. Jellyfin loads the provider named by an NFO's `uniqueid` type attribute, so both the file and the existing item need attention. [NFO documentation](https://jellyfin.org/docs/general/server/metadata/nfo/), [NFO parser](https://github.com/jellyfin/jellyfin/blob/master/MediaBrowser.XbmcMetadata/Parsers/BaseNfoParser.cs)

Before bulk repair, stop Ryokan's writers and save a consistent Jellyfin database backup with each profile's watch data. After repair, compare the stored metadata with that backup. Jellyfin's automatic NFO reader can apply other fields from an existing NFO when it reads the corrected file. Restore changed field values through the complete item-update payload before ending maintenance. The tool's per-request checks do not replace this whole-run comparison.

Re-running the same plan skips completed changes. File hashes and provider maps protect newer edits. If a run stops partway, retain its plan and backup directory and run it again after resolving the reported error.

Jellyfin can read the corrected NFO before the API step and retain both `anilist` and `AniList` with the same ID. The resume check accepts this casing change. It still rejects a changed AniList value or any changed ID from another provider, including an already canonical map.

## Verification

Confirm that the item API returns `ProviderIds.AniList` with the original ID and that AniList poster search returns artwork.

A targeted refresh can check persistence through a metadata pass:

```text
POST /Items/{itemId}/Refresh
  ?MetadataRefreshMode=ValidationOnly
  &ImageRefreshMode=None
  &Recursive=false
  &ReplaceAllMetadata=false
  &ReplaceAllImages=false
```

Query the item and `GET /Items/{itemId}/RemoteImages?Type=Primary&ProviderName=AniList&IncludeAllLanguages=true` again after the refresh. This checks discovery without selecting or replacing a poster. Validation can skip an unchanged NFO; it does not force a provider reload.

The vm622 pilot used Jellyfin 12.4.0 and AniList 15.0.0.0. The repair changed `86 EIGHTY-SIX` from `anilist=116589` to `AniList=116589`. AniList poster search changed from zero results to one. A second repair run made no changes.

The pilot also completed a forced `MetadataRefreshMode=FullRefresh` with the other flags above. Jellyfin's `DateLastRefreshed` advanced, and the canonical ID and artwork result survived. This refresh changed the language and country preferences; the pilot restored both from the saved item. Final verification covered the original editable metadata, 32 file observations, and watch data for all seven profiles. The repair uses the item update to retain existing field values. A forced provider refresh is unnecessary for the bulk repair.

The vm622 bulk repair corrected 180 NFO files and 180 Jellyfin entries. Verification found no remaining casing fixes and no changes on the second run. It covered 4,047 file observations and seven profiles. The metadata audit found one season premiere date changed by an automatic NFO reread; the original date was restored through the item-update API.

## Remaining titles

Review a fresh preview after the pilot. Omit `--folder` to apply the reviewed plan to all listed entries after the export fix is deployed. Use the same verification on the resulting items. Keep the recovery files until the repair is verified.

## Regression checks

```sh
python3 -m unittest discover -s scripts -p 'test_repair_anilist_provider_ids.py'
cargo nextest run --features test-support --lib -E 'test(services::nfo::)'
```

CI runs the repair tests and the Rust tests. They cover the canonical exported key, legacy readers, metadata preservation, preview conflicts, newer file edits, and repeated repairs.
