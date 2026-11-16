# Retained source cleanup

Removing a rejected torrent job does not remove its downloaded files. The
one-time administrative tool `scripts/source_cleanup.py` can move eligible
retained files into recoverable quarantine. It does not free disk space.
Quarantine has no automatic expiry.

This tool is intentionally bounded. It handles completed, still-blocked source
retirements with one exact owner and a saved native file manifest. It reads all
configured qBittorrent categories and NZBGet queue and history entries, including
hidden history. Unsupported or unavailable clients prevent a complete inventory
and stop the operation. Active, shared, changed or ambiguous files stay in place.

Use a cold database backup and the reviewed deployment procedure. Before apply
or restore, hold acquisition and post-processing through Ryokan's settings,
disable client routing, finish open file selections, and stop Ryokan. The tool
also requires Ryokan's existing library lease. Do not change the database with
SQL to bypass these guards.

Use absolute, canonical host paths. Map `/data` through `--data-root` and
`/config` through the database directory. The quarantine directory is fixed to
`source-quarantine` beside the database. Store each receipt under
`source-cleanup` beside the database. Example:

```sh
python3 scripts/source_cleanup.py preview \
  --db /srv/ryokan/ryokan.db \
  --data-root /srv/data \
  --source-root /srv/data/downloads/qbittorrent/completed \
  --compat-root /srv/data/anime-jellyfin \
  --plan /srv/ryokan/source-cleanup/retained-batch.json
```

Review the receipt. Run the same command with `apply` instead of `preview` after
the service is stopped and its settings are held. Keep the receipt and tool
with the backup. Run the host operation under a supervised terminal or one-shot
service so a browser or SSH disconnect does not interrupt it.

Only exact regular files move. Each move uses an atomic no-overwrite rename on
one filesystem. There is no cross-filesystem copy/delete fallback. The tool
records the file identity and saves each state before and after movement. A
second apply reconciles an interrupted move. It does not sweep directories.

To restore, use the same paths and receipt with the `restore` command while
Ryokan is held and stopped. Restore refuses existing targets and changed files.
It keeps source blocks and native client state unchanged. A restored file does
not become accepted for seeding.

The Downloads page displays recorded cleanup outcomes. These receipts are
administrative records, not live file verification or permission to delete.
Library files, recovery entries, import history, proofs, language settings and
native file selections are outside this tool's mutation scope.

Validation:

```sh
python3 -m unittest discover -s scripts/tests -p 'test_source_cleanup.py'
```
