#!/usr/bin/env python3
"""Offline development-delivery structure controls, including negative mutations.

No publisher helper API is assumed. These controls certify wiring/security only;
release API state transitions require executable implementation-specific controls.
Run directly, or through ci-build-controls.py / ci-portable.
"""
import ast
import copy
import hashlib
import json
from pathlib import Path
import re
import unittest
import sys

sys.dont_write_bytecode = True

import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / '.github/workflows'


def load(name):
    return yaml.load((WORKFLOWS / name).read_text(), Loader=yaml.BaseLoader)


def needs(job):
    value = job.get('needs', [])
    return value if isinstance(value, list) else [value]


def condition(expression, event='push', ref='refs/heads/dev', repository='context-graph-ai/vigil', result='success'):
    """Evaluate the small declarative caller guard, never repository code."""
    expression = expression.strip().removeprefix('${{').removesuffix('}}').strip()
    # GitHub adds success() implicitly unless a status function is explicit.
    if not re.search(r'\b(?:success|always|failure|cancelled)\s*\(', expression):
        expression = 'success() && (' + expression + ')'
    values = {'github.event_name': event, 'github.ref': ref, 'github.repository': repository,
              'needs.change-gate.result': result}
    for key, value in values.items():
        expression = expression.replace(key, repr(value))
    expression = expression.replace('&&', ' and ').replace('||', ' or ')
    expression = re.sub(r'!(?!=)', ' not ', expression)
    expression = expression.replace('success()', repr(result == 'success')).replace('always()', 'True')
    expression = re.sub(r'\btrue\b', 'True', expression)
    expression = re.sub(r'\bfalse\b', 'False', expression)
    try:
        tree = ast.parse(expression.strip(), mode='eval')
        allowed = (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.UnaryOp, ast.Not,
                   ast.Compare, ast.Eq, ast.NotEq, ast.Constant)
        if any(not isinstance(node, allowed) for node in ast.walk(tree)):
            return False
        return eval(compile(tree, '<caller guard>', 'eval'), {'__builtins__': {}}) is True
    except (ValueError, SyntaxError, TypeError):
        return False


def automatic_valid(ci):
    job = ci.get('jobs', {}).get('development-binaries', {})
    # Pin the complete reviewed reusable-workflow caller.
    digest = hashlib.sha256(json.dumps(
        job, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()).hexdigest()
    if digest != '3fda840226c48d9bc915355812165b53173a99262b6266bca956eaa3fd1d1921':
        return False
    if (needs(job) != ['change-gate'] or job.get('uses') != './.github/workflows/build.yml'
            or job.get('secrets') or job.get('continue-on-error') == 'true'):
        return False
    guard = job.get('if', '')
    return (condition(guard)
            and all(not condition(guard, **case) for case in [
                {'event': 'pull_request'}, {'event': 'pull_request_target'},
                {'event': 'workflow_run'}, {'event': 'workflow_dispatch'},
                {'ref': 'refs/heads/main'}, {'ref': 'refs/heads/dev-next'},
                {'ref': 'refs/pull/1/merge'}, {'ref': 'refs/tags/dev-latest'},
                {'repository': 'outsider/vigil'},
                {'result': 'failure'}, {'result': 'cancelled'},
                {'result': 'skipped'}, {'result': ''}]))


def publisher_jobs(build):
    return [(key, job) for key, job in build.get('jobs', {}).items()
            if job.get('permissions', {}).get('contents') == 'write']


def permissions_valid(build):
    if build.get('permissions', {}).get('contents') != 'read':
        return False
    if any(value == 'write' for value in build.get('permissions', {}).values()):
        return False
    publishers = publisher_jobs(build)
    if len(publishers) != 1:
        return False
    publisher_id = publishers[0][0]
    for key, job in build['jobs'].items():
        permissions = job.get('permissions', build['permissions'])
        if not isinstance(permissions, dict):
            return False
        if any(value == 'write' and not (key == publisher_id and scope == 'contents')
               for scope, value in permissions.items()):
            return False
        if job.get('secrets'):
            return False
    return True


def queue_valid(build):
    publishers = publisher_jobs(build)
    if len(publishers) != 1:
        return False
    publisher_id, publisher = publishers[0]
    lock = publisher.get('concurrency', {})
    if lock != {'group': 'vigil-dev-binaries-publish', 'cancel-in-progress': 'false', 'queue': 'max'}:
        return False
    def visit(value, path=()):
        if isinstance(value, dict):
            for key, item in value.items():
                if key == 'queue' and path != ('jobs', publisher_id, 'concurrency'):
                    return False
                if not visit(item, path + (key,)):
                    return False
        elif isinstance(value, list):
            return all(visit(item, path + (index,)) for index, item in enumerate(value))
        return True
    return visit(build)


class DevelopmentDeliveryControls(unittest.TestCase):
    def setUp(self):
        self.ci = load('ci.yml')
        self.build = load('build.yml')
        self.haos = load('dev-haos-artifact.yml')

    def publisher(self):
        jobs = publisher_jobs(self.build)
        self.assertEqual(len(jobs), 1, 'MISSING: one isolated contents:write development publisher')
        return jobs[0]

    def test_automatic_delivery_requires_actual_aggregate_and_trusted_dev_push(self):
        self.assertTrue(automatic_valid(self.ci),
                        'MISSING: local same-commit build.yml call after change-gate on canonical Vigil dev push')

    def test_build_has_reusable_and_manual_entrypoints_only(self):
        self.assertEqual(set(self.build['on']), {'workflow_call', 'workflow_dispatch'})
        self.assertIn('workflow_dispatch', self.haos['on'], 'preserve direct manual HAOS retry')
        self.assertIn('workflow_call', self.haos['on'])

    def test_write_permission_is_confined_to_publisher(self):
        self.assertTrue(permissions_valid(self.build), 'MISSING: isolated minimum-write publisher; build remains read-only')

    def test_native_max_queue_only_on_publisher(self):
        self.assertTrue(queue_valid(self.build), 'MISSING: job-only native max queue with cancellation disabled')

    def test_publisher_waits_for_real_existing_haos_recipe(self):
        key, publisher = self.publisher()
        packages = {name for name, job in self.build['jobs'].items()
                    if job.get('uses') == './.github/workflows/dev-haos-artifact.yml'}
        self.assertTrue(packages, 'existing HAOS reusable recipe must remain the packaging path')
        def ancestors(job_id, seen=None):
            seen = set() if seen is None else seen
            for dependency in needs(self.build['jobs'][job_id]):
                if dependency in self.build['jobs'] and dependency not in seen:
                    seen.add(dependency)
                    ancestors(dependency, seen)
            return seen
        self.assertTrue(packages <= ancestors(key), 'publisher must depend on completed HAOS package/smoke')
        self.assertNotIn('continue-on-error', publisher)
        self.assertNotIn(publisher.get('if'), ('always()', '${{ always() }}'))

    def test_publisher_never_checks_out_or_executes_downloaded_payload(self):
        # Pin the complete reviewed publisher and inherited execution metadata.
        # Any metadata change requires an explicit baseline review.
        reviewed = {'publisher': self.build['jobs']['publish'],
                    **{key: self.build[key] for key in ('defaults', 'env') if key in self.build}}
        digest = hashlib.sha256(json.dumps(
            reviewed, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()).hexdigest()
        self.assertEqual(digest, '9dce0b7fc4b74a9c73f27faae223bbe2ed15f5088bb202281df5cea76b370ba4',
                         'publisher execution metadata differs from reviewed baseline')
        _, publisher = self.publisher()
        # Fail closed over the reviewed publisher's three data/control steps.
        # A language blacklist misses indirect execution and downloaded scripts.
        steps = publisher.get('steps', [])
        self.assertEqual(len(steps), 3, 'unreviewed publisher step')
        trusted_fetch = '''test "$GITHUB_SHA" = "$SOURCE_SHA"
          test "$GITHUB_REPOSITORY" = context-graph-ai/vigil
          [[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]
          gh api "repos/$GITHUB_REPOSITORY/contents/scripts/ci-development.py?ref=$SOURCE_SHA" \\
            --jq '.content' | base64 --decode > "$RUNNER_TEMP/ci-development.py"'''
        expected = [trusted_fetch, None,
                    'python3 "$RUNNER_TEMP/ci-development.py" publish delivery']
        for index, step in enumerate(steps):
            self.assertNotIn('continue-on-error', step)
            self.assertNotIn('env', step, 'publisher step cannot override source binding')
            self.assertNotIn('shell', step, 'publisher must use the reviewed default shell')
            self.assertNotIn('working-directory', step)
            if index == 1:
                self.assertEqual(step.get('uses'),
                                 'actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093')
                self.assertNotIn('run', step)
                self.assertEqual(step.get('with'), {
                    'name': 'vigil-haos-production-amd64-${{ needs.metadata.outputs.vigil_sha }}',
                    'path': 'delivery'})
            else:
                self.assertNotIn('uses', step)
                # Ignore indentation only; preserve shell statement boundaries.
                self.assertEqual([line.strip() for line in step.get('run', '').strip().splitlines()],
                                 [line.strip() for line in expected[index].splitlines()],
                                 'publisher may execute only source-bound reviewed control code')

    def test_publisher_security_guard_rejects_every_payload_execution_route(self):
        original = self.build
        commands = [
            'python3 delivery/payload.py',
            "python3 -c 'import subprocess; subprocess.run([\"delivery/payload\"])'",
            "python3 - <<'PYCODE'\nimport subprocess\nsubprocess.run(['delivery/payload'])\nPYCODE",
            'bash delivery/payload.sh', 'sh delivery/payload.sh',
            'node delivery/payload.js',
            "node -e 'require(\"child_process\").execSync(\"delivery/payload\")'",
            'perl delivery/payload.pl', 'exec delivery/payload',
            'source delivery/payload.sh', '. delivery/payload.sh',
            'chmod +x delivery/payload\ndelivery/payload',
            'gh api repos/context-graph-ai/vigil/releases/assets/1 > "$RUNNER_TEMP/downloaded"\nbash "$RUNNER_TEMP/downloaded"',
        ]
        # Exercise the same guard that certifies the real workflow. A passing
        # positive witness above permits reviewed source-bound control code.
        for command in commands:
            self.build = copy.deepcopy(original)
            _, publisher = self.publisher()
            publisher['steps'].append({'run': command})
            with self.subTest(command=command), self.assertRaises(AssertionError,
                    msg='publisher security guard accepted artifact execution'):
                self.test_publisher_never_checks_out_or_executes_downloaded_payload()
        self.build = original

    def test_required_correctness_inventory_and_gate_stay_intact(self):
        jobs = self.ci['jobs']
        self.assertTrue({'portable', 'source-revisions', 'change', 'change-gate'} <= set(jobs))
        self.assertFalse(set(jobs) - {'portable', 'source-revisions', 'change', 'change-gate', 'development-binaries'})
        for event in ('push', 'pull_request'):
            self.assertIn(event, self.ci['on'])
            self.assertNotIn('paths', self.ci['on'][event])
            self.assertNotIn('paths-ignore', self.ci['on'][event])
        self.assertEqual(set(needs(jobs['change-gate'])), {'portable', 'source-revisions', 'change'})
        self.assertEqual(jobs['change-gate']['if'], 'always()')
        self.assertEqual(jobs['change']['uses'], './.github/workflows/default-checks.yml')
        self.assertEqual(needs(jobs['change']), ['source-revisions'])
        script = jobs['change-gate']['steps'][0]['run']
        env = jobs['change-gate']['steps'][0]['env']
        self.assertEqual(env, {
            'PORTABLE_RESULT': '${{ needs.portable.result }}',
            'PUBLIC_TUPLE_RESULT': '${{ needs.source-revisions.result }}',
            'CHANGE_RESULT': '${{ needs.change.result }}'})
        import os
        import subprocess
        baseline = dict(os.environ, **{key: 'success' for key in env})
        self.assertEqual(subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                                       env=baseline, capture_output=True).returncode, 0)
        for key in env:
            for value in ('failure', 'cancelled', 'skipped', '', 'neutral'):
                with self.subTest(job=key, result=value):
                    result = subprocess.run(['bash', '-euo', 'pipefail', '-c', script],
                                            env=dict(baseline, **{key: value}), capture_output=True)
                    self.assertNotEqual(result.returncode, 0)
        commands = '\n'.join(step.get('run', '') for step in load('default-checks.yml')['jobs']['change']['steps'])
        for profile in ('runtime-harness', 'fmt', 'clippy', 'default-inventory', 'default-estate', 'default-tests'):
            self.assertIn('./scripts/verify workflow --step ' + profile, commands)


class NativeQualificationControls(unittest.TestCase):
    def test_specialised_qualification_inventory_remains_required(self):
        expected = {
            'dev-closeout.yml': ('dev-closeout-gate', {
                'exact-sources', 'impact', 'fast', 'features', 'slow-preflight',
                'slow-shards', 'install-preflight', 'install-smoke'}),
            'release-qualification.yml': ('release-qualified', {
                'exact-sources', 'clean-qualification', 'static', 'static-image', 'hardware'}),
        }
        for filename, (aggregate, prerequisites) in expected.items():
            with self.subTest(workflow=filename):
                jobs = load(filename)['jobs']
                self.assertEqual(set(jobs), prerequisites | {aggregate})
                self.assertEqual(set(needs(jobs[aggregate])), prerequisites)
                for name in prerequisites:
                    self.assertNotIn('continue-on-error', jobs[name])
        self.assertEqual(set(load('integrate-dev.yml')['jobs']), {'integrate'})
        smoke = (ROOT / 'scripts/ci-packaged-smoke.py').read_text()
        self.assertIn('no hardware, HAOS or performance qualification', smoke)


class PublicTupleMutationControls(unittest.TestCase):
    def test_real_public_dependency_entrypoint_refuses_invalid_and_mismatched_pins_before_network(self):
        import json
        import os
        import subprocess
        import tempfile
        with tempfile.TemporaryDirectory(prefix='vigil-pin-controls-') as directory:
            scratch = Path(directory)
            script = scratch / 'ci-public-dependencies'
            script.write_bytes((ROOT / 'scripts/ci-public-dependencies').read_bytes())
            commands = scratch / 'commands'
            commands.mkdir()
            log = scratch / 'forbidden.log'
            for name in ('curl', 'git', 'gh', 'ssh', 'cargo'):
                stub = commands / name
                stub.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$FORBIDDEN_LOG"\nexit 99\n')
                stub.chmod(0o755)
            environment = dict(os.environ, PATH=f'{commands}:/usr/bin:/bin', FORBIDDEN_LOG=str(log))
            valid = {'context_graph': 'b' * 40, 'contextdb': 'c' * 40}
            cases = [({}, []), ({**valid, 'extra': 'd' * 40}, [])]
            for field in valid:
                for value in ('', 'dev', 'a' * 39, 'g' * 40, None, 123):
                    cases.append(({**valid, field: value}, []))
            cases.extend([(valid, ['d' * 40, valid['contextdb']]),
                          (valid, [valid['context_graph'], 'd' * 40])])
            for pins, arguments in cases:
                with self.subTest(pins=pins, arguments=arguments):
                    (scratch / 'ci-dependency-pins.json').write_text(json.dumps(pins))
                    result = subprocess.run(['bash', str(script), 'check', *arguments],
                                            env=environment, capture_output=True, timeout=5)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(log.exists(), 'invalid tuple reached a network/build command')


class DevelopmentMutationControls(unittest.TestCase):
    """Positive witnesses prevent always-false validators from passing controls."""
    def automatic_fixture(self):
        return copy.deepcopy(load('ci.yml'))

    def publisher_fixture(self):
        return {'permissions': {'contents': 'read'}, 'jobs': {
            'package': {'permissions': {'contents': 'read'}},
            'publish': {'permissions': {'contents': 'write', 'actions': 'read'},
                        'concurrency': {'group': 'vigil-dev-binaries-publish',
                                        'cancel-in-progress': 'false', 'queue': 'max'}}}}

    def test_realistic_admission_mutations(self):
        good = self.automatic_fixture()
        self.assertTrue(automatic_valid(good))
        for key, value in [('needs', ['required']), ('needs', ['portable']), ('needs', []),
                           ('uses', 'someone/vigil/.github/workflows/build.yml@dev'),
                           ('secrets', 'inherit'), ('continue-on-error', 'true')]:
            bad = copy.deepcopy(good)
            bad['jobs']['development-binaries'][key] = value
            with self.subTest(key=key, value=value):
                self.assertFalse(automatic_valid(bad))
        for clause in ("github.event_name == 'push'", "github.ref == 'refs/heads/dev'",
                       "github.repository == 'context-graph-ai/vigil'"):
            bad = copy.deepcopy(good)
            bad['jobs']['development-binaries']['if'] = good['jobs']['development-binaries']['if'].replace(clause, 'true')
            self.assertFalse(automatic_valid(bad), clause)

    def test_realistic_permission_mutations(self):
        good = self.publisher_fixture()
        self.assertTrue(permissions_valid(good))
        for scope in ('contents', 'actions', 'id-token', 'packages'):
            bad = copy.deepcopy(good)
            bad['jobs']['package']['permissions'][scope] = 'write'
            self.assertFalse(permissions_valid(bad), scope)
        bad = copy.deepcopy(good)
        bad['permissions']['contents'] = 'write'
        self.assertFalse(permissions_valid(bad))
        bad = copy.deepcopy(good)
        bad['jobs']['publish']['secrets'] = 'inherit'
        self.assertFalse(permissions_valid(bad))

    def test_realistic_queue_mutations(self):
        good = self.publisher_fixture()
        self.assertTrue(queue_valid(good))
        for key, value in [('queue', 'single'), ('queue', '100'), ('cancel-in-progress', 'true'),
                           ('group', 'vigil-${{ github.sha }}'), ('invented-key', 'max')]:
            bad = copy.deepcopy(good)
            bad['jobs']['publish']['concurrency'][key] = value
            self.assertFalse(queue_valid(bad), key)
        bad = copy.deepcopy(good)
        bad['concurrency'] = bad['jobs']['publish'].pop('concurrency')
        self.assertFalse(queue_valid(bad), 'workflow-level lock')
        bad = copy.deepcopy(good)
        bad['jobs']['package']['concurrency'] = {'queue': 'max'}
        self.assertFalse(queue_valid(bad), 'queue outside publisher')


if __name__ == '__main__':
    unittest.main(verbosity=2)
