"""诊断工具的路径拒绝与旁证保护自检；不编译或启动服务。"""
from pathlib import Path
import importlib.util
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
spec = importlib.util.spec_from_file_location('diagnostic_memory', HERE / 'memory.py')
diagnostic_memory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diagnostic_memory)


class SafetyChecks(unittest.TestCase):
    def reject(self, script, *args):
        result = subprocess.run([sys.executable, str(HERE / script), *map(str, args)],
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('仓库外', result.stderr)

    def test_builder_rejects_repository_path(self):
        with tempfile.TemporaryDirectory(dir=REPO) as tmp:
            target = Path(tmp) / 'new'
            self.reject('build.py', '--work-dir', target)
            self.assertFalse(target.exists())

    def test_builder_preserves_existing_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = Path(tmp) / 'keep'
            marker.write_bytes(b'confirmed')
            self.reject('build.py', '--work-dir', tmp)
            self.assertEqual(marker.read_bytes(), b'confirmed')
            self.assertEqual(list(Path(tmp).iterdir()), [marker])

    def test_memory_rejects_repository_path(self):
        with tempfile.TemporaryDirectory(dir=REPO) as tmp:
            target = Path(tmp) / 'new.db'
            self.reject('memory.py', '--database', target, '--mode', 'default')
            self.assertFalse(target.exists())

    def test_builder_rejects_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / 'existing'
            target.mkdir()
            link = Path(tmp) / 'alias'
            link.symlink_to(target)
            self.reject('build.py', '--work-dir', link)
            self.assertTrue(link.is_symlink())
            self.assertEqual(list(target.iterdir()), [])

    def test_memory_preserves_existing_outputs(self):
        for kind in ('file', 'directory', 'symlink', 'dangling-symlink'):
            for output in ('database', 'log'):
                with self.subTest(kind=kind, output=output), tempfile.TemporaryDirectory() as tmp:
                    database = Path(tmp) / 'new.db'
                    log = database.with_suffix('.server.log')
                    existing = database if output == 'database' else log
                    target = Path(tmp) / 'confirmed'
                    marker = b'confirmed evidence must be preserved'
                    if kind == 'file':
                        existing.write_bytes(marker)
                    elif kind == 'directory':
                        existing.mkdir()
                        (existing / 'confirmed').write_bytes(marker)
                    else:
                        if kind == 'symlink':
                            target.write_bytes(marker)
                        existing.symlink_to(target)
                    before = sorted(Path(tmp).iterdir())
                    argv = ['memory.py', '--database', str(database), '--mode', 'default']
                    with patch.object(sys, 'argv', argv), patch.object(diagnostic_memory.subprocess, 'Popen') as popen:
                        with self.assertRaisesRegex(ValueError, '仓库外'):
                            diagnostic_memory.main()
                        popen.assert_not_called()
                    self.assertEqual(sorted(Path(tmp).iterdir()), before)
                    if kind == 'file':
                        self.assertEqual(existing.read_bytes(), marker)
                    elif kind == 'directory':
                        self.assertEqual((existing / 'confirmed').read_bytes(), marker)
                    else:
                        self.assertTrue(existing.is_symlink())
                        self.assertEqual(existing.readlink(), target)
                        if kind == 'symlink':
                            self.assertEqual(target.read_bytes(), marker)
                        else:
                            self.assertFalse(target.exists())

    def test_memory_rejects_log_alias_of_database(self):
        with tempfile.TemporaryDirectory() as tmp:
            database = Path(tmp) / 'new.db'
            log = database.with_suffix('.server.log')
            log.symlink_to(database)
            argv = ['memory.py', '--database', str(database), '--mode', 'default']
            with patch.object(sys, 'argv', argv), patch.object(diagnostic_memory.subprocess, 'Popen') as popen:
                with self.assertRaisesRegex(ValueError, '互不相同'):
                    diagnostic_memory.main()
                popen.assert_not_called()
            self.assertEqual(list(Path(tmp).iterdir()), [log])
            self.assertTrue(log.is_symlink())
            self.assertEqual(log.readlink(), database)
            self.assertFalse(database.exists())

    def test_memory_log_creation_is_exclusive(self):
        with tempfile.TemporaryDirectory() as tmp:
            database = Path(tmp) / 'new.db'
            log = database.with_suffix('.server.log')
            marker = b'created after preflight'
            original_open = Path.open

            def concurrent_create(path, mode='r', *args, **kwargs):
                if path == log:
                    with original_open(log, 'wb') as stream:
                        stream.write(marker)
                return original_open(path, mode, *args, **kwargs)

            argv = ['memory.py', '--database', str(database), '--mode', 'default']
            with patch.object(sys, 'argv', argv), patch.object(diagnostic_memory.sys, 'platform', 'darwin'), patch.object(Path, 'open', concurrent_create), patch.object(diagnostic_memory.subprocess, 'Popen') as popen:
                with self.assertRaises(FileExistsError):
                    diagnostic_memory.main()
                popen.assert_not_called()
            self.assertEqual(log.read_bytes(), marker)
            self.assertFalse(database.exists())

    def test_memory_new_log_is_closed_when_probe_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            database = Path(tmp) / 'new.db'
            log = database.with_suffix('.server.log')
            argv = ['memory.py', '--database', str(database), '--mode', 'default']

            def fail_probe(args, stream):
                stream.write('startup failure evidence\n')
                raise RuntimeError('probe failed')

            with patch.object(sys, 'argv', argv), patch.object(diagnostic_memory.sys, 'platform', 'darwin'), patch.object(diagnostic_memory, 'run_probe', side_effect=fail_probe) as probe:
                with self.assertRaisesRegex(RuntimeError, 'probe failed'):
                    diagnostic_memory.main()
            self.assertTrue(probe.call_args.args[1].closed)
            self.assertEqual(log.read_text(), 'startup failure evidence\n')
            self.assertFalse(database.exists())


if __name__ == '__main__':
    unittest.main()
