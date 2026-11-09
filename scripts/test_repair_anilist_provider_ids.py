import copy
import json
from pathlib import Path
import tempfile
import unittest

from repair_anilist_provider_ids import apply, nfo_patch, normalize_ids, preview


class ProviderRepairTests(unittest.TestCase):
    def test_reader_accepts_both_spellings_and_preserves_all_other_bytes(self):
        original = (b'\xef\xbb\xbf<?xml version="1.0" encoding="UTF-8"?>\r\n'
                    b'<tvshow><!-- <uniqueid type="anilist">999</uniqueid> -->'
                    b'<title>Example &amp; title</title>'
                    b"<uniqueid default='true' type = 'anilist'>116589</uniqueid>"
                    b'<uniqueid type="mal">41457</uniqueid><plot><![CDATA[anilist]]></plot></tvshow>')
        fixed, ani_id, title, changes = nfo_patch(original)
        self.assertEqual(ani_id, "116589")
        self.assertEqual(title, "Example & title")
        self.assertEqual(changes, 1)
        self.assertEqual(fixed, original.replace(b"type = 'anilist'", b"type = 'AniList'"))
        self.assertEqual(nfo_patch(fixed), (fixed, "116589", title, 0))

    def test_mixed_case_and_duplicate_ids_with_the_same_value(self):
        original = b'<season><uniqueid type="ANILIST">12</uniqueid><uniqueid type="AniList">12</uniqueid></season>'
        fixed, ani_id, _, changes = nfo_patch(original)
        self.assertEqual((ani_id, changes), ("12", 1))
        self.assertNotIn(b'type="ANILIST"', fixed)
        self.assertEqual(normalize_ids({"anilist": "12", "AniList": "12", "Tmdb": "90"}),
                         {"AniList": "12", "Tmdb": "90"})

    def test_attribute_text_inside_another_attribute_is_untouched(self):
        original = b'''<tvshow><uniqueid label=" type='anilist'" type="anilist">12</uniqueid></tvshow>'''
        fixed, ani_id, _, changes = nfo_patch(original)
        self.assertEqual((ani_id, changes), ("12", 1))
        self.assertEqual(fixed, original.replace(b'type="anilist"', b'type="AniList"'))

    def test_conflicting_ids_and_external_entities_are_refused(self):
        with self.assertRaisesRegex(ValueError, "Conflicting"):
            nfo_patch(b'<tvshow><uniqueid type="anilist">12</uniqueid><uniqueid type="AniList">13</uniqueid></tvshow>')
        with self.assertRaisesRegex(ValueError, "document type"):
            nfo_patch(b'<!DOCTYPE tvshow [<!ENTITY x SYSTEM "file:///private">]><tvshow>&x;</tvshow>')
        with self.assertRaisesRegex(ValueError, "positive"):
            normalize_ids({"anilist": "-12"})

    def test_preview_and_repeated_apply_preserve_metadata_media_and_backups(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "media"
            folder = root / "Example"
            folder.mkdir(parents=True)
            nfo = folder / "tvshow.nfo"
            original = b'<tvshow><title>Example</title><uniqueid type="anilist">12</uniqueid><plot>Keep me</plot></tvshow>'
            nfo.write_bytes(original)
            media = folder / "episode.mkv"
            media.write_bytes(b"media bytes")
            plan = preview(root)
            self.assertEqual(nfo.read_bytes(), original)
            first = apply(plan, Path(temp) / "backup", "Example")
            after_stat = nfo.stat()
            self.assertEqual(first["nfos_changed"], ["Example/tvshow.nfo"])
            self.assertEqual(apply(plan, Path(temp) / "backup", "Example")["nfos_changed"], [])
            self.assertEqual(nfo.stat().st_mtime_ns, after_stat.st_mtime_ns)
            self.assertEqual((Path(temp) / "backup/nfos/Example/tvshow.nfo").read_bytes(), original)
            self.assertEqual(media.read_bytes(), b"media bytes")
            self.assertEqual(nfo_patch(nfo.read_bytes())[3], 0)

    def test_changed_nfo_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "media"
            root.mkdir()
            path = root / "tvshow.nfo"
            path.write_bytes(b'<tvshow><uniqueid type="anilist">12</uniqueid></tvshow>')
            plan = preview(root)
            path.write_bytes(b'<tvshow><uniqueid type="anilist">12</uniqueid><plot>New edit</plot></tvshow>')
            with self.assertRaisesRegex(ValueError, "changed since preview"):
                apply(plan, Path(temp) / "backup")

    def test_preview_reports_nfo_and_jellyfin_conflicts_without_changes(self):
        class Client:
            server_id = "server"

            def items(self, root):
                return [{"Id": "abc", "Name": "Example", "Type": "Series",
                         "Path": str(root / "Example"), "ProviderIds": {"anilist": "13"}}]

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "media"
            folder = root / "Example"
            folder.mkdir(parents=True)
            nfo = folder / "tvshow.nfo"
            original = b'<tvshow><uniqueid type="anilist">12</uniqueid></tvshow>'
            nfo.write_bytes(original)
            client = Client()
            plan = preview(root, client)
            self.assertEqual(plan["jellyfin"], [])
            self.assertIn("conflict", plan["issues"][0]["message"])
            with self.assertRaisesRegex(ValueError, "preview issues"):
                apply(plan, Path(temp) / "backup", "Example", client)
            self.assertEqual(nfo.read_bytes(), original)

    def test_api_update_keeps_the_complete_metadata_and_is_idempotent(self):
        class Client:
            server_id = "server"
            item_data = {"Id": "abc", "Path": "/data/anime/Example", "Name": "Example",
                         "Overview": "Keep", "Genres": ["Animation"], "LockedFields": ["Name"],
                         "LockData": True, "ForcedSortName": "Custom",
                         "ProviderIds": {"anilist": "12", "Tmdb": "99"},
                         "UserData": {"Played": True, "PlaybackPositionTicks": 123}}
            writes = 0

            def item(self, _):
                return copy.deepcopy(self.item_data)

            def call(self, path, method, body):
                self.assertion = (path, method)
                self.item_data = copy.deepcopy(body)
                self.writes += 1

        client = Client()
        before = copy.deepcopy(client.item_data)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "media"
            root.mkdir()
            plan = {"version": 1, "root": str(root), "nfos": [], "issues": [],
                    "server_id": "server", "jellyfin": [
                        {"id": "abc", "path": "Example", "before": before["ProviderIds"],
                         "after": {"AniList": "12", "Tmdb": "99"}}]}
            result = apply(plan, Path(temp) / "backup", "Example", client)
            self.assertEqual(result["jellyfin_changed"], ["abc"])
            self.assertEqual(client.item_data | {"ProviderIds": before["ProviderIds"]}, before)
            self.assertEqual(client.assertion, ("/Items/abc", "POST"))
            apply(plan, Path(temp) / "backup", "Example", client)
            self.assertEqual(client.writes, 1)
            self.assertEqual(json.loads((Path(temp) / "backup/jellyfin-abc.json").read_text()), before)

    def test_nfo_watcher_casing_change_can_resume_but_other_id_edits_cannot(self):
        class Client:
            server_id = "server"

            def __init__(self):
                self.item_data = {"ProviderIds": {"anilist": "12", "AniList": "12", "Tmdb": "99"}}
                self.writes = 0

            def item(self, _):
                return copy.deepcopy(self.item_data)

            def call(self, path, method, body):
                self.item_data = copy.deepcopy(body)
                self.writes += 1

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "media"
            root.mkdir()
            plan = {"version": 1, "root": str(root), "nfos": [], "issues": [],
                    "server_id": "server", "jellyfin": [
                        {"id": "abc", "path": "Example", "before": {"anilist": "12", "Tmdb": "99"},
                         "after": {"AniList": "12", "Tmdb": "99"}}]}
            client = Client()
            result = apply(plan, Path(temp) / "backup", "Example", client)
            self.assertEqual(result["jellyfin_changed"], ["abc"])
            self.assertEqual(client.item_data["ProviderIds"], plan["jellyfin"][0]["after"])
            for changed in ({"anilist": "13", "Tmdb": "99"}, {"anilist": "12", "Tmdb": "100"},
                            {"AniList": "13", "Tmdb": "99"}):
                client.item_data["ProviderIds"] = changed
                with self.assertRaisesRegex(ValueError, "IDs changed"):
                    apply(plan, Path(temp) / "backup", "Example", client)
            self.assertEqual(client.writes, 1)


if __name__ == "__main__":
    unittest.main()
