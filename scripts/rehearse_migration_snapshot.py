#!/usr/bin/env python3
"""Rehearse a snapshot on a NEW fixture: real schema/NFOs, placeholder media.

This does not connect to a server or copy real videos. The schema input must be
an empty database made by the migration_rehearsal_db example.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess

from collect_migration_snapshot import FIELDS, collect
import migration_transaction as migration


def rehearse(snapshot_path, manifest_path, schema_path, planner, work):
    source = json.loads(snapshot_path.read_bytes())
    manifest = json.loads(manifest_path.read_bytes())
    if hashlib.sha256(snapshot_path.read_bytes()).hexdigest() != manifest["snapshot_sha256"]:
        raise ValueError("Source manifest does not match the source snapshot")
    if source["issues"]:
        raise ValueError("Source observation contains issues")
    work.mkdir(mode=0o700)  # Never reuse a directory or a production root.
    work = work.resolve(strict=True)
    root = work / "media"
    root.mkdir()
    (work / "rehearsal.json").write_text(json.dumps({"purpose": "synthetic migration rehearsal", "workspace": str(work)}))
    db_path = work / "ryokan.db"
    shutil.copyfile(schema_path, db_path)
    with migration.connect(db_path) as db:
        if db.execute("SELECT COUNT(*) FROM series").fetchone()[0] or db.execute("SELECT COUNT(*) FROM users").fetchone()[0]:
            raise ValueError("Schema input is not an empty test database")
        db.execute("INSERT OR IGNORE INTO config (id) VALUES (1)")
        for table, columns in FIELDS.items():
            allowed = set(columns.split())
            for row in source["tables"][table]:
                if not set(row) <= allowed:
                    raise ValueError("Unexpected input column")
                if table == "config":
                    row = {**row, "media_root": str(root)}
                    setters = ",".join(f'"{c}"=?' for c in row)
                    db.execute(f'UPDATE config SET {setters} WHERE id=1', tuple(row.values()))
                else:
                    cols = ",".join(f'"{c}"' for c in row)
                    marks = ",".join("?" for _ in row)
                    db.execute(f'INSERT INTO "{table}" ({cols}) VALUES ({marks})', tuple(row.values()))
    hardlinks = {}
    for file in sorted(source["files"], key=lambda f: (f["path"].count("/"), f["path"])):
        path = migration.safe(root, file["path"])
        if file["kind"] == "directory":
            path.mkdir(parents=True, exist_ok=True)
        elif file["kind"] == "file":
            path.parent.mkdir(parents=True, exist_ok=True)
            identity = (file["device"], file["inode"])
            if identity in hardlinks:
                os.link(hardlinks[identity], path)
            else:
                if "text" in file and file["text"] is not None:
                    path.write_text(file["text"], encoding="utf-8")
                else:
                    path.write_bytes(("SYNTHETIC PLACEHOLDER: " + file["path"]).encode())
                hardlinks[identity] = path
        else:
            raise ValueError("Unsupported observation")
    ids = [r["id"] for r in source["tables"]["series"]]
    snapshot = collect(db_path, root, ids)
    raw = json.dumps(snapshot, sort_keys=True).encode()
    manifest["snapshot_sha256"] = hashlib.sha256(raw).hexdigest()
    (work / "snapshot.json").write_bytes(raw)
    (work / "manifest.json").write_text(json.dumps(manifest))
    result = subprocess.run([str(planner), str(work / "snapshot.json"), str(work / "manifest.json")], check=True, capture_output=True)
    (work / "preview.json").write_bytes(result.stdout)
    preview = json.loads(result.stdout)
    before_files = {f["path"]: (root / f["path"]).read_bytes() for f in snapshot["files"] if f["kind"] == "file"}
    with migration.connect(db_path) as db:
        before_db = migration.database_digest(db)
    migration.apply(work, raw, preview)
    for file in preview["files"]:
        actual = migration.safe(root, file["to"])
        if not actual.is_file():
            raise AssertionError("Missing migrated file")
        if file["kind"] != "episode_nfo" and actual.read_bytes() != before_files[file["from"]]:
            raise AssertionError("Migration changed media or companion bytes")
    with migration.connect(db_path) as db:
        if db.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise AssertionError("Database integrity check failed")
        migrated = [dict(r) for r in db.execute("SELECT id,episodes,folder_name FROM series ORDER BY id")]
    migration.rollback(work)
    with migration.connect(db_path) as db:
        if migration.database_digest(db) != before_db:
            raise AssertionError("Rollback changed database contents")
    actual_files = {p.relative_to(root).as_posix(): p.read_bytes() for p in root.rglob("*") if p.is_file()}
    if before_files != actual_files:
        raise AssertionError("Rollback changed files")
    summary = {"status": "apply and rollback passed", "video_bytes": "synthetic placeholders",
               "database": "limited observation inserted into the real current schema",
               "files": len(preview["files"]), "records": len(preview["records"]), "entries_after_apply": migrated,
               "blockers": preview["blockers"], "production_changed": False}
    (work / "result.json").write_text(json.dumps(summary, indent=2))
    return summary


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("snapshot", "manifest", "schema", "planner", "workspace"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(rehearse(args.snapshot, args.manifest, args.schema, args.planner, args.workspace), indent=2))
