#!/usr/bin/env python3
"""Independent execution vectors. Offline stubs only; loaded by canonical suite.

Guard implementations remain in the existing control modules, not here.
"""
import copy
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import yaml

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
UNSAFE_SHELL = "bash --noprofile --norc -c '( source \"$1\" ) || true' audit {0}"


def imported(name):
    spec = importlib.util.spec_from_file_location('execution_' + name.replace('-', '_'), ROOT / 'scripts' / name)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def bind(module):
    global canonical
    canonical = module


def document(name):
    return yaml.load((ROOT / '.github/workflows' / name).read_text(), Loader=LOADER)


def smoke(doc):
    return next(s for s in doc['jobs']['build']['steps'] if SMOKE in s.get('run', ''))


def failure_exit(doc):
    """Execute the unchanged Actions command file with only its entry tool stubbed.

    Resolve step > job > workflow shell, and inherit literal env at those levels.
    Expressions are inert fixture values; no package, docker or native code runs.
    """
    job = doc['jobs']['build']
    step = smoke(doc)
    shell = step.get('shell', job.get('defaults', {}).get('run', {}).get('shell',
                      doc.get('defaults', {}).get('run', {}).get('shell', None)))
    if shell is None:
        shell = 'bash -e {0}'
    elif shell == 'bash':
        shell = 'bash --noprofile --norc -e -o pipefail {0}'
    with tempfile.TemporaryDirectory(prefix='execution-failure-') as directory:
        root = Path(directory)
        command = root / 'command.sh'
        command.write_text(step['run'])
        stub = root / ENTRY
        stub.write_text('#!/bin/sh\nexit 37\n')
        stub.chmod(0o755)
        env = dict(PATH=str(root) + ':/usr/bin:/bin', HOME=str(root))
        for scope in (doc, job, step):
            env.update({k: str(v) for k, v in scope.get('env', {}).items()})
        args = shlex.split(shell.replace('{0}', shlex.quote(str(command))))
        # The outer shell is trusted; only the command file's entry tool is stubbed.
        args[0] = '/bin/bash'
        return subprocess.run(args, cwd=root, env=env, capture_output=True, timeout=10).returncode


class ExecutionContractControls(unittest.TestCase):
    def test_actual_configuration_passes_canonical_guards(self):
        self.assertTrue(build_valid(document(WORKFLOW)))
        self.assertTrue(caller_valid(document('ci.yml')))

    def test_unchanged_smoke_failure_effective_shell_and_unsafe_override(self):
        doc = document(WORKFLOW)
        self.assertEqual(failure_exit(doc), 37)
        bad = copy.deepcopy(doc)
        smoke(bad)['shell'] = UNSAFE_SHELL
        self.assertEqual(smoke(bad)['run'], smoke(doc)['run'])
        self.assertEqual(failure_exit(bad), 0)

    def test_canonical_guard_rejects_real_smoke_shell_override(self):
        doc = document(WORKFLOW)
        self.assertTrue(build_valid(doc))
        smoke(doc)['shell'] = UNSAFE_SHELL
        self.assertFalse(build_valid(doc), 'canonical guard accepted a shell that masks unchanged smoke failure')

    def test_canonical_build_execution_metadata_is_reviewed(self):
        for location, key, value in [
                ('workflow', 'defaults', {'run': {'shell': UNSAFE_SHELL}}),
                ('build', 'defaults', {'run': {'shell': UNSAFE_SHELL}}),
                ('build', 'env', {'BASH_ENV': 'unreviewed-control.sh'}),
                ('step', 'env', {'BASH_ENV': 'unreviewed-control.sh'}),
                ('build', 'if', '${{ false }}')]:
            with self.subTest(location=location, key=key):
                doc = document(WORKFLOW)
                target = doc if location == 'workflow' else doc['jobs']['build'] if location == 'build' else smoke(doc)
                target[key] = value
                self.assertFalse(build_valid(doc), 'canonical build guard accepted unreviewed execution metadata')

    def test_canonical_build_controller_rejects_unreviewed_control_execution(self):
        doc = document('build.yml')
        self.assertTrue(controller_valid(doc))
        job = doc['jobs'][CONTROL_JOB]
        step = next(s for s in job['steps'] if 'run' in s)
        step['run'] = 'exit 0\n' + step['run']
        self.assertFalse(controller_valid(doc), 'canonical guard accepted bypassed admission control')

    def test_reused_haos_recipe_cannot_skip_build_and_receipt(self):
        doc = document(WORKFLOW)
        self.assertTrue(build_valid(doc))
        step = next(s for s in doc['jobs']['build']['steps'] if 'Build and receipt' in s.get('name', ''))
        step['run'] = 'if false; then\n' + step['run'] + '\nfi'
        self.assertFalse(build_valid(doc), 'canonical guard accepted skipped reused HAOS recipe')

    def test_canonical_ci_delivery_job_rejects_unreviewed_execution_metadata(self):
        for key, value in [('env', {'BASH_ENV': 'unreviewed-control.sh'}),
                           ('defaults', {'run': {'shell': UNSAFE_SHELL}}),
                           ('continue-on-error', True)]:
            with self.subTest(key=key):
                doc = document('ci.yml')
                doc['jobs']['development-binaries'][key] = value
                self.assertFalse(caller_valid(doc), 'canonical caller guard accepted unreviewed delivery metadata')

LOADER = yaml.BaseLoader
WORKFLOW = 'dev-haos-artifact.yml'
SMOKE = 'ci-packaged-smoke.py'
ENTRY = 'python3'


def build_valid(doc):
    return canonical.packaged_path_valid(doc)


def caller_valid(doc):
    return imported('ci-development-controls.py').automatic_valid(doc)


class ManualForkContractControls(unittest.TestCase):
    def invoke(self, fork):
        controls = imported('ci-delivery-controls.py')
        api = controls.FakeAPI()
        api.run.update(status='completed', conclusion='success')
        api.jobs = api.jobs[:-1]
        with tempfile.TemporaryDirectory(prefix='qualified-manual-fork-') as directory:
            root = Path(directory)
            (root / 'scripts').mkdir()
            (root / 'scripts/ci-dependency-pins.json').write_text(json.dumps(controls.TUPLE))
            env = dict(controls.ENV, GITHUB_EVENT_NAME='workflow_dispatch',
                       INPUT_VIGIL_SHA=controls.SOURCE,
                       INPUT_CONTEXT_GRAPH_SHA=controls.TUPLE['context_graph'],
                       INPUT_CONTEXTDB_SHA=controls.TUPLE['contextdb'],
                       GITHUB_OUTPUT=str(root / 'output'))
            if fork:
                env['GITHUB_REPOSITORY'] = 'fork/vigil'
            with patch.dict(os.environ, env, clear=True), \
                 patch('subprocess.check_output', return_value=controls.SOURCE + '\n'):
                if fork:
                    try:
                        with self.assertRaisesRegex(ValueError, 'canonical repository required'):
                            controls.d.admit(api, root)
                    finally:
                        self.assertEqual(api.calls, [], 'fork refusal must happen before any API request')
                else:
                    controls.d.admit(api, root)
                    self.assertTrue(api.calls, 'qualified positive fixture must reach correctness API')
                    self.assertIn('vigil_sha=' + controls.SOURCE, (root / 'output').read_text())

    def test_qualified_canonical_manual_dispatch_passes(self):
        self.invoke(False)

    def test_qualified_manual_fork_refused_before_api(self):
        self.invoke(True)

CONTROL_JOB = 'metadata'


def controller_valid(doc):
    return canonical.controller_valid(doc, document(WORKFLOW))
