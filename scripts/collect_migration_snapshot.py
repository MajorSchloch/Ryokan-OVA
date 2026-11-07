#!/usr/bin/env python3
"""Read a limited Ryokan snapshot for migration review. Never update the database.

Only named, non-credential columns are exported. Media bytes are not read.
This is an observation, not a backup or permission to apply a migration.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sqlite3
import stat
from datetime import datetime, timezone


FIELDS = {
    "config": "id media_root title_language series_folder_format season_folder_format episode_file_format",
    "series": "id anilist_id mal_id title title_romaji title_english title_native episodes season_year folder_name monitor_mode allow_upgrades cumulative_prior_episodes cumulative_offset_known cumulative_offset_reviewed",
    "episode_monitor_state": "series_id episode_number monitored updated_at",
    "episode_quality_tags": "series_id episode_number quality_tag release_title release_group state updated_at source resolution is_remux is_bdmv web_kind manual_override needs_review classification_confidence",
    "episode_grab_history": "id series_id episode_number quality_tag release_title release_group file_name size_bytes is_batch state grabbed_at",
    "series_episode_metadata": "series_id episode_number title title_romaji title_english title_native aired source cached_at",
    "provider_episode_metadata": "provider_id episode_number title title_romaji title_english title_native aired source cached_at",
    "episode_cache": "mal_id episode_number title aired cached_at",
    "episode_airings": "series_id episode airing_at duration_minutes refreshed_at",
    "grabbed_torrents": "id series_id torrent_name episode_numbers state imported_at import_mode imported_source_paths imported_episode_coverage release_mapping replacement_episode client_removed_at library_migration_bridge",
    "grabbed_torrent_series": "grab_id series_id file_indices episode_numbers episode_offset matched_subtitle",
    "grabbed_torrent_numbering": "grab_id snapshot",
    "pending_grabs": "preview_id series_id created_at heartbeat_at",
}


def collect(db_path, root, series_ids):
    root = root.resolve(strict=True)
    db_path = db_path.resolve(strict=True)
    db = sqlite3.connect(db_path.as_uri() + "?mode=ro", uri=True)
    db.row_factory = sqlite3.Row
    db.execute("PRAGMA query_only=ON")
    version_before = db.execute("PRAGMA data_version").fetchone()[0]
    db.execute("BEGIN")
    tables, columns, issues = {}, {}, []
    existing = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    for table, names in FIELDS.items():
        if table not in existing:
            tables[table], columns[table] = [], []
            continue
        present = {r[1] for r in db.execute(f'PRAGMA table_info("{table}")')}
        selected = [n for n in names.split() if n in present]
        columns[table] = selected
        # Table/column identifiers come exclusively from the constants above.
        tables[table] = [dict(r) for r in db.execute(
            'SELECT ' + ','.join('"' + n + '"' for n in selected) + f' FROM "{table}"'
        )]
    entries = [r for r in tables["series"] if r["id"] in series_ids]
    if {r["id"] for r in entries} != set(series_ids):
        raise ValueError("One or more requested series do not exist")
    providers = {r["anilist_id"] for r in entries}
    mals = {r["mal_id"] for r in entries if r.get("mal_id")}
    routes = tables["grabbed_torrent_series"]
    grabs = {r["id"] for r in tables["grabbed_torrents"] if r["series_id"] in series_ids}
    grabs.update(r["grab_id"] for r in routes if r["series_id"] in series_ids)
    for table, rows in tables.items():
        if table == "config":
            tables[table] = [r for r in rows if r["id"] == 1]
        elif table == "series":
            tables[table] = entries
        elif table == "grabbed_torrents":
            tables[table] = [r for r in rows if r["id"] in grabs]
        elif table in ("grabbed_torrent_series", "grabbed_torrent_numbering"):
            tables[table] = [r for r in rows if r["grab_id"] in grabs]
        elif table == "provider_episode_metadata":
            tables[table] = [r for r in rows if r["provider_id"] in providers]
        elif table == "episode_cache":
            tables[table] = [r for r in rows if r["mal_id"] in mals]
        elif table == "pending_grabs":
            # Unassigned picker jobs can acquire a series when they commit.
            tables[table] = rows
        else:
            tables[table] = [r for r in rows if r.get("series_id") in series_ids]
    files = []
    for entry in entries:
        folder = Path(entry["folder_name"])
        if len(folder.parts) != 1 or folder.name in ("", ".", "..") or "\\" in str(folder):
            raise ValueError("Unsafe or unsupported series folder")
        base = root / folder
        if base.is_symlink() or not base.is_dir():
            raise ValueError("Missing or linked series folder: " + str(folder))
        stack = [base]
        while stack:
            path = stack.pop()
            before = path.lstat()
            kind = "directory" if stat.S_ISDIR(before.st_mode) else "file" if stat.S_ISREG(before.st_mode) else "unsupported"
            row = {"path": path.relative_to(root).as_posix(), "kind": kind,
                   "size": before.st_size, "mtime_ns": before.st_mtime_ns,
                   "device": before.st_dev, "inode": before.st_ino, "links": before.st_nlink}
            if kind == "directory":
                stack.extend(sorted(path.iterdir(), reverse=True))
            elif kind == "unsupported":
                issues.append("Unsupported file or symlink: " + row["path"])
            elif path.suffix.lower() == ".nfo":
                if before.st_size > 262144:
                    issues.append("NFO exceeds review limit: " + row["path"])
                else:
                    content = path.read_bytes()
                    row["text"] = content.decode("utf-8-sig")
                    row["sha256"] = hashlib.sha256(content).hexdigest()
            after = path.lstat()
            if (before.st_size, before.st_mtime_ns, before.st_ino) != (after.st_size, after.st_mtime_ns, after.st_ino):
                issues.append("File changed during collection: " + row["path"])
            files.append(row)
            if len(files) > 100000:
                raise ValueError("Too many paths for one migration preview")
    # Check the whole observation again, including directories. A file added or
    # removed after its parent was listed must not silently disappear from review.
    for row in files:
        current = (root / row["path"]).lstat()
        if (current.st_size, current.st_mtime_ns, current.st_ino) != (row["size"], row["mtime_ns"], row["inode"]):
            issues.append("Path changed during collection: " + row["path"])
    db.rollback()
    version_after = db.execute("PRAGMA data_version").fetchone()[0]
    db.close()
    if version_before != version_after:
        issues.append("Database changed during collection; collect again")
    return {"version": 1, "collected_at": datetime.now(timezone.utc).isoformat(),
            "tables": tables, "columns": columns,
            "root_children": sorted(p.name for p in root.iterdir()),
            "files": sorted(files, key=lambda f: f["path"]), "issues": issues}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, type=Path)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--series", required=True, type=int, nargs="+")
    args = parser.parse_args()
    print(json.dumps(collect(args.db, args.root, args.series), ensure_ascii=False, sort_keys=True))


if __name__ == "__main__":
    main()
