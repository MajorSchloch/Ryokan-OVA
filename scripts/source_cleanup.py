#!/usr/bin/env python3
"""Bounded administrative cleanup of completed source retirements.

Preview uses read-only database/client access. Apply and restore require Ryokan
stopped, acquisition held, and its existing library lease. Only exact regular
files move, on one filesystem, with durable receipts and no automatic expiry.
Native jobs, source blocks, catalog rows and media contents are never changed.
"""
import argparse
import base64
import contextlib
import ctypes
import datetime
import errno
import fcntl
import hashlib
import http.cookiejar
import json
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import sqlite3
import stat
import subprocess
import sys
import urllib.parse
import urllib.request

CAP = 16 * 1024 * 1024


def identity(path):
    s = Path(path).lstat()
    if not stat.S_ISREG(s.st_mode) or Path(path).resolve() != Path(path):
        raise ValueError("Not an exact regular file; symlinks require review")
    return dict(size=s.st_size, device=s.st_dev, inode=s.st_ino, mtime_ns=str(s.st_mtime_ns))


def beneath(path, root):
    return path != root and root in path.parents


def host_path(value, args):
    p = PurePosixPath(value)
    if not p.is_absolute() or ".." in p.parts:
        raise ValueError("Invalid recorded absolute path")
    for prefix, target in ((PurePosixPath('/data'), args.data_root), (PurePosixPath('/config'), args.db.parent)):
        if p == prefix or prefix in p.parents:
            return Path(target).joinpath(*p.relative_to(prefix).parts)
    raise ValueError("Recorded path has no reviewed host mapping")


def atomic_json(path, value, owner=None):
    path = Path(path)
    durable_directory(path.parent)
    if path.parent.is_symlink() or path.is_symlink():
        raise ValueError("Receipt path is a symlink")
    if owner:
        os.chown(path.parent, *owner)
    temp = path.with_name(path.name + '.' + secrets.token_hex(4) + '.tmp')
    try:
        with temp.open('x', encoding='utf-8') as f:
            os.chmod(temp, 0o600)
            json.dump(value, f, indent=2)
            f.write('\n')
            f.flush()
            os.fsync(f.fileno())
        if owner:
            os.chown(temp, *owner)
        os.replace(temp, path)
        fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    finally:
        if temp.exists():
            temp.unlink()


def durable_directory(path):
    path=Path(path)
    if path.exists():
        if path.resolve()!=path or not path.is_dir():raise ValueError("Directory path is not canonical")
        return
    durable_directory(path.parent)
    path.mkdir(mode=0o700)
    for directory in (path,path.parent):
        fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY)
        try:os.fsync(fd)
        finally:os.close(fd)


def validate_roots(args, protected=()):
    if args.quarantine_root!=args.db.parent/'source-quarantine':
        raise ValueError("Quarantine must use the dedicated app-data source-quarantine directory")
    for path in (args.source_root,*protected):
        if path==args.quarantine_root or path in args.quarantine_root.parents or args.quarantine_root in path.parents:
            raise ValueError("Quarantine overlaps a protected/source tree")


def rename_no_replace(source, target):
    """Atomic no-overwrite rename; there is deliberately no copy/unlink fallback."""
    lib = ctypes.CDLL(None, use_errno=True)
    if sys.platform.startswith('linux'):
        result = lib.renameat2(-100, os.fsencode(source), -100, os.fsencode(target), 1)
    elif sys.platform == 'darwin':
        result = lib.renamex_np(os.fsencode(source), os.fsencode(target), 4)
    else:
        raise ValueError("Atomic no-replace rename is unavailable")
    if result != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))


def read_db(args):
    if args.db.is_symlink() or args.db.resolve() != args.db:
        raise ValueError("Database path must be canonical")
    c = sqlite3.connect(args.db.as_uri() + '?mode=ro', uri=True)
    c.row_factory = sqlite3.Row
    return c


def retirements(c):
    result = {}
    for row in c.execute('SELECT * FROM torrent_seed_reviews WHERE retired_at IS NOT NULL AND blocked_at IS NOT NULL AND unblocked_at IS NULL'):
        r = dict(row)
        if not re.fullmatch(r'[0-9a-f]{40}|[0-9a-f]{64}', r['hash']):
            raise ValueError("Invalid retirement hash")
        owners = [dict(x) for x in c.execute('SELECT * FROM grabbed_torrents WHERE lower(hash)=?', (r['hash'],))]
        if len(owners) != 1:
            continue
        owner = owners[0]
        if (owner['id'], owner['grabbed_at'], owner['download_client_id']) != (r['retirement_owner'], r['retirement_attempt'], r['client_id']):
            continue
        if owner['state'] not in ('failed', 'removed', 'cancelled', 'imported', 'replaced') or owner['import_mode'] == 'move':
            continue
        snapshot = json.loads(r['retirement_snapshot'])
        if snapshot['owner']['series_id'] != owner['series_id']:
            continue
        result[(r['client_id'], r['hash'])] = (r, snapshot)
    return result


def protected_paths(c, args, native_inventory):
    paths, covers, inodes, hashes = native_inventory(c, args)
    cfg = dict(c.execute('SELECT * FROM config WHERE id=1').fetchone())
    roots = [host_path(cfg['media_root'], args), args.compat_root]
    if cfg['recycle_bin_path']:
        roots.append(host_path(cfg['recycle_bin_path'], args))
    validate_roots(args,roots)
    for root in roots:
        if not root.is_dir():
            raise ValueError("A protected library/recovery mount is unavailable")
        covers.add(root.resolve())
        def walk_error(error):raise error
        for parent, dirs, files in os.walk(root, followlinks=False,onerror=walk_error):
            for name in dirs + files:
                p = Path(parent) / name
                if p.is_symlink():
                    raw = Path(os.readlink(p))
                    if raw.is_absolute() and (str(raw) == '/data' or str(raw).startswith('/data/') or str(raw) == '/config' or str(raw).startswith('/config/')):
                        target = host_path(str(raw), args).resolve()
                    else:
                        target = p.resolve()
                    # A dangling link still reserves its exact target. A link
                    # to a directory reserves the entire referenced subtree.
                    (covers if target.is_dir() else paths).add(target)
                    if target.is_file():
                        s = target.stat()
                        inodes.add((s.st_dev, s.st_ino))
                if p.is_file():
                    s = p.stat()
                    inodes.add((s.st_dev, s.st_ino))
    for row in c.execute("SELECT imported_source_paths,client_content_path FROM grabbed_torrents WHERE state='pending'"):
        for p in json.loads(row['imported_source_paths'] or '[]'):
            paths.add(host_path(p, args).resolve())
        if row['client_content_path']:
            covers.add(host_path(row['client_content_path'], args).resolve())
    hashes.update(x[0].lower() for x in c.execute('SELECT info_hash FROM pending_grabs'))
    return paths, covers, inodes, hashes


def request_json(opener, request):
    with opener.open(request, timeout=30) as response:
        body = response.read(CAP + 1)
        if len(body) > CAP:
            raise ValueError("Native inventory exceeded its complete-read limit")
        return json.loads(body)


def native_inventory(c, args, sessions=None):
    """Every configured native client/category, not only Ryokan-owned jobs."""
    paths, covers, inodes, hashes = set(), set(), set(), set()
    sessions = {} if sessions is None else sessions
    ip = None
    for record in c.execute('SELECT * FROM download_clients ORDER BY id'):
        row = dict(record)
        url = urllib.parse.urlsplit(row['url'])
        if url.hostname == 'gluetun':
            if ip is None:
                ip = subprocess.check_output(['docker', 'inspect', 'gluetun', '--format', '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}'], text=True).strip()
            url = url._replace(netloc=ip + (':' + str(url.port) if url.port else ''))
        base = urllib.parse.urlunsplit(url).rstrip('/')
        if row['kind'] == 'qbittorrent':
            key=(row['id'],base,hashlib.sha256((row['username']+':'+row['password']).encode()).hexdigest())
            opener=sessions.get(key)
            if opener is None:
                opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
                # The authenticated read, not the login text, proves access.
                opener.open(urllib.request.Request(base + '/api/v2/auth/login', data=urllib.parse.urlencode({'username':row['username'], 'password':row['password']}).encode()), timeout=30).read(CAP)
                sessions[key]=opener
            jobs = request_json(opener, base + '/api/v2/torrents/info')
            if not isinstance(jobs, list):
                raise ValueError("Incomplete native torrent inventory")
            for job in jobs:
                hashes.add(job['hash'].lower())
                files = request_json(opener, base + '/api/v2/torrents/files?' + urllib.parse.urlencode({'hash':job['hash']}))
                if not isinstance(files,list):raise ValueError("Native file inventory is incomplete")
                if not files:
                    if not job.get('content_path'):
                        raise ValueError("A native metadata job has no bounded path")
                    covers.add(host_path(job['content_path'], args).resolve())
                for f in files:
                    p = PurePosixPath(f['name'])
                    if p.is_absolute() or '..' in p.parts:
                        raise ValueError("Unsafe native file path")
                    path = host_path(str(PurePosixPath(job['save_path']) / p), args).resolve()
                    paths.add(path)
                    if path.is_file():
                        s = path.stat(); inodes.add((s.st_dev, s.st_ino))
        elif row['kind'] == 'nzbget':
            opener = urllib.request.build_opener()
            auth = base64.b64encode((row['username'] + ':' + row['password']).encode()).decode()
            for method, params in (('listgroups',[0]), ('history',[True])):
                reply = request_json(opener, urllib.request.Request(base + '/jsonrpc', data=json.dumps({'method':method,'params':params,'id':1}).encode(), headers={'Authorization':'Basic '+auth,'Content-Type':'application/json'}))
                if reply.get('error') or not isinstance(reply.get('result'),list):
                    raise ValueError("NZBGet inventory unavailable")
                for job in reply['result']:
                    found = False
                    for field in ('DestDir','FinalDir'):
                        if job.get(field):
                            covers.add(host_path(job[field], args).resolve()); found = True
                    if method == 'listgroups' and not found:
                        raise ValueError("A queued Usenet job has no bounded path")
        else:
            raise ValueError("An unsupported configured client prevents a complete reference inventory")
    return paths, covers, inodes, hashes


def allowed(path, expected, refs, args):
    if not beneath(path, args.source_root) or path.resolve() != path:
        raise ValueError("Source leaves the reviewed root or uses a symlink")
    current = identity(path)
    if current != expected:
        raise ValueError("File identity changed; original retained")
    paths, covers, inodes, _ = refs
    if path in paths or any(path == p or p in path.parents for p in covers) or (current['device'],current['inode']) in inodes:
        raise ValueError("Another download, library or recovery entry references this file")
    if path.stat().st_nlink != 1:
        raise ValueError("File has additional hardlinks; retained for review")


def preview(args, inventory=native_inventory):
    with contextlib.closing(read_db(args)) as c:
        refs = protected_paths(c, args, inventory)
        batch = args.plan.stem
        if not re.fullmatch(r'[A-Za-z0-9_-]{1,80}',batch):raise ValueError("Invalid cleanup batch name")
        report = dict(format_version=1, batch=batch, source_root=str(args.source_root), quarantine_root=str(args.quarantine_root), entries=[], updated_at=datetime.datetime.now(datetime.timezone.utc).isoformat(), purpose='Recoverable administrative cleanup; no automatic expiry; no disk-space saving claimed')
        seen = set()
        for (client, hash_), (row, snapshot) in retirements(c).items():
            for f in snapshot['files']:
                part = PurePosixPath(f['name'])
                if part.is_absolute() or '..' in part.parts:
                    raise ValueError("Invalid saved source manifest")
                path = host_path(str(PurePosixPath(snapshot['item']['save_path']) / part), args)
                if path in seen:
                    continue
                seen.add(path)
                entry = dict(client_id=client, hash=hash_, owner=row['retirement_owner'], attempt=row['retirement_attempt'], name=row['name'], original_path=str(path), size=max(0,f['size']), quarantine_path=str(args.quarantine_root / batch / str(len(report['entries']))), status='planned', reason='', identity=None)
                try:
                    entry['identity'] = identity(path)
                    if entry['identity']['size'] != f['size'] or hash_ in refs[3]:
                        raise ValueError("Source size changed or native job still exists")
                    allowed(path,entry['identity'],refs,args)
                except FileNotFoundError:
                    entry['status']='absent';entry['reason']='Already absent; no files moved'
                except (OSError,ValueError) as error:
                    entry['status']='held';entry['reason']=str(error)
                report['entries'].append(entry)
        return report


@contextlib.contextmanager
def stopped_lease(args):
    state = json.loads(subprocess.check_output(['docker','inspect',args.container]))[0]
    if state['State']['Running']:
        raise ValueError("Stop Ryokan before a physical cleanup or restore")
    with contextlib.closing(read_db(args)) as c:
        cfg = dict(c.execute('SELECT * FROM config WHERE id=1').fetchone())
        if any(cfg[k] for k in ('rss_master_enabled','upgrade_search_enabled','auto_redownload_failed','post_processing_enabled')):
            raise ValueError("Acquisition and post-processing must be held")
        if c.execute('SELECT COUNT(*) FROM download_clients WHERE enabled!=0').fetchone()[0] or c.execute('SELECT COUNT(*) FROM pending_grabs').fetchone()[0]:
            raise ValueError("Client routing or file selections remain active")
    lock = Path(str(args.db)+'.library.lock')
    if os.path.lexists(str(args.db)+'.library-migration.json'):
        raise ValueError("Library migration is active")
    fd = os.open(lock,os.O_RDWR|os.O_NOFOLLOW)
    try:
        fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
        if (os.fstat(fd).st_dev,os.fstat(fd).st_ino)!=(lock.stat().st_dev,lock.stat().st_ino):
            raise ValueError("Library lease changed")
        yield
    finally:
        os.close(fd)


def move_entry(entry, restore=False):
    source = Path(entry['quarantine_path'] if restore else entry['original_path'])
    target = Path(entry['original_path'] if restore else entry['quarantine_path'])
    if identity(source) != entry['identity']:
        raise ValueError("File changed before movement")
    if target.exists() or target.is_symlink():
        raise ValueError("Target exists; no overwrite permitted")
    if restore and not target.parent.is_dir():
        raise ValueError("Original parent directory is missing; restore held")
    durable_directory(target.parent)
    if source.parent.resolve()!=source.parent or target.parent.resolve()!=target.parent:
        raise ValueError("Movement parent uses a symlink")
    if source.stat().st_dev!=target.parent.stat().st_dev:
        raise ValueError("Cross-filesystem source cleanup requires review")
    rename_no_replace(source,target)
    if identity(target)!=entry['identity']:
        # Preserve an unexpected replacement; put it back only without overwrite.
        rename_no_replace(target,source)
        raise ValueError("Source changed during movement; restored without overwrite")
    for directory in {source.parent,target.parent}:
        fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY)
        try:os.fsync(fd)
        finally:os.close(fd)


def execute(args, report, restore=False, inventory=native_inventory):
    if report['format_version']!=1 or report['source_root']!=str(args.source_root) or report['quarantine_root']!=str(args.quarantine_root):
        raise ValueError("Receipt does not match the reviewed roots")
    validate_roots(args)
    if report['batch']!=args.plan.stem or not re.fullmatch(r'[A-Za-z0-9_-]{1,80}',report['batch']) or len(report['entries'])>4096:
        raise ValueError("Invalid receipt batch or entry limit")
    originals=set();destinations=set()
    for index,entry in enumerate(report['entries']):
        if entry['quarantine_path']!=str(args.quarantine_root/report['batch']/str(index)) or entry['original_path'] in originals or entry['quarantine_path'] in destinations:
            raise ValueError("Duplicate or unbound receipt path")
        originals.add(entry['original_path']);destinations.add(entry['quarantine_path'])
    owner=(args.db.stat().st_uid,args.db.stat().st_gid)
    if inventory is native_inventory:
        sessions={}
        inventory=lambda c,a:native_inventory(c,a,sessions)
    def save():
        report['updated_at']=datetime.datetime.now(datetime.timezone.utc).isoformat()
        atomic_json(args.plan,report,owner)
    with stopped_lease(args), contextlib.closing(read_db(args)) as c:
        current=retirements(c)
        paths,covers,inodes,hashes=protected_paths(c,args,inventory)
        for entry in report['entries']:
            if entry['identity'] is None:
                continue
            try:
                # Native users/other tools do not share Ryokan's lease. Read
                # every category again immediately before each exact move.
                fresh=inventory(c,args)
                refs=(paths|fresh[0],covers|fresh[1],inodes|fresh[2],hashes|fresh[3])
                source=Path(entry['original_path']);target=Path(entry['quarantine_path'])
                if not beneath(source,args.source_root) or not beneath(target,args.quarantine_root) or source.resolve()!=source or target.parent.resolve()!=target.parent:
                    raise ValueError("Receipt path is outside its reviewed root")
                record=current.get((entry['client_id'],entry['hash']))
                if not record or (record[0]['retirement_owner'],record[0]['retirement_attempt'])!=(entry['owner'],entry['attempt']) or entry['hash'] in refs[3]:
                    raise ValueError("Retirement ownership/block or native absence changed")
                permitted={host_path(str(PurePosixPath(record[1]['item']['save_path'])/f['name']),args):f['size'] for f in record[1]['files']}
                if source not in permitted or entry['identity']['size']!=permitted[source]:
                    raise ValueError("File is not in the exact retirement manifest")
                if not restore and not source.exists() and target.exists():
                    if identity(target)!=entry['identity']:raise ValueError("Quarantine identity differs")
                    entry['status']='quarantined';save();continue
                if restore and source.exists() and not target.exists():
                    if identity(source)!=entry['identity']:raise ValueError("Original target differs; no overwrite permitted")
                    entry['status']='restored';entry['reason']='Exact original already present; no movement needed';save();continue
                if restore and not source.exists() and target.exists():
                    if identity(target)!=entry['identity']:raise ValueError("Quarantine identity differs")
                    entry['status']='quarantined'
                if entry['status'] in ('held','absent','restored'):
                    continue
                if restore:
                    if source.exists() or source.is_symlink():raise ValueError("Restore target exists")
                    if source in refs[0] or any(source==p or p in source.parents for p in refs[1]):raise ValueError("Restore target is used by another job")
                else:
                    allowed(source,entry['identity'],refs,args)
                entry['status']='restoring' if restore else 'moving';save()
                move_entry(entry,restore)
                entry['status']='restored' if restore else 'quarantined';entry['reason']='Source block retained; no native client control';save()
                if not restore:
                    fresh=inventory(c,args)
                    if source in fresh[0] or any(source==p or p in source.parents for p in fresh[1]):
                        move_entry(entry,True)
                        entry['status']='held';entry['reason']='A native reference appeared during cleanup; source restored';save()
            except (OSError,ValueError) as error:
                entry['status']='held';entry['reason']=str(error);save()
        save()
    return report


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('mode',choices=('preview','apply','restore'))
    p.add_argument('--db',type=Path,required=True)
    p.add_argument('--data-root',type=Path,required=True)
    p.add_argument('--source-root',type=Path,required=True)
    p.add_argument('--compat-root',type=Path,required=True)
    p.add_argument('--quarantine-root',type=Path)
    p.add_argument('--plan',type=Path,required=True)
    p.add_argument('--container',default='ryokan')
    args=p.parse_args();os.umask(0o077)
    args.quarantine_root=args.quarantine_root or args.db.parent/'source-quarantine'
    for path in (args.db,args.data_root,args.source_root,args.compat_root,args.quarantine_root,args.plan.parent):
        if not path.is_absolute() or path.resolve()!=path:raise ValueError("All administrative paths must be canonical and absolute")
    if args.plan.parent!=args.db.parent/'source-cleanup':raise ValueError("Receipt must be in the app data directory's source-cleanup folder")
    if args.mode=='preview':
        if args.plan.exists() or args.plan.is_symlink():raise ValueError("Preview must not overwrite an existing recovery receipt")
        report=preview(args);atomic_json(args.plan,report,(args.db.stat().st_uid,args.db.stat().st_gid))
    else:
        if args.plan.is_symlink() or args.plan.stat().st_size>4*1024*1024:raise ValueError("Unsafe receipt")
        report=execute(args,json.loads(args.plan.read_text()),args.mode=='restore')
    counts={s:sum(e['status']==s for e in report['entries']) for s in sorted({e['status'] for e in report['entries']})}
    print(json.dumps({'status_counts':counts,'quarantined_logical_bytes':sum(e['size'] for e in report['entries'] if e['status']=='quarantined'),'disk_bytes_freed':0}))


if __name__=='__main__':
    try:main()
    except Exception as error:
        print('Source cleanup stopped: '+str(error),file=sys.stderr);sys.exit(1)
