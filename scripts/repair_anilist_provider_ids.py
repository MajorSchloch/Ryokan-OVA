#!/usr/bin/env python3
"""Preview and repair AniList provider-key casing in NFOs and Jellyfin.

Defaults to preview. Set JELLYFIN_API_KEY for optional Jellyfin API access.
Apply a reviewed plan with --apply --plan PATH --backup-dir PATH.
Use --folder RELATIVE_FOLDER to repair one title first.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import tempfile
from urllib.parse import urlencode
from urllib.request import Request, urlopen
from xml.parsers import expat

CANONICAL = "AniList"
TAG = re.compile(rb"""<uniqueid\b(?:[^>"']|"[^"]*"|'[^']*')*>""")
ATTRIBUTE = re.compile(rb"""\s+([^\s=/>]+)\s*=\s*(["'])(.*?)\2""", re.S)
# Fields written by Jellyfin's metadata editor. Verify them after each API update.
METADATA = """Name ForcedSortName OriginalTitle OriginalLanguage CriticRating CommunityRating
IndexNumber ParentIndexNumber Overview Genres Taglines Studios DateCreated EndDate PremiereDate
ProductionYear OfficialRating CustomRating Tags ProductionLocations PreferredMetadataCountryCode
PreferredMetadataLanguage DisplayOrder AspectRatio LockData LockedFields RunTimeTicks Status
AirDays AirTime People""".split()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def normalize_ids(ids):
    """Accept both the canonical and legacy spellings, retaining every other ID."""
    matches = {str(value) for key, value in ids.items() if key.casefold() == "anilist"}
    if len(matches) > 1:
        raise ValueError("Conflicting AniList IDs need review")
    result = {k: v for k, v in ids.items() if k.casefold() != "anilist"}
    if matches:
        value = matches.pop()
        if not value.isdecimal() or int(value) <= 0:
            raise ValueError("AniList ID must be a positive integer")
        result[CANONICAL] = value
    return result


def nfo_patch(data):
    """Patch actual XML attribute values, preserving all other bytes."""
    if len(data) > 4 * 1024 * 1024:
        raise ValueError("NFO exceeds the 4 MiB review limit")
    data.decode("utf-8-sig")  # Ryokan exports UTF-8. Leave other encodings for review.
    parser = expat.ParserCreate()
    stack, values, edits = [], [], []
    title, text = [], []
    current = None

    def start(name, attrs):
        nonlocal current
        stack.append(name)
        if name == "uniqueid" and attrs.get("type", "").casefold() == "anilist":
            current = []
            token = TAG.match(data, parser.CurrentByteIndex)
            if not token:
                raise ValueError("Cannot locate the provider attribute")
            attr = next((a for a in ATTRIBUTE.finditer(token.group()) if a[1] == b"type"), None)
            if not attr:
                raise ValueError("Cannot locate the provider attribute")
            if attrs["type"] != CANONICAL:
                edits.append((token.start() + attr.start(3), token.start() + attr.end(3)))
        if name == "title" and len(stack) == 2:
            text.clear()

    def chars(value):
        if current is not None:
            current.append(value)
        if stack and stack[-1] == "title" and len(stack) == 2:
            text.append(value)

    def end(name):
        nonlocal current
        if name == "uniqueid" and current is not None:
            values.append("".join(current).strip())
            current = None
        if name == "title" and len(stack) == 2 and not title:
            title.append("".join(text))
        stack.pop()

    def doctype(*_):
        raise ValueError("NFO with a document type needs review")

    parser.StartElementHandler = start
    parser.CharacterDataHandler = chars
    parser.EndElementHandler = end
    parser.StartDoctypeDeclHandler = doctype
    parser.Parse(data, True)
    ids = {}
    if values:
        ids = normalize_ids({"anilist": values[0]})
        if any(value != ids[CANONICAL] for value in values):
            raise ValueError("Conflicting AniList IDs need review")
    result = data
    for first, last in reversed(edits):
        result = result[:first] + b"AniList" + result[last:]
    return result, ids.get(CANONICAL), title[0] if title else "", len(edits)


def safe_file(root, relative):
    rel = Path(relative)
    if rel.is_absolute() or not rel.parts or any(p in (".", "..") for p in rel.parts):
        raise ValueError("Invalid relative NFO path")
    path = root
    for part in rel.parts:
        path = path / part
        if path.is_symlink():
            raise ValueError("Linked metadata needs review")
    if not path.is_file():
        raise ValueError("NFO is missing or is not a regular file")
    return path


class Jellyfin:
    def __init__(self, url, token):
        if not token:
            raise ValueError("Set JELLYFIN_API_KEY for Jellyfin access")
        self.url, self.token = url.rstrip("/"), token
        self.server_id = self.call("/System/Info/Public")["Id"]
        users = self.call("/Users")
        self.admin = next(u["Id"] for u in users if u.get("Policy", {}).get("IsAdministrator"))

    def call(self, path, method="GET", body=None):
        data = None if body is None else json.dumps(body).encode()
        request = Request(self.url + path, data=data, method=method, headers={
            "Authorization": 'MediaBrowser Token="' + self.token.replace('"', "") + '"',
            "Content-Type": "application/json",
        })
        with urlopen(request, timeout=90) as response:
            raw = response.read()
            return json.loads(raw) if raw else None

    def item(self, item_id):
        return self.call("/Users/" + self.admin + "/Items/" + item_id)

    def items(self, root):
        result, start = [], 0
        while True:
            page = self.call("/Items?" + urlencode({
                "Recursive": "true", "IncludeItemTypes": "Series,Season,Movie",
                "Fields": "ProviderIds,Path", "StartIndex": start, "Limit": 200,
            }))
            for item in page["Items"]:
                path = PurePosixPath(item.get("Path", ""))
                if path.is_relative_to(root):
                    result.append(item)
            start += len(page["Items"])
            if not page["Items"] or start >= page["TotalRecordCount"]:
                return result


def preview(root, jellyfin=None, jellyfin_root="/data/anime"):
    root = root.resolve(strict=True)
    plan = {"version": 1, "root": str(root), "nfos": [], "jellyfin": [], "issues": []}
    for current, dirs, files in os.walk(root, followlinks=False):
        for name in dirs:
            if (Path(current) / name).is_symlink():
                plan["issues"].append({"path": (Path(current) / name).relative_to(root).as_posix(),
                                       "message": "Linked directory needs review"})
        dirs[:] = sorted(p for p in dirs if not (Path(current) / p).is_symlink())
        for name in sorted(files):
            if not name.lower().endswith(".nfo"):
                continue
            path = Path(current) / name
            relative = path.relative_to(root).as_posix()
            try:
                path = safe_file(root, relative)
                data = path.read_bytes()
                # Multi-episode NFOs without provider IDs need no XML parsing.
                if b"anilist" not in data.lower():
                    continue
                after, ani_id, title, changes = nfo_patch(data)
                if ani_id:
                    plan["nfos"].append({
                        "path": relative, "title": title or path.parent.name,
                        "anilist_id": ani_id, "changes": changes,
                        "before_sha256": digest(data), "after_sha256": digest(after),
                    })
            except (ValueError, OSError, expat.ExpatError) as error:
                plan["issues"].append({"path": relative, "message": str(error)})
    if jellyfin:
        plan["server_id"] = jellyfin.server_id
        plan["jellyfin_root"] = jellyfin_root
        by_folder = {}
        for nfo in plan["nfos"]:
            if Path(nfo["path"]).name.lower() in ("tvshow.nfo", "season.nfo", "movie.nfo"):
                by_folder[str(PurePosixPath(nfo["path"]).parent)] = nfo
        for item in jellyfin.items(PurePosixPath(jellyfin_root)):
            ids = item.get("ProviderIds") or {}
            relative = PurePosixPath(item["Path"]).relative_to(jellyfin_root)
            try:
                after = normalize_ids(ids)
                folder = relative.parent if item["Type"] == "Movie" else relative
                nfo = by_folder.get(str(folder))
                if nfo and after.get(CANONICAL) not in (None, nfo["anilist_id"]):
                    raise ValueError("NFO and Jellyfin AniList IDs conflict")
                if ids != after:
                    plan["jellyfin"].append({
                        "id": item["Id"], "path": str(relative), "title": item["Name"],
                        "type": item["Type"], "before": ids, "after": after,
                    })
            except ValueError as error:
                plan["issues"].append({"path": str(relative), "message": str(error)})
    return plan


def atomic_write(path, data, expected):
    before = path.lstat()
    if not stat.S_ISREG(before.st_mode) or path.read_bytes() != expected:
        raise ValueError("Metadata changed before the repair")
    temp = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, prefix="." + path.name, delete=False) as out:
            temp = Path(out.name)
            out.write(data)
            out.flush()
            os.fchmod(out.fileno(), stat.S_IMODE(before.st_mode))
            if os.geteuid() == 0:
                os.fchown(out.fileno(), before.st_uid, before.st_gid)
            os.fsync(out.fileno())
        current = path.lstat()
        identity = lambda value: (value.st_dev, value.st_ino, value.st_mtime_ns,
                                  value.st_ctime_ns, value.st_size, value.st_mode)
        if identity(current) != identity(before) or path.read_bytes() != expected:
            raise ValueError("Metadata changed during the repair")
        os.replace(temp, path)
        fd = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    finally:
        if temp and temp.exists():
            temp.unlink()


def metadata(item):
    return {key: item.get(key) for key in METADATA}


def selected(path, folder):
    return not folder or Path(path).parts[0] == folder


def apply(plan, backup, folder=None, jellyfin=None):
    if plan["version"] != 1:
        raise ValueError("Unsupported repair plan")
    root = Path(plan["root"]).resolve(strict=True)
    if folder and (Path(folder).name != folder or folder in (".", "..")):
        raise ValueError("Choose one direct title folder")
    issues = [r for r in plan["issues"] if selected(r["path"], folder)]
    if issues:
        raise ValueError("Resolve the selected preview issues before repair")
    if plan["jellyfin"] and not jellyfin:
        raise ValueError("This plan includes Jellyfin updates. Supply its API connection.")
    if jellyfin and plan.get("server_id") != jellyfin.server_id:
        raise ValueError("The repair plan belongs to another Jellyfin server")
    backup = backup.resolve()
    if backup.is_relative_to(root):
        raise ValueError("Keep repair backups outside the media library")
    backup.mkdir(parents=True, exist_ok=True, mode=0o700)
    report = {"nfos_changed": [], "jellyfin_changed": [], "already_correct": []}
    # Validate all selected files before writing any of them.
    pending = []
    for entry in plan["nfos"]:
        if not selected(entry["path"], folder):
            continue
        path = safe_file(root, entry["path"])
        data = path.read_bytes()
        after, ani_id, _, _ = nfo_patch(data)
        if ani_id != entry["anilist_id"]:
            raise ValueError("AniList ID changed since preview")
        if digest(data) == entry["after_sha256"]:
            report["already_correct"].append(entry["path"])
            continue
        if digest(data) != entry["before_sha256"] or digest(after) != entry["after_sha256"]:
            raise ValueError("NFO changed since preview. Preview again.")
        pending.append((entry, path, data, after))
    for entry, path, data, after in pending:
        saved = backup / "nfos" / entry["path"]
        saved.parent.mkdir(parents=True, exist_ok=True)
        if not saved.resolve().is_relative_to(backup):
            raise ValueError("The backup path escapes the recovery directory")
        if saved.exists():
            if saved.read_bytes() != data:
                raise ValueError("The backup contains a different original NFO")
        else:
            with saved.open("xb") as out:
                out.write(data)
                out.flush()
                os.fsync(out.fileno())
            shutil.copystat(path, saved)
        atomic_write(path, after, data)
        report["nfos_changed"].append(entry["path"])
    if jellyfin:
        for entry in plan["jellyfin"]:
            if not selected(entry["path"], folder):
                continue
            before = jellyfin.item(entry["id"])
            ids = before.get("ProviderIds") or {}
            wanted = normalize_ids(ids)
            if ids == wanted:
                report["already_correct"].append(entry["id"])
                continue
            if ids != entry["before"] or wanted != entry["after"]:
                raise ValueError("Jellyfin IDs changed since preview. Preview again.")
            saved = backup / ("jellyfin-" + entry["id"] + ".json")
            if not saved.exists():
                with saved.open("x") as out:
                    json.dump(before, out, ensure_ascii=False, indent=2)
                    out.flush()
                    os.fsync(out.fileno())
            changed = copy.deepcopy(before)
            changed["ProviderIds"] = wanted
            jellyfin.call("/Items/" + entry["id"], "POST", changed)
            after = jellyfin.item(entry["id"])
            if after.get("ProviderIds") != wanted or metadata(after) != metadata(before):
                raise ValueError("Jellyfin verification failed. Review the saved original metadata.")
            report["jellyfin_changed"].append(entry["id"])
    # Jellyfin's metadata saver must not undo the exact NFO edit.
    for entry, path, data, after in pending:
        current = path.read_bytes()
        if current != after:
            raise ValueError("Another metadata writer changed the repaired NFO. Review the backup.")
    (backup / "result.json").write_text(json.dumps(report, indent=2))
    return report


def main(argv=None):
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--jellyfin-url")
    parser.add_argument("--jellyfin-root", default="/data/anime")
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--plan", type=Path)
    parser.add_argument("--backup-dir", type=Path)
    parser.add_argument("--folder")
    args = parser.parse_args(argv)
    client = Jellyfin(args.jellyfin_url, os.environ.get("JELLYFIN_API_KEY")) if args.jellyfin_url else None
    if args.apply:
        if not args.plan or not args.backup_dir:
            parser.error("--apply requires --plan and --backup-dir")
        result = apply(json.loads(args.plan.read_text()), args.backup_dir, args.folder, client)
    else:
        if not args.root:
            parser.error("Preview requires --root")
        result = preview(args.root, client, args.jellyfin_root)
    text = json.dumps(result, ensure_ascii=False, indent=2)
    if args.output:
        args.output.write_text(text + "\n")
    else:
        print(text)


if __name__ == "__main__":
    main()
