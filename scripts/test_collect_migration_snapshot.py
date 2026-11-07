"""Safety checks for the read-only snapshot collector (standard library only)."""
import hashlib
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

from collect_migration_snapshot import collect


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "anime"
        (self.root / "Example").mkdir(parents=True)
        (self.root / "Example" / "Example - S01E01.mkv").write_bytes(b"video bytes")
        (self.root / "Example" / "Example - S01E01.nfo").write_text("<episodedetails><episode>1</episode></episodedetails>")
        self.dbpath = self.base / "ryokan.db"
        with sqlite3.connect(self.dbpath) as db:
            db.executescript("""
                CREATE TABLE config(id INTEGER, media_root TEXT, api_key TEXT);
                INSERT INTO config VALUES (1, '/data/anime', 'DO-NOT-EXPORT-SECRET');
                CREATE TABLE series(id INTEGER, anilist_id INTEGER, mal_id INTEGER, folder_name TEXT);
                INSERT INTO series VALUES (1, 101, 201, 'Example');
                INSERT INTO series VALUES (2, 102, 202, 'Unselected');
                CREATE TABLE episode_monitor_state(series_id INTEGER, episode_number INTEGER, monitored INTEGER);
                INSERT INTO episode_monitor_state VALUES (1, 1, 0);
                INSERT INTO episode_monitor_state VALUES (2, 1, 1);
                CREATE TABLE pending_grabs(preview_id TEXT, series_id INTEGER, info_hash TEXT);
                INSERT INTO pending_grabs VALUES ('unassigned', NULL, 'NOT-EXPORTED');
            """)

    def test_database_and_media_bytes_stay_unchanged_and_secrets_are_excluded(self):
        before = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in self.base.rglob("*") if p.is_file()}
        s = collect(self.dbpath, self.root, [1])
        after = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in self.base.rglob("*") if p.is_file()}
        self.assertEqual(before, after)
        self.assertEqual([], s["issues"])
        self.assertEqual([1], [r["id"] for r in s["tables"]["series"]])
        self.assertEqual(1, len(s["tables"]["episode_monitor_state"]))
        self.assertNotIn("DO-NOT-EXPORT", json.dumps(s))
        self.assertNotIn("NOT-EXPORTED", json.dumps(s))
        self.assertEqual("unassigned", s["tables"]["pending_grabs"][0]["preview_id"])

    def test_symlink_is_reported_without_reading_its_target(self):
        secret = self.base / "private.nfo"
        secret.write_text("SENSITIVE TARGET")
        (self.root / "Example" / "linked.nfo").symlink_to(secret)
        s = collect(self.dbpath, self.root, [1])
        self.assertTrue(s["issues"])
        self.assertNotIn("SENSITIVE TARGET", json.dumps(s))

    def test_missing_database_is_not_created(self):
        missing = self.base / "missing.db"
        with self.assertRaises(FileNotFoundError):
            collect(missing, self.root, [1])
        self.assertFalse(missing.exists())

    def test_path_traversal_in_series_folder_is_rejected(self):
        with sqlite3.connect(self.dbpath) as db:
            db.execute("UPDATE series SET folder_name='../outside' WHERE id=1")
        with self.assertRaisesRegex(ValueError, "Unsafe"):
            collect(self.dbpath, self.root, [1])


if __name__ == "__main__":
    unittest.main()
