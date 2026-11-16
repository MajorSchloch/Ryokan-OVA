import contextlib
import copy
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('source_cleanup',Path(__file__).parents[1]/'source_cleanup.py')
tool=importlib.util.module_from_spec(spec);spec.loader.exec_module(tool)
HASH='a'*40


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        root=Path(self.temp.name).resolve();config=root/'config';config.mkdir()
        self.args=SimpleNamespace(db=config/'ryokan.db',data_root=root/'data',source_root=root/'data'/'downloads',compat_root=root/'data'/'compat',quarantine_root=config/'source-quarantine',plan=config/'source-cleanup'/'fixture.json',container='test-fixture')
        for p in (self.args.source_root,self.args.compat_root,root/'data'/'anime',config/'recycle'):p.mkdir(parents=True)
        self.source=self.args.source_root/'Show - 01.mkv';self.source.write_bytes(b'original media')
        self.backup=root/'data'/'anime'/'independent.mkv';self.backup.write_bytes(b'library copy')
        self.c=sqlite3.connect(self.args.db);self.addCleanup(self.c.close)
        self.c.executescript('''CREATE TABLE config(id,media_root,recycle_bin_path,rss_master_enabled,upgrade_search_enabled,auto_redownload_failed,post_processing_enabled);
            CREATE TABLE download_clients(id,enabled);
            CREATE TABLE pending_grabs(info_hash);
            CREATE TABLE grabbed_torrents(id,hash,grabbed_at,download_client_id,series_id,state,import_mode,imported_source_paths,client_content_path);
            CREATE TABLE torrent_seed_reviews(client_id,hash,name,retired_at,blocked_at,unblocked_at,retirement_owner,retirement_attempt,retirement_snapshot);''')
        self.c.execute("INSERT INTO config VALUES(1,'/data/anime','/config/recycle',0,0,0,0)")
        self.c.execute("INSERT INTO grabbed_torrents VALUES(1,?,'attempt',1,2,'imported','copy','[]','')",(HASH,))
        snapshot={'owner':{'series_id':2},'item':{'save_path':'/data/downloads'},'files':[{'name':self.source.name,'size':len(b'original media')}]}
        self.c.execute("INSERT INTO torrent_seed_reviews VALUES(1,?,'Show','done','blocked',NULL,1,'attempt',?)",(HASH,json.dumps(snapshot)));self.c.commit()
        Path(str(self.args.db)+'.library.lock').touch()
        self.native=lambda c,a:(set(),set(),set(),set())

    def plan(self):return tool.preview(self.args,self.native)

    def run_plan(self,report,restore=False,inventory=None):
        with patch.object(tool,'stopped_lease',lambda args:contextlib.nullcontext()):
            return tool.execute(self.args,report,restore,inventory or self.native)

    def test_preview_changes_no_source_or_database(self):
        before=list(self.c.iterdump());report=self.plan()
        self.assertEqual(report['entries'][0]['status'],'planned')
        self.assertTrue(self.source.exists());self.assertFalse(self.args.quarantine_root.exists())
        self.assertEqual(before,list(self.c.iterdump()))

    def test_quarantine_restore_preserves_bytes_inode_block_and_library(self):
        before=tool.identity(self.source);db=list(self.c.iterdump());report=self.run_plan(self.plan())
        entry=report['entries'][0]
        self.assertFalse(self.source.exists());self.assertEqual(tool.identity(entry['quarantine_path']),before)
        self.assertEqual(self.backup.read_bytes(),b'library copy')
        self.run_plan(report,True)
        self.assertEqual(tool.identity(self.source),before);self.assertEqual(list(self.c.iterdump()),db)
        self.assertEqual(report['entries'][0]['status'],'restored')

    def test_active_job_of_any_category_protects_file(self):
        refs=lambda c,a:({self.source},set(),set(),set())
        report=tool.preview(self.args,refs)
        self.assertEqual(report['entries'][0]['status'],'held');self.assertTrue(self.source.exists())

    def test_new_native_reference_between_preview_and_apply_holds_file(self):
        report=self.plan();refs=lambda c,a:({self.source},set(),set(),set())
        self.run_plan(report,inventory=refs)
        self.assertEqual(report['entries'][0]['status'],'held');self.assertTrue(self.source.exists())

    def test_current_library_symlink_and_hardlink_protect_source(self):
        link=self.backup.parent/'pointer.mkv';link.symlink_to(self.source)
        self.assertEqual(self.plan()['entries'][0]['status'],'held');link.unlink()
        link.hardlink_to(self.source)
        self.assertEqual(self.plan()['entries'][0]['status'],'held')

    def test_changed_file_symlink_and_existing_restore_target_never_overwrite(self):
        report=self.plan();self.source.write_bytes(b'changed media!')
        self.run_plan(report);self.assertEqual(report['entries'][0]['status'],'held')
        self.source.unlink();self.source.symlink_to(self.backup)
        self.assertEqual(self.plan()['entries'][0]['status'],'held')
        self.source.unlink();self.source.write_bytes(b'original media');report=self.run_plan(self.plan())
        self.source.write_bytes(b'new wanted file');self.run_plan(report,True)
        self.assertEqual(self.source.read_bytes(),b'new wanted file')
        self.assertEqual(Path(report['entries'][0]['quarantine_path']).read_bytes(),b'original media')

    def test_lost_receipt_after_move_and_after_restore_are_reconciled(self):
        report=self.plan();entry=report['entries'][0]
        entry['status']='moving';tool.move_entry(entry)
        entry['status']='held'  # e.g. fsync failed after the rename
        self.run_plan(report,True)
        self.assertEqual(entry['status'],'restored');self.assertTrue(self.source.exists())
        report=self.run_plan(self.plan());entry=report['entries'][0]
        tool.move_entry(entry,True);entry['status']='restoring'
        self.run_plan(report,True)
        self.assertEqual(entry['status'],'restored');self.assertTrue(self.source.exists())

    def test_restore_keeps_quarantine_files_with_new_references(self):
        report=self.run_plan(self.plan());entry=report['entries'][0]
        link=self.backup.parent/'quarantine-pointer.mkv';link.symlink_to(entry['quarantine_path'])
        self.run_plan(report,True)
        self.assertEqual(entry['status'],'held');self.assertFalse(self.source.exists())
        self.assertEqual(link.read_bytes(),b'original media')

    def test_post_rename_fsync_failure_reconciles_only_after_durable_flush(self):
        for restore in (False,True):
            report=self.plan()
            if restore:self.run_plan(report)
            entry=report['entries'][0]
            with patch.object(tool,'sync_move_parents',side_effect=OSError('fixture fsync failed')):
                self.run_plan(report,restore)
                self.assertEqual(entry['status'],'held')
                # Retrying with an unavailable fsync must never stamp success.
                self.run_plan(report,restore)
                self.assertEqual(entry['status'],'held')
            with patch.object(tool,'sync_move_parents',wraps=tool.sync_move_parents) as flush:
                self.run_plan(report,restore)
                self.assertTrue(flush.called)
            self.assertEqual(entry['status'],'restored' if restore else 'quarantined')
            if not restore:self.run_plan(report,True)

    def test_duplicate_paths_unbound_batch_and_other_quarantine_roots_rejected(self):
        for change in ('duplicate','destination','batch','root'):
            report=self.plan()
            if change=='duplicate':report['entries'].append(copy.deepcopy(report['entries'][0]))
            elif change=='destination':report['entries'][0]['quarantine_path']=str(self.args.quarantine_root/'elsewhere')
            elif change=='batch':report['batch']='different'
            else:report['quarantine_root']=str(self.args.source_root/'bin')
            with self.assertRaises(ValueError):self.run_plan(report)
            self.assertTrue(self.source.exists())

    def test_protected_tree_read_errors_fail_closed(self):
        real=tool.os.walk
        def unreadable(root,**kwargs):
            kwargs['onerror'](PermissionError('fixture unreadable subtree'))
            yield from real(root,**kwargs)
        with patch.object(tool.os,'walk',unreadable):
            with self.assertRaises(PermissionError):self.plan()
        self.assertTrue(self.source.exists())

    def test_container_absolute_and_dangling_library_links_are_protected(self):
        link=self.backup.parent/'pointer.mkv';link.symlink_to('/data/downloads/'+self.source.name)
        self.assertEqual(self.plan()['entries'][0]['status'],'held');link.unlink()
        link.symlink_to('/data/downloads/missing.mkv')
        self.assertEqual(self.plan()['entries'][0]['status'],'planned')

    def test_native_inventory_reads_all_categories_and_hidden_usenet_history(self):
        self.c.execute('ALTER TABLE download_clients ADD COLUMN kind')
        self.c.execute('ALTER TABLE download_clients ADD COLUMN url')
        self.c.execute('ALTER TABLE download_clients ADD COLUMN username')
        self.c.execute('ALTER TABLE download_clients ADD COLUMN password')
        self.c.execute("INSERT INTO download_clients VALUES(1,0,'qbittorrent','http://fixture:8080','user','secret')")
        self.c.execute("INSERT INTO download_clients VALUES(2,0,'nzbget','http://fixture:6789','user','secret')");self.c.commit()
        calls=[]
        def response(opener,request):
            url=request if isinstance(request,str) else request.full_url;calls.append(url)
            if url.endswith('/torrents/info'):return [{'hash':'b'*40,'save_path':'/data/downloads','category':'other'}]
            if '/torrents/files?' in url:return [{'name':self.source.name,'priority':0}]
            payload=json.loads(request.data);calls.append(payload)
            return {'result':[{'DestDir':'/data/downloads/hidden'}] if payload['method']=='history' else []}
        with tool.read_db(self.args) as c, patch.object(tool,'request_json',response), patch.object(tool.urllib.request,'build_opener'):
            paths,covers,inodes,hashes=tool.native_inventory(c,self.args)
        self.assertIn(self.source,paths);self.assertIn(self.args.source_root/'hidden',covers)
        self.assertIn({'method':'history','params':[True],'id':1},calls)
        self.assertTrue(any(isinstance(x,str) and x.endswith('/torrents/info') for x in calls))

    def test_missing_files_are_not_counted_as_quarantined(self):
        self.source.unlink();report=self.plan()
        self.assertEqual(report['entries'][0]['status'],'absent')

    def test_stopped_guard_rejects_running_app_and_dangling_marker(self):
        with patch.object(tool.subprocess,'check_output',return_value=b'[{"State":{"Running":true}}]'):
            with self.assertRaises(ValueError):
                with tool.stopped_lease(self.args):pass
        marker=Path(str(self.args.db)+'.library-migration.json');marker.symlink_to(marker.parent/'missing')
        with patch.object(tool.subprocess,'check_output',return_value=b'[{"State":{"Running":false}}]'):
            with self.assertRaises(ValueError):
                with tool.stopped_lease(self.args):pass

    def test_source_block_or_owner_change_prevents_movement(self):
        report=self.plan();self.c.execute('UPDATE torrent_seed_reviews SET unblocked_at=\'explicit\'');self.c.commit()
        self.run_plan(report);self.assertTrue(self.source.exists())
        self.assertEqual(report['entries'][0]['status'],'held')


if __name__=='__main__':unittest.main()
