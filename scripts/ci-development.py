#!/usr/bin/env python3
"""Trusted development delivery control. Never extracts or executes package bytes."""
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tomllib
import urllib.parse
import urllib.request

REPO = 'context-graph-ai/vigil'
ARCHIVE = 'vigil-haos-production-amd64.tar.gz'
TARGET = 'x86_64-alpine-linux-musl'
FIELDS = {'source', 'context_graph', 'contextdb', 'version', 'toolchain', 'target', 'archive_sha256', 'run_id', 'run_attempt'}


def require(ok, message):
    if not ok:
        raise ValueError(message)


def sha(value):
    require(isinstance(value, str) and re.fullmatch('[0-9a-f]{40}', value), 'exact source required')
    return value


def identifier(value):
    require(type(value) is int and value > 0, 'invalid API identifier')
    return value


def digest(data):
    return hashlib.sha256(data).hexdigest()


class API:
    def request(self, path, method='GET', data=None, binary=False, raw=False):
        require(path.startswith('/repos/' + REPO + '/') or path == '/repos/' + REPO,
                'API path outside Vigil')
        headers = {'Authorization': 'Bearer ' + os.environ['GH_TOKEN'],
                   'Accept': 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28'}
        if raw:
            headers['Accept'] = 'application/octet-stream'
        if isinstance(data, bytes):
            headers['Content-Type'] = 'application/octet-stream'
            body = data
        else:
            headers['Content-Type'] = 'application/json'
            body = None if data is None else json.dumps(data).encode()
        host = 'https://uploads.github.com' if binary else 'https://api.github.com'
        with urllib.request.urlopen(urllib.request.Request(host + path, body, headers, method=method), timeout=60) as response:
            raw = response.read()
        return raw if headers['Accept'] == 'application/octet-stream' else (json.loads(raw) if raw else None)

    def pages(self, path, key=None):
        rows = []
        for page in range(1, 1001):
            result = self.request(path + ('&' if '?' in path else '?') + f'per_page=100&page={page}')
            batch = result[key] if key else result
            require(isinstance(batch, list), 'invalid paginated response')
            rows.extend(batch)
            if len(batch) < 100:
                return rows
        raise ValueError('pagination exceeded bound')

    def file(self, name, source):
        import base64
        result = self.request(f'/repos/{REPO}/contents/{name}?ref={sha(source)}')
        require(result['type'] == 'file' and result['encoding'] == 'base64', 'invalid source file')
        return base64.b64decode(result['content'], validate=False)


def base(allow_dev_next=False):
    require(os.environ['GITHUB_REPOSITORY'] == REPO, 'canonical repository required')
    refs = ('refs/heads/dev', 'refs/heads/dev-next') if allow_dev_next else ('refs/heads/dev',)
    require(os.environ['GITHUB_REF'] in refs, 'trusted development ref required')
    require(os.environ['GITHUB_EVENT_NAME'] in ('push', 'workflow_dispatch'), 'trusted event required')
    return sha(os.environ['GITHUB_SHA'])


def pins(data):
    require(set(data) == {'context_graph', 'contextdb'}, 'invalid committed pin keys')
    for value in data.values():
        require(isinstance(value, str) and re.fullmatch('[0-9a-f]{40}', value),
                'PENDING: exact PUBLIC dependency pins required before network or build')
    return data


def correctness(api, run, source, current=False):
    require(run['head_sha'] == source and run['event'] == 'push' and run['head_branch'] == 'dev'
            and run['path'] == '.github/workflows/ci.yml'
            and run['head_repository']['full_name'] == REPO, 'CI source identity mismatch')
    run_id = identifier(run['id'])
    attempt = identifier(run['run_attempt'])
    if current:
        require(run_id == int(os.environ['GITHUB_RUN_ID']) and attempt == int(os.environ['GITHUB_RUN_ATTEMPT']),
                'current CI run/attempt mismatch')
        # The overall run is deliberately still running while this reusable call executes.
        require(run['status'] == 'in_progress', 'automatic CI must still be running')
    jobs = api.pages(f'/repos/{REPO}/actions/runs/{run_id}/attempts/{attempt}/jobs', 'jobs')
    expected = {'Portable CI checks (not full qualification)', 'Validate public exact dependency tuple',
                'Default change gate / Default change gate', 'Change qualification receipt'}
    seen = set()
    for job in jobs:
        name = job['name']
        delivery = name == 'development-binaries' or name.startswith('development-binaries / ')
        if delivery:
            continue
        require(name in expected and name not in seen, 'unexpected or duplicate correctness job')
        require(job['status'] == 'completed' and job['conclusion'] == 'success', 'correctness did not succeed')
        seen.add(name)
    require(seen == expected, 'missing successful correctness job/aggregate')
    if not current:
        require(run['status'] == 'completed', 'manual retry requires settled correctness run')
        require(run['conclusion'] in ('success', 'failure'), 'manual retry cannot recover cancellation')


def admit(api, root):
    source = base()
    # Checkout is source-bound. Validate pins before the first API request.
    require(__import__('subprocess').check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip() == source,
            'checkout source mismatch')
    tuple_ = pins(json.loads((root / 'scripts/ci-dependency-pins.json').read_text()))
    if os.environ['GITHUB_EVENT_NAME'] == 'workflow_dispatch':
        for key, value in [('vigil_sha', source), ('context_graph_sha', tuple_['context_graph']),
                           ('contextdb_sha', tuple_['contextdb'])]:
            require(os.environ['INPUT_' + key.upper()] == value, 'manual tuple differs from committed source/pins')
    require(api.request(f'/repos/{REPO}/git/ref/heads/dev')['object']['sha'] == source, 'dev tip moved')
    if os.environ['GITHUB_EVENT_NAME'] == 'push':
        run = api.request(f'/repos/{REPO}/actions/runs/{int(os.environ["GITHUB_RUN_ID"])}')
        correctness(api, run, source, True)
    else:
        runs = api.pages(f'/repos/{REPO}/actions/workflows/ci.yml/runs?event=push&head_sha={source}', 'workflow_runs')
        errors = []
        for run in runs:
            try:
                correctness(api, run, source)
                break
            except (ValueError, KeyError) as error:
                errors.append(str(error))
        else:
            raise ValueError('no exact-source successful correctness proof: ' + '; '.join(errors))
    with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
        for key, value in dict(vigil_sha=source, context_graph_sha=tuple_['context_graph'],
                               contextdb_sha=tuple_['contextdb']).items():
            output.write(f'{key}={value}\n')


def receipt(root, output, toolchain):
    # Preserve the original direct HAOS manual dev-next packaging entrypoint.
    # The release publisher and automatic controller still admit dev only.
    source = base(allow_dev_next=True)
    tuple_ = pins(json.loads((root / 'scripts/ci-dependency-pins.json').read_text()))
    smoke = json.loads((output / 'receipts/dev-haos-artifact-smoke.json').read_text())
    archive_hash = digest((output / ARCHIVE).read_bytes())
    require(smoke['archive_sha256'] == archive_hash, 'smoked archive differs')
    require(isinstance(toolchain, str) and toolchain.startswith('rustc ') and len(toolchain) < 256,
            'actual builder rustc version required')
    version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']
    result = dict(source=source, **tuple_, version=version, toolchain=toolchain,
                  target=TARGET, archive_sha256=archive_hash,
                  run_id=identifier(int(os.environ['GITHUB_RUN_ID'])),
                  run_attempt=identifier(int(os.environ['GITHUB_RUN_ATTEMPT'])))
    (output / 'delivery.json').write_text(json.dumps(result, sort_keys=True, indent=2) + '\n')


def publish(api, directory, expected):
    source = base()
    run_id = identifier(int(os.environ['GITHUB_RUN_ID']))
    run_attempt = identifier(int(os.environ['GITHUB_RUN_ATTEMPT']))
    require(source == expected['source'], 'publisher source mismatch')
    # Never copy raw build receipts or logs. Decode and validate the exact allowlist.
    data = json.loads((directory / 'delivery.json').read_text())
    require(set(data) == FIELDS, 'receipt fields outside public allowlist')
    require(type(data['run_id']) is int and type(data['run_attempt']) is int
            and data['run_id'] == run_id and data['run_attempt'] == run_attempt, 'receipt run identity mismatch')
    for key, value in expected.items():
        require(data[key] == value, 'receipt differs from trusted source metadata')
    archive = (directory / ARCHIVE).read_bytes()
    require(data['archive_sha256'] == digest(archive) and data['target'] == TARGET, 'archive identity mismatch')
    require(isinstance(data['toolchain'], str) and data['toolchain'].startswith('rustc ')
            and len(data['toolchain']) < 256 and '\n' not in data['toolchain'], 'invalid toolchain')
    require(isinstance(data['version'], str) and re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?', data['version']), 'invalid version')

    def tip():
        return api.request(f'/repos/{REPO}/git/ref/heads/dev')['object']['sha'] == source

    if not tip():
        return 'Skipped: source is no longer the dev tip.'
    releases = [r for r in api.pages(f'/repos/{REPO}/releases') if r['tag_name'] == 'dev-latest']
    require(len(releases) <= 1, 'duplicate channel releases')
    if releases:
        release = releases[0]
    else:
        repository = api.request(f'/repos/{REPO}')
        branch = urllib.parse.quote(repository['default_branch'], safe='')
        head = sha(api.request(f'/repos/{REPO}/git/ref/heads/{branch}')['object']['sha'])
        # Existing tag is never updated. Creating the release uses an existing tag
        # unchanged, or bootstraps it at the current default branch head.
        release = api.request(f'/repos/{REPO}/releases', 'POST', {
            'tag_name': 'dev-latest', 'target_commitish': head, 'draft': True,
            'prerelease': True, 'make_latest': 'false', 'name': 'Development binaries (preparing)'})
    rid = identifier(release['id'])
    require(release['prerelease'] is True, 'channel must remain prerelease')
    assets_path = f'/repos/{REPO}/releases/{rid}/assets'

    def assets():
        result = api.pages(assets_path)
        require(len({a['name'] for a in result}) == len(result), 'duplicate asset names')
        for asset in result:
            identifier(asset['id'])
        return {a['name']: a for a in result}

    receipt_bytes = (json.dumps(data, sort_keys=True, indent=2) + '\n').encode()

    def group(name):
        return [(name, archive), (name + '.sha256', f'{digest(archive)}  {name}\n'.encode()),
                (name + '.json', receipt_bytes)]

    def read(asset):
        return api.request(f'/repos/{REPO}/releases/assets/{identifier(asset["id"])}', raw=True)

    def verify(items):
        remote = assets()
        for name, content in items:
            require(name in remote and digest(read(remote[name])) == digest(content),
                    'actual remote asset bytes differ')

    def upload(name, content):
        # GitHub rejects duplicate asset names. Never delete or clobber here.
        api.request(assets_path + '?name=' + urllib.parse.quote(name), 'POST', content, binary=True)

    run_name = ARCHIVE.removesuffix('.tar.gz') + f'-{TARGET}-{source[:12]}-r{run_id}a{run_attempt}.tar.gz'
    immutable = group(run_name)
    existing = assets()
    require(not any(name in existing for name, _ in immutable), 'run group name already exists; collision')
    for name, content in immutable:
        upload(name, content)
    # All immutable bytes must be downloaded and hashed before ANY stable mutation.
    verify(immutable)
    for name, content in group(ARCHIVE):  # archive, checksum, receipt LAST
        existing = assets()
        if name in existing:
            if not tip():
                return 'Skipped stable replacement: dev tip moved; run group retained.'
            api.request(f'/repos/{REPO}/releases/assets/{identifier(existing[name]["id"])}', 'DELETE')
        if not tip():
            return 'Skipped stable replacement: dev tip moved; run group retained.'
        upload(name, content)
    verify(group(ARCHIVE))
    if not tip():
        return 'Skipped release metadata: dev tip moved; run group retained.'
    api.request(f'/repos/{REPO}/releases/{rid}', 'PATCH', {
        'name': 'Development binaries ' + source, 'body': 'Exact source: ' + source +
        f'\nRun: {run_id}, attempt: {run_attempt}\nRun-named assets:\n' +
        '\n'.join(name for name, _ in immutable) +
        '\nIf stable verification fails, use the newest complete run-named set. '
        'The stationary tag is the bootstrap point, not the binary source.',
        'draft': False, 'prerelease': True, 'make_latest': 'false'})
    # Pruning is the ONLY deletion path for run names. Incomplete groups survive.
    remote = assets()
    stable_receipt = read(remote[ARCHIVE + '.json'])
    groups = {}
    # Recognize older groups without a target for retention only; new writes always name TARGET.
    pattern = re.escape(ARCHIVE.removesuffix('.tar.gz')) + '(?:-' + re.escape(TARGET) + ')?' + r'-[0-9a-f]{12}-r[1-9][0-9]*a[1-9][0-9]*\.tar\.gz'
    for name, asset in remote.items():
        match = re.fullmatch('(' + pattern + r')(?:\.sha256|\.json)?', name)
        if match:
            groups.setdefault(match[1], {})[name] = asset
    complete = []
    protected = set()
    for name, members in groups.items():
        if set(members) != {name, name + '.sha256', name + '.json'}:
            continue
        receipt_content = read(members[name + '.json'])
        if receipt_content == stable_receipt:
            protected.add(name)
        # Presence alone does not make an interrupted/corrupt group complete.
        try:
            old_receipt = json.loads(receipt_content)
            h = digest(read(members[name]))
            if (old_receipt['archive_sha256'] != h or
                    read(members[name + '.sha256']) != f'{h}  {name}\n'.encode()):
                continue
        except (ValueError, KeyError, TypeError):
            continue
        complete.append((max(a['created_at'] for a in members.values()), name, members))
    complete.sort(reverse=True)
    retained = protected | {name for _, name, _ in complete[:3]}
    for _, name, members in complete:
        if name in retained:
            continue
        for asset in members.values():
            if not tip():
                return 'Published; pruning stopped because dev moved.'
            api.request(f'/repos/{REPO}/releases/assets/{identifier(asset["id"])}', 'DELETE')
    return 'Published source-bound development group; hosted download qualification is separate.'


def main():
    mode = sys.argv[1]
    api = API()
    if mode == 'admit':
        admit(api, Path('.'))
    elif mode == 'receipt':
        receipt(Path('.'), Path(sys.argv[2]), sys.argv[3])
    elif mode == 'publish':
        expected = dict(source=base(), context_graph=sha(os.environ['CONTEXT_GRAPH_SHA']),
                        contextdb=sha(os.environ['CONTEXTDB_SHA']))
        # Source-bound metadata, never an artifact-supplied version.
        manifest = tomllib.loads(api.file('Cargo.toml', expected['source']).decode())
        expected['version'] = manifest['workspace']['package']['version']
        result = publish(api, Path(sys.argv[2]), expected)
        print(result)
        if os.environ.get('GITHUB_STEP_SUMMARY'):
            with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
                summary.write(result + '\n')
    else:
        raise ValueError('unknown mode')


if __name__ == '__main__':
    main()
