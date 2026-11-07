"""Offline migration transaction experiments. Only isolated rehearsal trees.

No production CLI is exposed. The marker is a scope guard, not a security
boundary. It shares the application lock and recovery marker, but external
writer exclusion and full live-copy acceptance still require validation.
"""
import hashlib
import fcntl
import functools
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import uuid
import xml.etree.ElementTree as ET

from collect_migration_snapshot import FIELDS, collect


def digest(data):
    return hashlib.sha256(data).hexdigest()


def database_digest(db):
    return digest("\n".join(db.iterdump()).encode())


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def save(path, value):
    tmp = path.with_suffix(".tmp")
    with tmp.open("x", encoding="utf-8") as out:
        json.dump(value, out, sort_keys=True)
        out.flush()
        os.fsync(out.fileno())
    os.replace(tmp, path)
    sync_dir(path.parent)


def safe(root, name):
    if not name or "\\" in name or "\0" in name or any(p in ("", ".", "..") for p in name.split("/")):
        raise ValueError("Unsafe migration path")
    path = root / name
    if Path(name).is_absolute():
        raise ValueError("Absolute migration path")
    for part in (path, *path.parents):
        if part == root:
            break
        if part.is_symlink():
            raise ValueError("Linked migration path")
    return path


def connect(path):
    db = sqlite3.connect(path.as_uri() + "?mode=rw", uri=True)
    db.row_factory = sqlite3.Row
    db.execute("PRAGMA foreign_keys=ON")
    return db


def scope(work):
    work = Path(work).resolve(strict=True)
    marker = json.loads((work / "rehearsal.json").read_text())
    if marker != {"purpose": "synthetic migration rehearsal", "workspace": str(work)}:
        raise ValueError("Only an isolated rehearsal workspace is supported")
    root, db = work / "media", work / "ryokan.db"
    if root.is_symlink() or db.is_symlink() or not root.is_dir() or not db.is_file() or db.stat().st_nlink != 1:
        raise ValueError("Invalid rehearsal paths")
    return work, root, db


def exclusive(function):
    @functools.wraps(function)
    def run(work, *args, **kwargs):
        _, _, db = scope(work)
        fd = os.open(str(db) + ".library.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return function(work, *args, **kwargs)
        finally:
            os.close(fd)
    return run


def marker_path(db):
    return Path(str(db) + ".library-migration.json")


def marker_value(journal):
    return {"version": 1, "journal": str(journal.resolve(strict=True))}


def publish_marker(db, journal):
    marker = marker_path(db)
    value = marker_value(journal)
    if marker.is_symlink():
        raise ValueError("Linked migration marker")
    if marker.exists():
        if json.loads(marker.read_text()) != value:
            raise ValueError("Recovery marker belongs to another migration")
        return
    temp = marker.with_name(marker.name + ".tmp-" + uuid.uuid4().hex)
    try:
        with temp.open("x") as out:
            json.dump(value, out)
            out.flush()
            os.fsync(out.fileno())
        os.link(temp, marker)  # Publish complete contents without overwriting.
        sync_dir(marker.parent)
    finally:
        temp.unlink(missing_ok=True)
        sync_dir(marker.parent)


def clear_marker(db, journal):
    marker = marker_path(db)
    if marker.is_symlink() or json.loads(marker.read_text()) != marker_value(journal):
        raise ValueError("Recovery marker changed")
    marker.unlink()
    sync_dir(marker.parent)


def fingerprint(path):
    st = path.lstat()
    if not stat.S_ISREG(st.st_mode):
        raise ValueError("Migration needs regular files")
    result = {"device": st.st_dev, "inode": st.st_ino, "size": st.st_size,
              "mtime_ns": st.st_mtime_ns}
    if path.suffix.lower() == ".nfo":
        result["sha256"] = digest(path.read_bytes())
    return result


def nfo_bytes(original, fields):
    # Preserve unknown elements, attributes, comments and processing instructions.
    # Refuse DTDs/entities. The parser does not fetch external documents.
    if b"<!DOCTYPE" in original.upper() or b"<!ENTITY" in original.upper():
        raise ValueError("Unsupported NFO declaration")
    parser = ET.XMLParser(target=ET.TreeBuilder(insert_comments=True, insert_pis=True))
    body = re.sub(r'^\s*<\?xml\s[^?]*\?>', '', original.decode("utf-8-sig"), count=1)
    wrapper = ET.fromstring("<migration>" + body + "</migration>", parser=parser)
    episodes = [e for e in wrapper if e.tag == "episodedetails"]
    if len(episodes) != len(fields) or any(e.tag not in ("episodedetails", ET.Comment, ET.ProcessingInstruction) for e in wrapper):
        raise ValueError("NFO episode count or root does not match the plan")
    for episode, desired in zip(episodes, fields):
        if set(desired) != {"season", "episode", "showtitle", "title", "aired"}:
            raise ValueError("Unsupported NFO field set")
        for name, value in desired.items():
            existing = episode.findall(name)
            if len(existing) > 1:
                raise ValueError("Duplicate NFO identity field")
            node = existing[0] if existing else ET.SubElement(episode, name)
            node.text = str(value)
    return b"\n".join(ET.tostring(e, encoding="utf-8") for e in wrapper) + b"\n"


KEYS = {
    "series": ("id",), "grabbed_torrents": ("id",),
    "episode_quality_tags": ("series_id", "episode_number"),
    "episode_monitor_state": ("series_id", "episode_number"),
    "episode_grab_history": ("id",),
    "series_episode_metadata": ("series_id", "episode_number"),
    "provider_episode_metadata": ("provider_id", "episode_number"),
}


def records(db, changes):
    prepared, seen = [], set()
    for item in changes:
        table, key, before, after, action = (item[n] for n in ("table", "key", "before", "after", "action"))
        if table not in KEYS or not key or action not in ("update", "insert", "delete_cache_row", "delete_monitor_row"):
            raise ValueError("Unsupported record operation")
        allowed = set(FIELDS[table].split())
        if not set(key) <= allowed or (after is not None and not set(after) <= allowed):
            raise ValueError("Unsupported record column")
        # An insert may fill the OLD key of a row that moves elsewhere.
        token = (table, "new" if action == "insert" else "old", json.dumps(key, sort_keys=True))
        if token in seen:
            raise ValueError("Duplicate record proposal")
        seen.add(token)
        if action == "insert":
            if before is not None or not isinstance(after, dict):
                raise ValueError("Invalid insert")
            prepared.append((table, None, after))
            continue
        if set(key) != set(KEYS[table]) or not isinstance(before, dict):
            raise ValueError("Invalid record key")
        where = " AND ".join(f'"{n}" = ?' for n in key)
        rows = db.execute(f'SELECT * FROM "{table}" WHERE {where}', tuple(key.values())).fetchall()
        if len(rows) != 1 or any(dict(rows[0]).get(k) != v for k, v in before.items()):
            raise ValueError("Database row changed after preview")
        old = dict(rows[0])
        if action.startswith("delete_"):
            expected = {"delete_cache_row": {"series_episode_metadata", "provider_episode_metadata"},
                        "delete_monitor_row": {"episode_monitor_state"}}[action]
            if table not in expected or after is not None:
                raise ValueError("Unsupported deletion")
            new = None
        else:
            new = {**old, **after}
        prepared.append((table, old, new))
    # Remove only leaf rows whose keys move (or cache/monitor deletions).
    # This avoids +1 shift collisions while preserving unexported columns.
    for table, old, new in prepared:
        if old is not None and (new is None or any(old[k] != new[k] for k in KEYS[table])):
            if table in ("series", "grabbed_torrents", "episode_grab_history"):
                raise ValueError("Cannot change durable record IDs")
            where = " AND ".join(f'"{k}" = ?' for k in KEYS[table])
            db.execute(f'DELETE FROM "{table}" WHERE {where}', tuple(old[k] for k in KEYS[table]))
    for table, old, new in prepared:
        if new is None:
            continue
        if old is None or any(old[k] != new[k] for k in KEYS[table]):
            cols = ",".join(f'"{c}"' for c in new)
            marks = ",".join("?" for _ in new)
            db.execute(f'INSERT INTO "{table}" ({cols}) VALUES ({marks})', tuple(new.values()))
        else:
            setters = ",".join(f'"{c}" = ?' for c in new)
            where = " AND ".join(f'"{k}" = ?' for k in KEYS[table])
            db.execute(f'UPDATE "{table}" SET {setters} WHERE {where}',
                       (*new.values(), *(old[k] for k in KEYS[table])))
    if db.execute("PRAGMA foreign_key_check").fetchone():
        raise ValueError("Migration broke a foreign key")


@exclusive
def apply(work, snapshot_bytes, preview, event=lambda name: None):
    work, root, db_path = scope(work)
    if os.path.lexists(marker_path(db_path)):
        raise ValueError("Recover the unfinished migration first")
    snapshot = json.loads(snapshot_bytes)
    if preview.get("preview_only") is not True or preview.get("blockers") or preview["snapshot_sha256"] != digest(snapshot_bytes):
        raise ValueError("Preview is blocked or does not match the snapshot")
    fresh = collect(db_path, root, [r["id"] for r in snapshot["tables"]["series"]])
    fresh["collected_at"] = snapshot["collected_at"]
    if fresh != snapshot:
        raise ValueError("Snapshot is stale")
    files = preview["files"]
    observed = {f["path"]: f for f in snapshot["files"] if f["kind"] == "file"}
    if len(files) != len(observed) or {f["from"] for f in files} != set(observed):
        raise ValueError("Plan must cover every observed file once")
    destinations = set()
    directories = set()
    for f in files:
        source, target = safe(root, f["from"]), safe(root, f["to"])
        expected_observation = {"text": None, "sha256": None, **observed[f["from"]]}
        if f["observed"] != expected_observation or f["to"].casefold() in destinations:
            raise ValueError("Changed observation or duplicate destination")
        destinations.add(f["to"].casefold())
        if target.exists() and f["to"] not in observed:
            raise ValueError("Occupied migration destination")
        for parent in target.parents:
            if parent == root:
                break
            if parent.exists() and not parent.is_dir():
                raise ValueError("Destination parent is not a directory")
            if not parent.exists():
                directories.add(parent.relative_to(root).as_posix())
    journal = work / "journal"
    journal.mkdir(mode=0o700)
    (journal / "originals").mkdir()
    (journal / "output").mkdir()
    db = connect(db_path)
    try:
        with sqlite3.connect(journal / "before.sqlite") as backup:
            db.backup(backup)
        with (journal / "before.sqlite").open("rb") as fd:
            os.fsync(fd.fileno())
        db.execute("BEGIN IMMEDIATE")
        with connect(journal / "before.sqlite") as backup:
            before_hash = database_digest(backup)
        if database_digest(db) != before_hash:
            raise ValueError("Database changed during backup")
        state = {"version": 1, "status": "prepared", "before_db": before_hash,
                 "directories": sorted(directories, key=lambda x: (x.count("/"), x)),
                 "old_directories": [f for f in snapshot["files"] if f["kind"] == "directory"],
                 "files": []}
        for i, f in enumerate(files):
            source = safe(root, f["from"])
            original = journal / "originals" / str(i)
            os.link(source, original)
            original_fp = fingerprint(source)
            payload = original
            if f["kind"] == "episode_nfo":
                payload = journal / "output" / str(i)
                with payload.open("xb") as out:
                    out.write(nfo_bytes(source.read_bytes(), f["nfo_fields"]))
                    out.flush()
                    os.fsync(out.fileno())
                os.chmod(payload, stat.S_IMODE(source.stat().st_mode))
            output_fp = fingerprint(payload)
            # Numeric journal paths have no suffix; retain NFO byte checks.
            if source.suffix.lower() == ".nfo":
                output_fp["sha256"] = digest(payload.read_bytes())
            state["files"].append({"from": f["from"], "to": f["to"], "original": str(i),
                                   "output": str(payload.relative_to(journal)),
                                   "before": original_fp, "after": output_fp})
        sync_dir(journal / "originals")
        sync_dir(journal / "output")
        records(db, preview["records"])
        state["after_db"] = database_digest(db)
        save(journal / "state.json", state)
        publish_marker(db_path, journal)
        event("prepared")
        for name in state["directories"]:
            path = safe(root, name)
            path.mkdir()
            sync_dir(path.parent)
        event("directories")
        for i, f in enumerate(state["files"]):
            path = safe(root, f["from"])
            if fingerprint(path) != f["before"]:
                raise ValueError("File changed before removal")
            path.unlink()
            sync_dir(path.parent)
            event(f"removed:{i}")
        for i, f in enumerate(state["files"]):
            path = safe(root, f["to"])
            os.link(journal / f["output"], path)  # Fails if any path is occupied.
            sync_dir(path.parent)
            event(f"placed:{i}")
        event("before_commit")
        db.commit()
        event("committed")
        state["status"] = "complete"
        save(journal / "state.json", state)
        clear_marker(db_path, journal)
        event("complete")
    finally:
        db.close()


def matches(path, expected):
    actual = fingerprint(path)
    if "sha256" in expected:
        actual["sha256"] = digest(path.read_bytes())
    return actual == expected


@exclusive
def rollback(work, event=lambda name: None):
    work, root, db_path = scope(work)
    journal = work / "journal"
    # A process can stop after writing a temporary state file but before rename.
    # The last published state remains authoritative.
    (journal / "state.tmp").unlink(missing_ok=True)
    state = json.loads((journal / "state.json").read_text())
    if state["version"] != 1:
        raise ValueError("Unknown journal version")
    db = connect(db_path)
    try:
        if database_digest(db) not in (state["before_db"], state["after_db"]):
            raise ValueError("Database changed after migration; rollback stopped")
        with connect(journal / "before.sqlite") as backup:
            if database_digest(backup) != state["before_db"]:
                raise ValueError("Database backup changed")
        expected = {}
        for f in state["files"]:
            original = journal / "originals" / f["original"]
            if not matches(original, f["before"]):
                raise ValueError("Original backup changed")
            expected.setdefault(f["from"], []).append(f["before"])
            expected.setdefault(f["to"], []).append(f["after"])
        known_directories = set(state["directories"]) | {d["path"] for d in state["old_directories"]}
        for folder in {name.split("/")[0] for name in expected}:
            base = safe(root, folder)
            if base.exists():
                for path in base.rglob("*"):
                    name = path.relative_to(root).as_posix()
                    if path.is_symlink() or (path.is_dir() and name not in known_directories) or (not path.is_dir() and name not in expected):
                        raise ValueError("Untracked path appeared after migration; rollback stopped")
        # Validate the complete rollback before removing a single current file.
        for name, choices in expected.items():
            path = safe(root, name)
            if path.exists() and not any(matches(path, fp) for fp in choices):
                raise ValueError("Library file changed after migration; rollback stopped")
        publish_marker(db_path, journal)
        for name in expected:
            path = safe(root, name)
            if path.exists():
                path.unlink()
                sync_dir(path.parent)
                event("rollback_removed")
        for f in state["files"]:
            path = safe(root, f["from"])
            os.link(journal / "originals" / f["original"], path)
            sync_dir(path.parent)
            event("rollback_restored")
        with connect(journal / "before.sqlite") as backup:
            backup.backup(db)
        event("rollback_database")
        for name in reversed(state["directories"]):
            path = safe(root, name)
            if path.exists():
                path.rmdir()
                sync_dir(path.parent)
        for old in sorted(state["old_directories"], key=lambda d: -d["path"].count("/")):
            path = safe(root, old["path"])
            os.utime(path, ns=(path.stat().st_atime_ns, old["mtime_ns"]))
        state["status"] = "rolled_back"
        save(journal / "state.json", state)
        clear_marker(db_path, journal)
    finally:
        db.close()
