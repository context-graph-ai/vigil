#!/usr/bin/env python3
"""Execute the production publisher against a stateful, offline release service."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import unquote

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('publisher', Path(__file__).with_name('ci-development.py'))
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)
SOURCE = 'a' * 40


class Interrupted(Exception):
    pass


class Service:
    def __init__(self):
        self.assets = {}
        self.next_id = 1
        self.operations = []
        self.reads = []
        self.cut = None
        self.corrupt = False
        self.stale_at = None
        self.tips = 0
        self.release = dict(id=1, tag_name='dev-latest', draft=False, prerelease=True)

    def pages(self, path, key=None):
        if path.endswith('/releases'):
            return [copy.deepcopy(self.release)]
        return [dict(id=i, name=a['name'], created_at=a['created_at'], digest='sha256:' + p.digest(a['bytes']), size=len(a['bytes']))
                for i, a in self.assets.items()]

    def add(self, name, content, created='2026-10-01'):
        i = self.next_id
        self.next_id += 1
        self.assets[i] = dict(name=name, bytes=content, created_at=created)
        return dict(id=i, name=name, digest='sha256:' + p.digest(content), size=len(content))

    def request(self, path, method='GET', data=None, binary=False, **kwargs):
        if '/git/ref/' in path:
            self.tips += 1
            return {'object': {'sha': 'f' * 40 if self.stale_at and self.tips >= self.stale_at else SOURCE}}
        if method == 'GET' and '/releases/assets/' in path:
            i = int(path.rsplit('/', 1)[1])
            self.reads.append(i)
            return b'corrupt actual remote bytes' if self.corrupt else self.assets[i]['bytes']
        if method == 'DELETE':
            i = int(path.rsplit('/', 1)[1]); name = self.assets[i]['name']
            del self.assets[i]
        elif method == 'POST':
            name = unquote(path.split('?name=')[1])
            if any(a['name'] == name for a in self.assets.values()):
                raise AssertionError('upload clobber')
            result = self.add(name, data, '2026-10-15')
        elif method == 'PATCH':
            name = 'notes'; self.release.update(data)
        else:
            raise AssertionError((path, method))
        self.operations.append((method, name))
        if self.cut == len(self.operations):
            raise Interrupted()
        return result if method == 'POST' else None

    def contents(self):
        return {a['name']: a['bytes'] for a in self.assets.values()}


def complete(contents, archive):
    try:
        data = json.loads(contents[archive + '.json'])
        return (p.digest(contents[archive]) == data['archive_sha256']
                and contents[archive + '.sha256'] == f"{data['archive_sha256']}  {archive}\n".encode())
    except (KeyError, ValueError):
        return False


class PublisherControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = patch.dict(os.environ, GITHUB_REPOSITORY=p.REPO, GITHUB_REF='refs/heads/dev',
                              GITHUB_EVENT_NAME='push', GITHUB_SHA=SOURCE, GITHUB_RUN_ID='10', GITHUB_RUN_ATTEMPT='1')
        self.env.start(); self.addCleanup(self.env.stop)
        self.expected = dict(source=SOURCE, context_graph='b'*40, contextdb='c'*40, version='1.2.3')
        self.data = dict(self.expected, target=p.TARGET, toolchain='rustc fixture', archive_sha256=p.digest(b'archive'))
        self.write()
        self.api = Service()
        self.api.add(p.ARCHIVE, b'old archive')
        h = p.digest(b'old archive')
        self.api.add(p.ARCHIVE+'.sha256', f'{h}  {p.ARCHIVE}\n'.encode())
        self.api.add(p.ARCHIVE+'.json', json.dumps(dict(self.data, archive_sha256=h, run_id=1, run_attempt=1)).encode())
        self.previous = self.api.contents()

    def write(self):
        (self.root/p.ARCHIVE).write_bytes(b'archive')
        # Builder receipts include the run ID and attempt used by the publisher.
        (self.root/'delivery.json').write_text(json.dumps(dict(self.data, run_id=10, run_attempt=int(os.environ['GITHUB_RUN_ATTEMPT']))))

    def run_publish(self, api=None):
        return p.publish(api or self.api, self.root, self.expected)

    def test_actual_readback_and_write_once_collision(self):
        self.run_publish()
        self.assertGreaterEqual(len(self.api.reads), 6)
        before = copy.deepcopy(self.api.assets); mutations = len(self.api.operations)
        with self.assertRaisesRegex(ValueError, 'exists|collision'):
            self.run_publish()
        self.assertEqual(self.api.assets, before)
        self.assertEqual(len(self.api.operations), mutations)

    def test_remote_bytes_mismatch_rejects_before_stable_mutation(self):
        self.api.corrupt = True
        with self.assertRaises(ValueError):
            self.run_publish()
        self.assertFalse(any(method == 'DELETE' or method == 'PATCH' for method, _ in self.api.operations))
        for name, content in self.previous.items():
            self.assertEqual(self.api.contents()[name], content)

    def test_receipt_only_new_attempt_preserves_previous_immutable_group(self):
        self.run_publish()
        old = {n: b for n,b in self.api.contents().items() if '-r10a1' in n}
        self.assertEqual(len(old), 3)
        os.environ['GITHUB_RUN_ATTEMPT'] = '2'; self.write(); self.run_publish()
        self.assertTrue(all(self.api.contents()[n] == b for n,b in old.items()))
        self.assertEqual(json.loads(self.api.contents()[p.ARCHIVE+'.json'])['run_attempt'], 2)
        self.assertEqual(len([n for n in self.api.contents() if '-r10a2' in n]), 3)

    def test_interrupt_after_every_upload_delete_and_notes_preserves_download(self):
        baseline = copy.deepcopy(self.api)
        self.run_publish()
        count = len(self.api.operations)
        self.assertGreaterEqual(count, 10)
        for cut in range(1, count+1):
            api = copy.deepcopy(baseline); api.cut = cut
            with self.assertRaises(Interrupted):
                self.run_publish(api)
            content = api.contents()
            survivors = [n for n in content if n.endswith('.tar.gz') and complete(content, n)]
            self.assertTrue(survivors, (cut, api.operations))
            # Before a complete new fallback exists, the previous stable bytes survive.
            if not any('-r10a1' in n for n in survivors):
                self.assertTrue(all(content.get(n) == b for n,b in self.previous.items()))

    def test_retention_protects_stable_receipt_and_incomplete_groups(self):
        for run in range(1, 7):
            os.environ['GITHUB_RUN_ID'] = str(run)
            self.write()
            d=json.loads((self.root/'delivery.json').read_text()); d['run_id']=run
            (self.root/'delivery.json').write_text(json.dumps(d))
            self.run_publish()
        names = self.api.contents()
        groups = [n for n in names if re_run(n) and n.endswith('.tar.gz')]
        self.assertEqual(len(groups), 3)
        current = names[p.ARCHIVE+'.json']
        self.assertTrue(any(names.get(n+'.json') == current for n in groups))
        incomplete = f'vigil-haos-production-amd64-{SOURCE[:12]}-r1a99.tar.gz'
        self.api.add(incomplete, b'incomplete')
        # An older publication with the stable receipt must survive pruning too.
        protected = f'vigil-haos-production-amd64-{SOURCE[:12]}-r1a98.tar.gz'
        self.api.add(protected, b'archive', '2000')
        self.api.add(protected+'.sha256', f'{p.digest(b"archive")}  {protected}\n'.encode(), '2000')
        # Predict the next stable receipt (run 7).
        os.environ['GITHUB_RUN_ID']='7'; self.write()
        d=json.loads((self.root/'delivery.json').read_text());d['run_id']=7
        (self.root/'delivery.json').write_text(json.dumps(d))
        protected_receipt=(json.dumps(d,sort_keys=True,indent=2)+'\n').encode()
        self.api.add(protected+'.json', protected_receipt, '2000')
        self.run_publish()
        self.assertIn(incomplete, self.api.contents())
        self.assertIn(protected+'.json', self.api.contents())
        for method,name in self.api.operations:
            if method == 'DELETE' and re_run(name):
                self.assertNotIn('-r1a98', name)

    def test_stale_tip_never_mutates_stable(self):
        self.api.stale_at = 2
        self.run_publish()
        self.assertFalse(any(m == 'DELETE' or m == 'PATCH' for m,n in self.api.operations))


def re_run(name):
    return '-r' in name and 'a' in name


if __name__ == '__main__':
    unittest.main(verbosity=2)
