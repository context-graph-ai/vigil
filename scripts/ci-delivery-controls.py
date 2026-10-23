#!/usr/bin/env python3
"""Offline API-state controls for the production delivery helper; no network."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('delivery', ROOT / 'scripts/ci-development.py')
d = importlib.util.module_from_spec(spec)
spec.loader.exec_module(d)
SOURCE = 'a' * 40
TUPLE = {'context_graph': 'b' * 40, 'contextdb': 'c' * 40}
ENV = dict(GITHUB_REPOSITORY=d.REPO, GITHUB_REF='refs/heads/dev', GITHUB_SHA=SOURCE,
           GITHUB_EVENT_NAME='push', GITHUB_RUN_ID='123', GITHUB_RUN_ATTEMPT='2')
NAMES = ['Portable CI checks (not full qualification)', 'Validate public exact dependency tuple',
         'Default change gate / Default change gate', 'Change qualification receipt']


class FakeAPI:
    def __init__(self):
        self.calls = []
        self.releases = []
        self.assets = {}
        self.payloads = {}
        self.next_id = 1
        self.tip = SOURCE
        self.default = 'main'
        self.fail_upload = None
        self.move_after = None
        self.uploads = 0
        self.run = dict(id=123, run_attempt=2, head_sha=SOURCE, event='push', head_branch='dev',
                        path='.github/workflows/ci.yml', head_repository={'full_name': d.REPO},
                        status='in_progress', conclusion=None)
        self.jobs = [dict(name=n, status='completed', conclusion='success') for n in NAMES]
        self.jobs.append(dict(name='development-binaries / metadata', status='in_progress', conclusion=None))

    def request(self, path, method='GET', data=None, binary=False, raw=False):
        self.calls.append((method, path, copy.deepcopy(data)))
        if raw:
            return self.payloads[int(path.rsplit('/', 1)[-1])]
        if path.endswith('/git/ref/heads/dev'):
            return {'object': {'sha': self.tip}}
        if path == '/repos/' + d.REPO:
            return {'default_branch': self.default, 'private': True}
        if '/git/ref/heads/' in path:
            return {'object': {'sha': 'd' * 40}}
        if '/actions/runs/' in path:
            return copy.deepcopy(self.run)
        if path.endswith('/releases') and method == 'POST':
            self.releases.append(dict(id=8, **data))
            return copy.deepcopy(self.releases[-1])
        if method == 'DELETE':
            target = int(path.rsplit('/', 1)[-1])
            for name, asset in list(self.assets.items()):
                if asset['id'] == target:
                    del self.assets[name]
                    return
            raise AssertionError('invalid deletion')
        if '/assets?name=' in path and method == 'POST':
            import urllib.parse
            name = urllib.parse.unquote(path.split('?name=')[1])
            if name in self.assets:
                raise ValueError('asset collision')
            self.uploads += 1
            if self.fail_upload == self.uploads:
                raise OSError('simulated interrupted upload')
            asset = dict(id=self.next_id, name=name, size=len(data), digest='sha256:' + d.digest(data),
                         created_at=f'2026-10-14T00:00:{self.next_id:02d}Z')
            self.payloads[self.next_id] = data
            self.next_id += 1
            self.assets[name] = asset
            if self.move_after == self.uploads:
                self.tip = 'e' * 40
            return copy.deepcopy(asset)
        if '/releases/' in path and method == 'PATCH':
            self.releases[0].update(data)
            return copy.deepcopy(self.releases[0])
        raise AssertionError((path, method))

    def pages(self, path, key=None):
        self.calls.append(('PAGES', path, None))
        if '/jobs' in path:
            return copy.deepcopy(self.jobs)
        if '/runs?' in path:
            return [copy.deepcopy(self.run)]
        if path.endswith('/assets'):
            return list(copy.deepcopy(self.assets).values())
        if path.endswith('/releases'):
            return copy.deepcopy(self.releases)
        raise AssertionError(path)


class DeliveryControls(unittest.TestCase):
    def setUp(self):
        self.environment = patch.dict(os.environ, ENV)
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.temporary = tempfile.TemporaryDirectory(prefix='vigil-delivery-controls-')
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.api = FakeAPI()
        self.data = dict(source=SOURCE, **TUPLE, version='0.1.0', toolchain='rustc 1.94.0 (fixture)',
                         target=d.TARGET, run_id=123, run_attempt=2, archive_sha256=d.digest(b'opaque archive fixture, never executed'))
        (self.directory / d.ARCHIVE).write_bytes(b'opaque archive fixture, never executed')
        self.write()
        self.expected = dict(source=SOURCE, **TUPLE, version='0.1.0')

    def write(self):
        (self.directory / 'delivery.json').write_text(json.dumps(self.data))

    def publish(self):
        return d.publish(self.api, self.directory, self.expected)

    def test_current_running_attempt_and_successful_aggregate_no_overall_wait(self):
        d.correctness(self.api, self.api.run, SOURCE, True)
        self.assertIn('/attempts/2/jobs', self.api.calls[-1][1])
        for key, value in [('id', 124), ('run_attempt', 1), ('head_sha', 'd' * 40),
                           ('event', 'pull_request'), ('head_branch', 'main'), ('status', 'completed')]:
            bad = dict(self.api.run, **{key: value})
            with self.subTest(key=key), self.assertRaises(ValueError):
                d.correctness(self.api, bad, SOURCE, True)
        for status in ['failure', 'cancelled', 'skipped', None, 'neutral']:
            self.api.jobs[-2]['conclusion'] = status
            with self.subTest(status=status), self.assertRaises(ValueError):
                d.correctness(self.api, self.api.run, SOURCE, True)
        self.api.jobs = self.api.jobs[:-2]
        with self.assertRaises(ValueError):
            d.correctness(self.api, self.api.run, SOURCE, True)

    def test_manual_retry_allows_only_settled_correctness_with_delivery_failure(self):
        run = dict(self.api.run, status='completed', conclusion='failure')
        self.api.jobs[-1].update(status='completed', conclusion='failure')
        d.correctness(self.api, run, SOURCE)
        self.api.jobs[0]['conclusion'] = 'failure'
        with self.assertRaises(ValueError):
            d.correctness(self.api, run, SOURCE)
        self.api.jobs[0]['conclusion'] = 'success'
        for change in [{'conclusion': 'cancelled'}, {'head_sha': 'f' * 40}, {'status': 'in_progress'}]:
            with self.assertRaises(ValueError):
                d.correctness(self.api, dict(run, **change), SOURCE)

    def test_empty_committed_pin_refuses_before_any_api(self):
        (self.directory / 'scripts').mkdir()
        (self.directory / 'scripts/ci-dependency-pins.json').write_text(json.dumps(dict(TUPLE, context_graph='')))
        with patch('subprocess.check_output', return_value=SOURCE + '\n'):
            with self.assertRaisesRegex(ValueError, 'PENDING'):
                d.admit(self.api, self.directory)
        self.assertEqual(self.api.calls, [])

    def test_bootstrap_uses_hidden_draft_default_head_and_never_moves_tag(self):
        for default in ('main', 'dev'):
            self.api = FakeAPI()
            self.api.default = default
            self.publish()
            created = next(c for c in self.api.calls if c[0] == 'POST' and c[1].endswith('/releases'))
            self.assertEqual(created[2]['target_commitish'], SOURCE if default == 'dev' else 'd'*40)
            self.assertTrue(created[2]['draft'])
            self.assertTrue(created[2]['prerelease'])
            self.assertEqual(created[2]['make_latest'], 'false')
            self.assertFalse(self.api.releases[0]['draft'])
            self.assertFalse(any('/git/refs' in c[1] for c in self.api.calls))
            self.assertEqual(self.api.calls[-1][0], 'GET')
            notes = [c for c in self.api.calls if c[0] == 'PATCH']
            self.assertEqual(len(notes), 1)
            self.assertIn('-r123a2', notes[0][2]['body'])

    def test_full_admission_exact_pin_outputs_and_manual_mismatch(self):
        (self.directory / 'scripts').mkdir()
        (self.directory / 'scripts/ci-dependency-pins.json').write_text(json.dumps(TUPLE))
        output = self.directory / 'outputs'
        with patch.dict(os.environ, GITHUB_OUTPUT=str(output)), patch('subprocess.check_output', return_value=SOURCE + '\n'):
            d.admit(self.api, self.directory)
            self.assertEqual(output.read_text(), f'vigil_sha={SOURCE}\ncontext_graph_sha={TUPLE["context_graph"]}\ncontextdb_sha={TUPLE["contextdb"]}\n')
            self.api.run.update(status='completed', conclusion='failure')
            with patch.dict(os.environ, GITHUB_EVENT_NAME='workflow_dispatch', INPUT_VIGIL_SHA=SOURCE,
                            INPUT_CONTEXT_GRAPH_SHA=TUPLE['context_graph'], INPUT_CONTEXTDB_SHA=TUPLE['contextdb']):
                d.admit(self.api, self.directory)
                with patch.dict(os.environ, INPUT_CONTEXT_GRAPH_SHA='e' * 40), self.assertRaisesRegex(ValueError, 'manual tuple'):
                    d.admit(self.api, self.directory)

    def test_stale_tip_and_mid_publication_move_never_overwrite_stable(self):
        self.api.tip = 'd' * 40
        self.assertIn('Skipped', self.publish())
        self.assertFalse(self.api.releases)
        for position in range(1, 7):
            self.api = FakeAPI()
            self.api.move_after = position
            self.assertIn('Skipped', self.publish())
            self.assertTrue(self.api.releases[0]['draft'])
            stable_uploads = [c for c in self.api.calls if '?name=' + d.ARCHIVE in c[1]]
            self.assertLessEqual(len(stable_uploads), max(0, position - 3))

    def test_duplicate_releases_bad_ids_and_private_metadata_refuse(self):
        self.api.releases = [dict(id=8, tag_name='dev-latest', prerelease=True)] * 2
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            self.publish()
        self.api.releases = [dict(id='../escape', tag_name='dev-latest', prerelease=True)]
        with self.assertRaisesRegex(ValueError, 'identifier'):
            self.publish()
        self.api = FakeAPI()
        self.data['private_log_path'] = '/private/secret'
        self.write()
        with self.assertRaisesRegex(ValueError, 'allowlist'):
            self.publish()
        self.assertEqual(self.api.calls, [])

    def test_changed_archive_source_tuple_or_missing_receipt_refuses(self):
        for key, value in [('source', 'd' * 40), ('context_graph', 'd' * 40),
                           ('version', '9.0.0'), ('archive_sha256', '0' * 64), ('target', 'other')]:
            original = self.data[key]
            self.data[key] = value
            self.write()
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.publish()
            self.data[key] = original
        self.assertEqual(self.api.calls, [])

    def test_real_api_uses_authenticated_fixed_hosts_and_rejects_cross_repo(self):
        from unittest.mock import MagicMock
        api = d.API()
        response = MagicMock()
        response.__enter__.return_value.read.return_value = b'[]'
        with patch.dict(os.environ, GH_TOKEN='offline-fixture-token'), patch('urllib.request.urlopen', return_value=response) as request:
            api.request('/repos/' + d.REPO + '/releases')
            sent = request.call_args[0][0]
            self.assertEqual(sent.full_url, 'https://api.github.com/repos/' + d.REPO + '/releases')
            self.assertEqual(sent.get_header('Authorization'), 'Bearer offline-fixture-token')
            self.assertEqual(request.call_args[1]['timeout'], 60)
            with self.assertRaises(ValueError):
                api.request('/repos/outsider/vigil/releases')
            self.assertEqual(request.call_count, 1)

    def test_receipt_generator_exports_only_allowlist_and_binds_smoked_archive(self):
        (self.directory / 'scripts').mkdir()
        (self.directory / 'scripts/ci-dependency-pins.json').write_text(json.dumps(TUPLE))
        (self.directory / 'Cargo.toml').write_text('[workspace.package]\nversion = "0.1.0"\n')
        (self.directory / 'receipts').mkdir()
        (self.directory / 'receipts/dev-haos-artifact-smoke.json').write_text(json.dumps({
            'archive_sha256': self.data['archive_sha256'], 'private_log_path': '/private/log'}))
        d.receipt(self.directory, self.directory, 'rustc 1.94.0 (fixture)')
        result = json.loads((self.directory / 'delivery.json').read_text())
        self.assertEqual(set(result), d.FIELDS)
        self.assertEqual(result, self.data)
        with patch.dict(os.environ, GITHUB_REF='refs/heads/dev-next', GITHUB_EVENT_NAME='workflow_dispatch'):
            d.receipt(self.directory, self.directory, 'rustc 1.94.0 (fixture)')
            with self.assertRaises(ValueError):
                self.publish()
        (self.directory / d.ARCHIVE).write_bytes(b'changed archive')
        with self.assertRaisesRegex(ValueError, 'smoked archive'):
            d.receipt(self.directory, self.directory, 'rustc 1.94.0 (fixture)')

    def test_lint_adapter_exact_placement_mutations_keep_other_bytes(self):
        spec = importlib.util.spec_from_file_location('workflow_lint', ROOT / 'scripts/ci-workflow-lint.py')
        lint = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(lint)
        text = (ROOT / '.github/workflows/build.yml').read_text()
        path = self.directory / 'build.yml'
        path.write_text(text)
        self.assertEqual(lint.compatible(path), text.replace('      queue: max\n', '', 1))
        for mutated in [text.replace('queue: max', 'queue: single'),
                        text.replace('      queue: max\n', '      queue: max\n      unknown: true\n'),
                        text.replace('      queue: max\n', '') + 'queue: max\n',
                        text + 'unexpected:\n  queue: max\n']:
            path.write_text(mutated)
            with self.subTest(mutated=mutated[-80:]), self.assertRaises(ValueError):
                lint.compatible(path)
        path = self.directory / 'other.yml'
        path.write_text('name: other\nqueue: max\n')
        with self.assertRaises(ValueError):
            lint.compatible(path)

    def test_authenticated_pagination_includes_drafts_and_all_pages(self):
        api = d.API()
        with patch.object(api, 'request', side_effect=[[{'draft': True}] * 100, [{'draft': False}]]) as request:
            rows = api.pages('/repos/' + d.REPO + '/releases')
        self.assertEqual(len(rows), 101)
        self.assertIn('page=2', request.call_args[0][0])


def load_tests(loader, tests, pattern):
    spec = importlib.util.spec_from_file_location('delivery_state_controls', ROOT / 'scripts/ci-delivery-state-controls.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    tests.addTests(loader.loadTestsFromModule(module))
    return tests


if __name__ == '__main__':
    unittest.main(verbosity=2)
