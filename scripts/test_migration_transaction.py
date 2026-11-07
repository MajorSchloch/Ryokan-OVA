"""Real-schema, synthetic-media transaction tests. See docs/migration-preview.md."""
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest

from collect_migration_snapshot import collect
import migration_transaction as migration


class MigrationTransactionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.schema = Path(os.environ["RYOKAN_TEST_SCHEMA"])
        cls.planner = Path(os.environ["RYOKAN_PREVIEW_BIN"])
        cls.guard = Path(os.environ["RYOKAN_GUARD_BIN"])
        if not cls.schema.is_file() or not cls.planner.is_file() or not cls.guard.is_file():
            raise RuntimeError("Build the test schema and preview executable first")

    def check_startup(self, work, clean):
        result = subprocess.run([str(self.guard), str(work / "ryokan.db"), "check"], capture_output=True)
        self.assertEqual(result.returncode == 0, clean, result.stderr.decode())
        if not clean:
            self.assertIn("unfinished migration", result.stderr.decode())

    def fixture(self, colliding_paths=False):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        work = Path(temp.name)
        root = work / "media"
        root.mkdir()
        (work / "rehearsal.json").write_text(json.dumps({"purpose": "synthetic migration rehearsal", "workspace": str(work)}))
        db_path = work / "ryokan.db"
        shutil.copyfile(self.schema, db_path)
        with sqlite3.connect(db_path) as db:
            db.execute("INSERT INTO config (id) VALUES (1)")
            db.execute("UPDATE config SET media_root=?, title_language='english', series_folder_format='{series.title} ({series.year})', season_folder_format='Season {season.number:00}', episode_file_format='{series.title} - S{season.number:00}E{episode.number:00} - {episode.title} [{quality.full}] [{group}]{ext}' WHERE id=1", (str(root),))
            db.execute("INSERT INTO series (id,anilist_id,title,title_english,episodes,season_year,folder_name) VALUES (1,101,'Example','Example',2,2020,'Old Example')")
            db.execute("CREATE TABLE private_notes (note TEXT)")
            db.execute("INSERT INTO private_notes VALUES ('not exported')")
            for ep in (1, 2):
                name = f"Example - S01E{ep:02}.mkv"
                path = root / "Old Example" / "Season 01" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(f"SYNTHETIC VIDEO {ep}".encode())
                path.with_suffix(".nfo").write_text(f'''<?xml version='1.0' encoding='utf-8'?>
<episodedetails data="keep"><title>Old</title><season>1</season><episode>{ep}</episode><custom>Keep me</custom><!-- comment --></episodedetails>''')
                db.execute("INSERT INTO episode_quality_tags (series_id,episode_number,state,source,resolution,release_group,manual_override,classification_evidence) VALUES (1,?,'completed','BluRay','1080p','Group',1,'private evidence')", (ep,))
                db.execute("INSERT INTO episode_monitor_state (series_id,episode_number,monitored) VALUES (1,?,0)", (ep,))
                db.execute("INSERT INTO episode_grab_history (series_id,episode_number,state,file_name,release_title) VALUES (1,?,'completed',?,'Pack')", (ep,name))
            db.execute("INSERT INTO grabbed_torrents (id,hash,series_id,torrent_name,episode_numbers,state,imported_source_paths) VALUES (9,'testhash',1,'Pack','[1,2]','imported','[\"original/source.mkv\"]')")
            db.execute("INSERT INTO grabbed_torrent_series (grab_id,series_id,file_indices,episode_numbers,episode_offset) VALUES (9,1,'[0,1]','[1,2]',0)")
            if colliding_paths:
                db.execute("UPDATE series SET title='Old Example',title_english='Old Example'")
                db.execute("UPDATE config SET series_folder_format='{series.title}',episode_file_format='Example - S{season.number:00}E{episode.number:00}{ext}'")
        special = root / "Old Example" / "Specials" / "Example - S00E01.mkv"
        special.parent.mkdir()
        special.write_bytes(b"SYNTHETIC PROLOGUE")
        (work / "seed").mkdir()
        os.link(root / "Old Example" / "Season 01" / "Example - S01E01.mkv", work / "seed" / "original.mkv")
        snapshot = collect(db_path, root, [1])
        snapshot_bytes = json.dumps(snapshot, sort_keys=True).encode()
        manifest = {"version": 1, "snapshot_sha256": hashlib.sha256(snapshot_bytes).hexdigest(),
                    "entries": [{"series_id": 1, "anilist_id": 101, "old_count": 2, "target_count": 3,
                                 "canonical_prior_episodes": 0, "evidence": "Synthetic reviewed scheme",
                                 "rules": [{"source_first": 1, "source_last": 2, "target_entry": 1, "target_first": 2}]}],
                    "special_files": [{"path": special.relative_to(root).as_posix(), "series_id": 1,
                                       "season": 1, "episode": 1, "title": "Prologue", "monitored": True,
                                       "evidence": "Synthetic promoted episode"}],
                    "file_evidence": [{"path": f["path"], "source": "BluRay", "resolution": "1080p",
                                       "release_group": "Group", "evidence": "Synthetic file identity", "grab_id": 9}
                                      for f in snapshot["files"] if f["path"].endswith(".mkv")],
                    "metadata": [{"series_id": 1, "episode": ep, "title": f"Canonical {ep}", "aired": "",
                                  "evidence": "Synthetic metadata"} for ep in (1, 2, 3)]}
        (work / "snapshot.json").write_bytes(snapshot_bytes)
        (work / "manifest.json").write_text(json.dumps(manifest))
        result = subprocess.run([str(self.planner), str(work / "snapshot.json"), str(work / "manifest.json")], capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        preview = json.loads(result.stdout)
        self.assertEqual(preview["blockers"], [])
        (work / "preview.json").write_bytes(result.stdout)
        return work, snapshot_bytes, preview

    def assert_restored(self, work, snapshot_bytes):
        snapshot = json.loads(snapshot_bytes)
        root = work / "media"
        actual = {p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_file()}
        self.assertEqual(actual, {f["path"] for f in snapshot["files"] if f["kind"] == "file"})
        for f in snapshot["files"]:
            if f["kind"] == "file":
                st = (root / f["path"]).stat()
                self.assertEqual((st.st_ino, st.st_size, st.st_mtime_ns), (f["inode"], f["size"], f["mtime_ns"]))
                if "sha256" in f:
                    self.assertEqual(migration.digest((root / f["path"]).read_bytes()), f["sha256"])
        with migration.connect(work / "ryokan.db") as db, migration.connect(work / "journal" / "before.sqlite") as old:
            self.assertEqual(migration.database_digest(db), migration.database_digest(old))

    def test_complete_and_rollback_preserve_unexported_data_and_hardlinks(self):
        work, snapshot, preview = self.fixture()
        migration.apply(work, snapshot, preview)
        self.check_startup(work, True)
        with migration.connect(work / "ryokan.db") as db:
            offset = db.execute("SELECT cumulative_prior_episodes, cumulative_offset_known, cumulative_offset_reviewed FROM series WHERE id=1").fetchone()
            self.assertEqual(tuple(offset), (0, 1, 1))
            tags = db.execute("SELECT episode_number,manual_override,classification_evidence FROM episode_quality_tags ORDER BY episode_number").fetchall()
            self.assertEqual([tuple(t) for t in tags], [(1,0,''),(2,1,'private evidence'),(3,1,'private evidence')])
            self.assertEqual(db.execute("SELECT COUNT(*) FROM episode_grab_history").fetchone()[0], 3)
            self.assertEqual(db.execute("SELECT quality_tag FROM episode_quality_tags WHERE episode_number=1").fetchone()[0], "BD-1080p")
            grab = db.execute("SELECT * FROM grabbed_torrents WHERE id=9").fetchone()
            self.assertEqual(grab["episode_numbers"], "[1,2]")
            self.assertEqual(json.loads(grab["imported_episode_coverage"]), {"1": [1,2,3]})
            self.assertEqual(json.loads(grab["library_migration_bridge"])["replay_policy"], "review_required")
            self.assertEqual(db.execute("SELECT episode_offset FROM grabbed_torrent_series").fetchone()[0], 0)
        for f in preview["files"]:
            path = work / "media" / f["to"]
            self.assertTrue(path.is_file())
            if f["kind"] == "episode_nfo":
                text = path.read_text()
                self.assertIn("<custom>Keep me</custom>", text)
                self.assertIn("<!-- comment -->", text)
                self.assertIn(f'<episode>{f["nfo_fields"][0]["episode"]}</episode>', text)
        self.assertEqual((work / "seed" / "original.mkv").read_bytes(), b"SYNTHETIC VIDEO 1")
        migration.rollback(work)
        self.assert_restored(work, snapshot)
        migration.rollback(work)
        self.assert_restored(work, snapshot)

    def test_process_exit_at_each_durable_boundary_recovers(self):
        work, snapshot, preview = self.fixture()
        points = ["prepared", "directories"] + [f"removed:{i}" for i in range(len(preview["files"]))] + [f"placed:{i}" for i in range(len(preview["files"]))] + ["before_commit", "committed", "complete"]
        # A fresh workspace for each real process exit. SQLite must recover its
        # uncommitted transaction without a Python exception handler helping.
        for point in points:
            with self.subTest(point=point):
                work, snapshot, preview = self.fixture()
                code = "import json,os,sys; from pathlib import Path; import migration_transaction as m; w=Path(sys.argv[1]); m.apply(w,(w/'snapshot.json').read_bytes(),json.loads((w/'preview.json').read_text()),lambda p: os._exit(79) if p == sys.argv[2] else None)"
                result = subprocess.run([sys.executable, "-c", code, str(work), point], cwd=Path(__file__).parent)
                self.assertEqual(result.returncode, 79)
                self.check_startup(work, point == "complete")
                migration.rollback(work)
                self.assert_restored(work, snapshot)
                self.check_startup(work, True)

    def test_interrupted_rollback_can_be_repeated(self):
        for point in ("rollback_removed", "rollback_restored", "rollback_database"):
            with self.subTest(point=point):
                work, snapshot, preview = self.fixture()
                migration.apply(work, snapshot, preview)
                code = "import os,sys; import migration_transaction as m; m.rollback(sys.argv[1],lambda p: os._exit(81) if p == sys.argv[2] else None)"
                result = subprocess.run([sys.executable, "-c", code, str(work), point], cwd=Path(__file__).parent)
                self.assertEqual(result.returncode, 81)
                self.check_startup(work, False)
                migration.rollback(work)
                self.assert_restored(work, snapshot)
                self.check_startup(work, True)

    def test_running_application_excludes_apply_and_rollback(self):
        work, snapshot, preview = self.fixture()
        app = subprocess.Popen([str(self.guard), str(work / "ryokan.db"), "hold"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            self.assertEqual(app.stdout.readline().strip(), "ready")
            with self.assertRaises(BlockingIOError):
                migration.apply(work, snapshot, preview)
            with self.assertRaises(BlockingIOError):
                migration.rollback(work)
            self.assertFalse((work / "journal").exists())
        finally:
            app.communicate("stop\n", timeout=10)
        self.assertEqual(app.returncode, 0)
        migration.apply(work, snapshot, preview)
        migration.rollback(work)
        self.assert_restored(work, snapshot)

    def test_same_folder_shift_never_overwrites_the_next_episode(self):
        work, snapshot, preview = self.fixture(colliding_paths=True)
        migration.apply(work, snapshot, preview)
        folder = work / "media" / "Old Example" / "Season 01"
        self.assertEqual((folder / "Example - S01E01.mkv").read_bytes(), b"SYNTHETIC PROLOGUE")
        self.assertEqual((folder / "Example - S01E02.mkv").read_bytes(), b"SYNTHETIC VIDEO 1")
        self.assertEqual((folder / "Example - S01E03.mkv").read_bytes(), b"SYNTHETIC VIDEO 2")
        migration.rollback(work)
        self.assert_restored(work, snapshot)

    def test_stale_or_blocked_input_does_not_start(self):
        work, snapshot, preview = self.fixture()
        bad = copy.deepcopy(preview)
        bad["blockers"] = ["Unresolved identity"]
        with self.assertRaises(ValueError):
            migration.apply(work, snapshot, bad)
        with sqlite3.connect(work / "ryokan.db") as db:
            db.execute("UPDATE episode_monitor_state SET monitored=1")
        with self.assertRaisesRegex(ValueError, "stale"):
            migration.apply(work, snapshot, preview)
        self.assertFalse((work / "journal").exists())

    def test_rollback_refuses_later_database_or_file_changes(self):
        for change in ("database", "file", "backup", "extra_file"):
            with self.subTest(change=change):
                work, snapshot, preview = self.fixture()
                migration.apply(work, snapshot, preview)
                if change == "database":
                    with sqlite3.connect(work / "ryokan.db") as db:
                        db.execute("INSERT INTO private_notes VALUES ('later edit')")
                elif change == "file":
                    path = work / "media" / preview["files"][0]["to"]
                    path.unlink()
                    path.write_bytes(b"external replacement")
                elif change == "backup":
                    with sqlite3.connect(work / "journal" / "before.sqlite") as db:
                        db.execute("INSERT INTO private_notes VALUES ('broken backup')")
                else:
                    path = work / "media" / preview["files"][0]["to"]
                    (path.parent / "untracked.txt").write_text("later file")
                paths = {str(p): p.read_bytes() for p in (work / "media").rglob("*") if p.is_file()}
                with self.assertRaises(ValueError):
                    migration.rollback(work)
                self.assertEqual(paths, {str(p): p.read_bytes() for p in (work / "media").rglob("*") if p.is_file()})

    def test_nfo_rejects_ambiguous_or_external_entities(self):
        fields = [{"season":1,"episode":2,"showtitle":"Show","title":"Title","aired":""}]
        for content in (b'<!DOCTYPE x [<!ENTITY y SYSTEM "file:///etc/passwd">]><episodedetails/>', b'<episodedetails><episode>1</episode><episode>2</episode></episodedetails>', b'<tvshow/>'):
            with self.assertRaises(ValueError):
                migration.nfo_bytes(content, fields)

    def test_combined_nfo_updates_each_episode_and_keeps_custom_fields(self):
        fields = [{"season":1,"episode":ep,"showtitle":"A & B","title":f"Episode {ep}","aired":""} for ep in (2,3)]
        output = migration.nfo_bytes(b'<episodedetails><episode>1</episode><custom id="a"/></episodedetails><episodedetails><episode>2</episode><custom id="b"/></episodedetails>', fields).decode()
        self.assertIn('<episode>2</episode>', output)
        self.assertIn('<episode>3</episode>', output)
        self.assertIn('<custom id="a"', output)
        self.assertIn('<custom id="b"', output)
        self.assertEqual(output.count('A &amp; B'), 2)


if __name__ == "__main__":
    unittest.main()
