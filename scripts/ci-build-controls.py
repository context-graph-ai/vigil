#!/usr/bin/env python3
"""Offline controller controls. Requires PyYAML; runs no Rust or network commands."""
import copy
import hashlib
import json
import os
import shlex
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / '.github/workflows'


def load(text):
    # BaseLoader leaves GitHub's `on` key and input booleans unambiguous.
    return yaml.load(text, Loader=yaml.BaseLoader)


def controller_valid(caller, callee):
    # Pin the whole controller and reused recipe before semantic checks.
    digest = hashlib.sha256(json.dumps(
        caller, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()).hexdigest()
    if digest != 'e3c6b681a6399d1048aa9e2eb46d71483ada82aa5eaee146a206aaed850df4aa' or not packaged_path_valid(callee):
        return False
    inputs = callee['on']['workflow_dispatch']['inputs']
    reusable_inputs = copy.deepcopy(inputs)
    # workflow_call requires explicit forwarding: actionlint rejects a default
    # on a required reusable input. Both manual UIs retain their default.
    reusable_inputs['cache_epoch'].pop('default')
    package = caller.get('jobs', {}).get('package', {})
    expected_with = {key: '${{ needs.metadata.outputs.' + key + ' }}'
                     for key in ('vigil_sha', 'context_graph_sha', 'contextdb_sha')}
    expected_with['cache_epoch'] = "${{ inputs.cache_epoch || 'dev-haos-artifact-v1' }}"
    return (
        set(caller['on']) == {'workflow_dispatch', 'workflow_call'}
        and caller['on']['workflow_dispatch']['inputs'] == inputs
        and set(caller['on']['workflow_call']['inputs']) == set(inputs)
        and all(item == {'type': 'string', 'required': 'false'}
                for item in caller['on']['workflow_call']['inputs'].values())
        and callee['on']['workflow_call']['inputs'] == reusable_inputs
        and caller['permissions'] == {'contents': 'read'}
        and package == {
            'name': 'Exact development HAOS binary',
            'needs': 'metadata', 'permissions': {'contents': 'read'},
            'uses': './.github/workflows/dev-haos-artifact.yml', 'with': expected_with,
        }
        and set(caller['jobs']) == {'metadata', 'package', 'publish'}
        and caller['jobs']['metadata']['permissions'] == {'contents': 'read', 'actions': 'read'}
        and caller['concurrency']['cancel-in-progress'] == 'false'
        and caller['concurrency']['group'] != callee['concurrency']['group']
    )



def packaged_path_valid(callee):
    # Include root defaults/env and every build and smoke execution field.
    digest = hashlib.sha256(json.dumps(
        callee, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()).hexdigest()
    if digest != '5e0294dee973de0af3c6dd2d911f268b19932311679ab398b32c3029df06ce6a':
        return False
    steps = callee['jobs']['build']['steps']
    smoke = [i for i, step in enumerate(steps) if 'ci-packaged-smoke.py' in step.get('run', '')]
    upload = [i for i, step in enumerate(steps) if step.get('uses', '').startswith('actions/upload-artifact@')]
    if len(smoke) != 1 or len(upload) != 1 or smoke[0] >= upload[0]:
        return False
    for index in (smoke[0], upload[0]):
        if any(key in steps[index] for key in ('if', 'continue-on-error')):
            return False
    paths = steps[upload[0]]['with']['path'].split()
    return (steps[upload[0]]['with']['if-no-files-found'] == 'error'
            and 'haos-output/vigil-haos-production-amd64.tar.gz' in paths
            and 'haos-output/receipts/dev-haos-artifact-smoke.json' in paths
            and shlex.split(steps[smoke[0]]['run'].replace('\\\n', '')) == [
                'python3', 'vigil/scripts/ci-packaged-smoke.py',
                'haos-output/binary/vigil',
                'haos-output/vigil-haos-production-amd64.tar.gz',
                'vigil-haos-smoke-builder:local',
                'haos-output/receipts/dev-haos-artifact-smoke.json'])


class BuildControls(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.caller = load((WORKFLOWS / 'build.yml').read_text())
        cls.callee = load((WORKFLOWS / 'dev-haos-artifact.yml').read_text())

    def test_manual_input_parity_and_same_commit_call(self):
        self.assertTrue(controller_valid(self.caller, self.callee))
        self.assertEqual(set(self.callee['on']), {'workflow_dispatch', 'workflow_call'})

    def test_packaging_and_guard_contracts(self):
        self.assertEqual(self.callee['permissions'], {'contents': 'read'})
        self.assertEqual(self.callee['jobs']['build']['needs'], 'exact-sources')
        steps = self.callee['jobs']['build']['steps']
        recipe = next(step['run'] for step in steps if 'Build and receipt' in step.get('name', ''))
        for token in ('./scripts/verify release', '--lane artifact-amd64',
                      '--target vigil-hw-binary-amd64', '--output type=local,dest=../haos-output/binary'):
            self.assertIn(token, recipe)
        self.assertTrue(packaged_path_valid(self.callee))
        guard = (ROOT / 'xtask/src/test_estate.rs').read_text()
        self.assertIn('audit_dev_haos_artifact_workflow(&haos_artifact, violations);', guard)
        self.assertIn('audit_public_dependency_workflow(workflow, violations);', guard)
        required = guard.split('fn audit_dev_haos_artifact_workflow(', 1)[1]
        required = required.split('&[', 1)[1].split('],', 1)[0]
        text = (WORKFLOWS / 'dev-haos-artifact.yml').read_text()
        # Evaluate the existing guard's required Rust string literals, without
        # building xtask. These particular literals share JSON escape syntax.
        for line in required.splitlines():
            literal = line.strip().rstrip(',')
            if literal.startswith('"'):
                self.assertIn(json.loads(literal), text)

    def test_reject_automatic_events_and_secret_inheritance(self):
        for event in ('push', 'pull_request', 'pull_request_target', 'workflow_run', 'schedule'):
            bad = copy.deepcopy(self.caller)
            bad['on'][event] = {}
            self.assertFalse(controller_valid(bad, self.callee), event)
        for key, value in [('secrets', 'inherit'), ('if', 'false'),
                           ('continue-on-error', 'true'), ('permissions', {'contents': 'write'})]:
            bad = copy.deepcopy(self.caller)
            bad['jobs']['package'][key] = value
            self.assertFalse(controller_valid(bad, self.callee), key)

    def test_reject_missing_real_job_or_tuple_forwarding(self):
        for value in ({}, {'summary': {'runs-on': 'ubuntu-latest',
                                      'steps': [{'run': 'echo success'}]}}):
            bad = copy.deepcopy(self.caller)
            bad['jobs'] = value
            self.assertFalse(controller_valid(bad, self.callee))
        for key in self.caller['jobs']['package']['with']:
            bad = copy.deepcopy(self.caller)
            bad['jobs']['package']['with'][key] = 'old-ref'
            self.assertFalse(controller_valid(bad, self.callee), key)

    def test_reject_input_drift_write_permission_and_lock_collision(self):
        bad = copy.deepcopy(self.callee)
        bad['on']['workflow_call']['inputs']['vigil_sha']['required'] = 'false'
        self.assertFalse(controller_valid(self.caller, bad))
        bad = copy.deepcopy(self.caller)
        bad['permissions']['contents'] = 'write'
        self.assertFalse(controller_valid(bad, self.callee))
        bad = copy.deepcopy(self.caller)
        bad['concurrency']['group'] = self.callee['concurrency']['group']
        self.assertFalse(controller_valid(bad, self.callee))

    def test_reject_missing_skipped_or_late_smoke(self):
        steps = self.callee['jobs']['build']['steps']
        index = next(i for i, step in enumerate(steps) if 'ci-packaged-smoke.py' in step.get('run', ''))
        for key, value in [('if', 'false'), ('continue-on-error', 'true')]:
            bad = copy.deepcopy(self.callee)
            bad['jobs']['build']['steps'][index][key] = value
            self.assertFalse(packaged_path_valid(bad))
        bad = copy.deepcopy(self.callee)
        bad['jobs']['build']['steps'].append(bad['jobs']['build']['steps'].pop(index))
        self.assertFalse(packaged_path_valid(bad))
        bad = copy.deepcopy(self.callee)
        del bad['jobs']['build']['steps'][index]
        self.assertFalse(packaged_path_valid(bad))

    def test_reject_smoke_of_different_payload_or_summary_only(self):
        index = next(i for i, step in enumerate(self.callee['jobs']['build']['steps'])
                     if 'ci-packaged-smoke.py' in step.get('run', ''))
        for old, new in [('haos-output/binary/vigil', 'target/debug/vigil'),
                         ('python3 vigil/scripts/', 'echo python3 vigil/scripts/'),
                         ('haos-output/vigil-haos-production-amd64.tar.gz', 'other.tar.gz')]:
            bad = copy.deepcopy(self.callee)
            step = bad['jobs']['build']['steps'][index]
            step['run'] = step['run'].replace(old, new)
            self.assertFalse(packaged_path_valid(bad))

    def test_smoke_shell_failure_cannot_be_hidden(self):
        index = next(i for i,s in enumerate(self.callee['jobs']['build']['steps'])
                     if 'ci-packaged-smoke.py' in s.get('run',''))
        command = self.callee['jobs']['build']['steps'][index]['run']
        for mutation in ('if false; then\n'+command+'\nfi', command.rstrip()+' || true\n'):
            bad = copy.deepcopy(self.callee)
            bad['jobs']['build']['steps'][index]['run'] = mutation
            self.assertFalse(packaged_path_valid(bad))
        with tempfile.TemporaryDirectory() as directory:
            stub = Path(directory)/'python3'
            stub.write_text('#!/bin/sh\nexit 37\n')
            stub.chmod(0o755)
            result = subprocess.run(['bash','-euo','pipefail','-c',command],
                                    env=dict(os.environ, PATH=directory+':/usr/bin:/bin'), capture_output=True)
            self.assertNotEqual(result.returncode, 0, 'packaged smoke failure must fail the job')

    def test_actual_dispatch_gate_offline(self):
        script = self.callee['jobs']['exact-sources']['steps'][0]['run']
        sha = 'a' * 40
        environment = dict(os.environ, GITHUB_REF='refs/heads/dev', GITHUB_SHA=sha,
                           VIGIL_SHA=sha, CONTEXT_GRAPH_SHA='b' * 40,
                           CONTEXTDB_SHA='c' * 40, CACHE_EPOCH='dev-haos-artifact-v1')
        for branch in ('dev', 'dev-next'):
            good = dict(environment, GITHUB_REF='refs/heads/' + branch)
            self.assertEqual(subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                                           env=good, capture_output=True).returncode, 0)
        for change in ({'GITHUB_REF': 'refs/pull/1/merge'}, {'GITHUB_REF': 'refs/heads/main'},
                       {'GITHUB_REF': 'refs/tags/v1'}, {'VIGIL_SHA': 'dev'},
                       {'CONTEXT_GRAPH_SHA': ''}, {'CONTEXTDB_SHA': 'old'},
                       {'CACHE_EPOCH': 'unsafe token'}, {'GITHUB_SHA': 'd' * 40}):
            bad = dict(environment, **change)
            result = subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                                    env=bad, capture_output=True)
            self.assertNotEqual(result.returncode, 0, change)

    def test_actual_current_tuple_gate_offline(self):
        script = self.callee['jobs']['exact-sources']['steps'][-1]['run']
        with tempfile.TemporaryDirectory(prefix='vigil-build-controls-') as directory:
            scratch = Path(directory)
            git = scratch / 'git'
            git.write_text('''#!/bin/bash
set -eu
case "$*" in
  '-C vigil rev-parse HEAD') echo "$EXPECTED_VIGIL_SHA" ;;
  '-C vigil rev-parse origin/dev'|'-C vigil rev-parse origin/dev-next') echo "$TIP_VIGIL" ;;
  '-C context-graph rev-parse HEAD') echo "$EXPECTED_CONTEXT_GRAPH_SHA" ;;
  '-C contextdb rev-parse HEAD') echo "$EXPECTED_CONTEXTDB_SHA" ;;
  *'status --porcelain=v1') printf '%s' "${DIRTY:-}" ;;
  *) exit 90 ;;
esac
''')
            gh = scratch / 'gh'
            gh.write_text('''#!/bin/bash
set -eu
case "$*" in
  *repos/context-graph-ai/context-graph/*) echo "$TIP_CG" ;;
  *repos/context-graph-ai/contextdb/*) echo "$TIP_DB" ;;
  *) exit 91 ;;
esac
''')
            git.chmod(0o755)
            gh.chmod(0o755)
            environment = dict(os.environ, PATH=f'{scratch}:/usr/bin:/bin',
                               TRUSTED_BRANCH='dev',
                               CALLER_WORKFLOW_REF='context-graph-ai/vigil/.github/workflows/dev-haos-artifact.yml@refs/heads/dev', EXPECTED_VIGIL_SHA='a' * 40,
                               EXPECTED_CONTEXT_GRAPH_SHA='b' * 40, EXPECTED_CONTEXTDB_SHA='c' * 40,
                               TIP_VIGIL='a' * 40, TIP_CG='b' * 40, TIP_DB='c' * 40,
                               GITHUB_OUTPUT=str(scratch / 'output'))
            for branch in ('dev', 'dev-next'):
                result = subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                                        env=dict(environment, TRUSTED_BRANCH=branch, CALLER_WORKFLOW_REF='context-graph-ai/vigil/.github/workflows/dev-haos-artifact.yml@refs/heads/'+branch), capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
            for caller in ('build.yml', 'ci.yml'):
                automated = dict(environment, TIP_CG='e'*40, TIP_DB='f'*40,
                                 CALLER_WORKFLOW_REF='context-graph-ai/vigil/.github/workflows/'+caller+'@refs/heads/dev')
                result = subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                                        env=automated, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
            for change in ({'CALLER_WORKFLOW_REF': 'outsider/vigil/.github/workflows/ci.yml@refs/heads/dev'},
                           {'TIP_VIGIL': 'd' * 40}, {'TIP_CG': 'd' * 40},
                           {'TIP_DB': 'd' * 40}, {'DIRTY': ' M source'}, {'TRUSTED_BRANCH': 'main'}):
                result = subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                                        env=dict(environment, **change), capture_output=True)
                self.assertNotEqual(result.returncode, 0, change)

    def test_empty_public_pin_refuses_before_any_network(self):
        pins = json.loads((ROOT / 'scripts/ci-dependency-pins.json').read_text())
        if pins['context_graph']:
            self.skipTest('empty-pin transition completed; public fetch needs separate qualification')
        with tempfile.TemporaryDirectory(prefix='vigil-build-pending-') as directory:
            scratch = Path(directory)
            log = scratch / 'forbidden-network'
            for name in ('curl', 'git', 'gh', 'ssh', 'cargo'):
                command = scratch / name
                command.write_text('#!/bin/bash\nprintf "%s\\n" "$0" >> "$FORBIDDEN_LOG"\nexit 99\n')
                command.chmod(0o755)
            result = subprocess.run(['bash', str(ROOT / 'scripts/ci-public-dependencies'), 'check'],
                                    env=dict(os.environ, PATH=f'{scratch}:/usr/bin:/bin',
                                             FORBIDDEN_LOG=str(log)), capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('PENDING:', result.stderr)
            self.assertFalse(log.exists(), 'pending path attempted a prohibited command')


def load_tests(loader, tests, pattern):
    # Keep the existing security/smoke tests, and run the owner-authorized
    # automatic-delivery controls through the existing portable entrypoint.
    import importlib.util
    import sys
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location(
        'development_controls', ROOT / 'scripts/ci-development-controls.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    tests.addTests(loader.loadTestsFromModule(module))
    metadata_spec = importlib.util.spec_from_file_location(
        'publisher_metadata_controls', Path(__file__).with_name('ci-publisher-metadata-tests.py'))
    metadata_module = importlib.util.module_from_spec(metadata_spec)
    metadata_spec.loader.exec_module(metadata_module)
    tests.addTests(loader.loadTestsFromModule(metadata_module))
    execution_spec = importlib.util.spec_from_file_location(
        'execution_contract_controls', Path(__file__).with_name('ci-execution-contract-tests.py'))
    execution_module = importlib.util.module_from_spec(execution_spec)
    execution_spec.loader.exec_module(execution_module)
    execution_module.bind(sys.modules[__name__])
    tests.addTests(loader.loadTestsFromModule(execution_module))
    return tests


if __name__ == '__main__':
    unittest.main(verbosity=2)
