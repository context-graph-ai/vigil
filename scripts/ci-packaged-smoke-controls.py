#!/usr/bin/env python3
"""Offline unit controls. Docker is mocked; fixture bytes are never executed."""
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('packaged_smoke', Path(__file__).with_name('ci-packaged-smoke.py'))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class PackagedSmokeControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='vigil-smoke-controls-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / 'vigil'
        self.binary.write_bytes(b'non-executable unit fixture, never a real Vigil binary\n')
        self.binary.chmod(0o755)
        self.archive = self.root / 'package.tar.gz'
        self.receipt = self.root / 'receipt.json'
        self.runs = []

    def docker(self, args, **kwargs):
        if args[:2] == ['docker', 'cp']:
            Path(args[-1]).write_bytes(self.binary.read_bytes())
            return subprocess.CompletedProcess(args, 0, '', '')
        if args[:2] == ['docker', 'rm']:
            return subprocess.CompletedProcess(args, 0, '', '')
        self.runs.append(args)
        self.assertEqual(args[:3], ['docker', 'run', '--rm'])
        for flag in ('--pull=never', '--network=none', '--read-only', '--cap-drop=ALL',
                     '--security-opt=no-new-privileges'):
            self.assertIn(flag, args)
        self.assertIn('65534:65534', args)
        self.assertIn('sha256:' + 'a' * 64, args)
        self.assertEqual(args[-4:-1], ['-i', 'PATH=/usr/local/bin:/usr/bin:/bin', '/smoke/vigil'])
        self.assertTrue(kwargs['check'])
        self.assertEqual(kwargs['timeout'], 30)
        mount = args[args.index('--mount') + 1]
        source = Path(mount.split('src=', 1)[1].split(',')[0])
        self.assertEqual(source.read_bytes(), self.binary.read_bytes())
        self.assertTrue(mount.endswith(',dst=/smoke/vigil,readonly'))
        self.assertIn(args[-1], ('--help', '--version'))
        output = 'Commands:\nOptions:\n --help\n --version\n' if args[-1] == '--help' else 'vigil unit-fixture\n'
        return subprocess.CompletedProcess(args, 0, output, '')

    def invoke(self, run=None):
        with patch.object(smoke.subprocess, 'check_output', side_effect=['sha256:' + 'a' * 64 + '\n', 'fixture-container']), \
                patch.object(smoke.subprocess, 'run', side_effect=run or self.docker):
            smoke.smoke(self.binary, self.archive, 'builder:local', self.receipt)

    def test_exact_archive_and_both_supported_commands(self):
        self.invoke()
        self.assertEqual([args[-1] for args in self.runs], ['--help', '--version'])
        receipt = json.loads(self.receipt.read_text())
        self.assertEqual(receipt['binary_sha256'], smoke.digest(self.binary))
        self.assertEqual(receipt['archive_sha256'], smoke.digest(self.archive))
        self.assertEqual(receipt['runtime_image_id'], 'sha256:' + 'a' * 64)
        with tarfile.open(self.archive) as package:
            self.assertEqual(package.getnames(), ['binary/vigil'])
            self.assertEqual(package.extractfile('binary/vigil').read(), self.binary.read_bytes())

    def test_failure_timeout_and_wrong_output_never_leave_success_receipt(self):
        for error in (subprocess.CalledProcessError(127, ['docker']),
                      subprocess.TimeoutExpired(['docker'], 30),
                      ValueError('no local runtime')):
            self.receipt.write_text('stale success')
            def fail_cli(args, **kwargs):
                if args[:2] == ['docker', 'run']:
                    raise error
                return self.docker(args, **kwargs)
            with self.assertRaises(type(error)):
                self.invoke(run=fail_cli)
            self.assertFalse(self.receipt.exists())
        def wrong_output(args, **kwargs):
            if args[:2] == ['docker', 'run']:
                return subprocess.CompletedProcess(args, 0, 'wrong output', '')
            return self.docker(args, **kwargs)
        with self.assertRaises(ValueError):
            self.invoke(run=wrong_output)
        self.assertFalse(self.receipt.exists())

    def test_mismatched_builder_refuses_before_cli(self):
        def mismatched(args, **kwargs):
            if args[:2] == ['docker', 'cp']:
                Path(args[-1]).write_bytes(b'different builder binary')
                return subprocess.CompletedProcess(args, 0, '', '')
            return self.docker(args, **kwargs)
        with self.assertRaisesRegex(ValueError, 'builder does not match'):
            self.invoke(run=mismatched)
        self.assertFalse(self.receipt.exists())
        self.assertFalse(self.runs)

    def test_missing_nonexecutable_and_symlink_binary_refuse(self):
        self.binary.chmod(0o644)
        with self.assertRaises(ValueError):
            self.invoke()
        self.binary.unlink()
        with self.assertRaises(ValueError):
            self.invoke()
        target = self.root / 'target'
        target.write_text('fixture')
        self.binary.symlink_to(target)
        with self.assertRaises(ValueError):
            self.invoke()
        self.assertFalse(self.runs)

    def test_archive_refuses_wrong_path_link_missing_duplicate_and_changed_bytes(self):
        for names in ([], ['../vigil'], ['binary/vigil', 'binary/vigil'], ['binary/vigil']):
            with tarfile.open(self.archive, 'w:gz') as package:
                for name in names:
                    member = tarfile.TarInfo(name)
                    member.mode = 0o755
                    member.size = 5
                    package.addfile(member, io.BytesIO(b'wrong'))
            with self.assertRaises(ValueError):
                smoke.unpack(self.archive, self.root, smoke.digest(self.binary))
        with tarfile.open(self.archive, 'w:gz') as package:
            member = tarfile.TarInfo('binary/vigil')
            member.type = tarfile.SYMTYPE
            member.linkname = '/bin/sh'
            member.mode = 0o755
            package.addfile(member)
        with self.assertRaises(ValueError):
            smoke.unpack(self.archive, self.root, smoke.digest(self.binary))


if __name__ == '__main__':
    unittest.main(verbosity=2)
