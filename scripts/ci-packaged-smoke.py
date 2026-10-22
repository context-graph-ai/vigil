#!/usr/bin/env python3
"""Archive and smoke the exported HAOS binary; never start the runtime."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def unpack(archive, destination, expected):
    with tarfile.open(archive, 'r:gz') as package:
        members = package.getmembers()
        if len(members) != 1 or members[0].name != 'binary/vigil':
            raise ValueError('package must contain exactly binary/vigil')
        member = members[0]
        if not member.isfile() or not member.mode & 0o111:
            raise ValueError('packaged vigil must be a regular executable')
        binary = destination / 'vigil'
        with package.extractfile(member) as source:
            binary.write_bytes(source.read())
        binary.chmod(0o555)
        if digest(binary) != expected:
            raise ValueError('unpacked binary differs from exported binary')
        return binary


def smoke(binary, archive, image, receipt):
    # A failed retry must not leave an earlier success receipt behind.
    receipt.unlink(missing_ok=True)
    if binary.is_symlink() or not binary.is_file() or not binary.stat().st_mode & 0o111:
        raise ValueError('exported vigil must be a regular executable')
    expected = digest(binary)
    with tarfile.open(archive, 'w:gz') as package:
        package.add(binary, arcname='binary/vigil', recursive=False)
    # Pin execution to the locally loaded image ID, never an implicit pull.
    image_id = subprocess.check_output(
        ['docker', 'image', 'inspect', '--format', '{{.Id}}', image], text=True, timeout=30).strip()
    if not image_id.startswith('sha256:'):
        raise ValueError('builder image has no immutable local ID')
    outputs = {}
    with tempfile.TemporaryDirectory(prefix='vigil-package-') as directory:
        extracted = unpack(archive, Path(directory), expected)
        # Confirm this environment is the builder of these exact exported bytes.
        # docker create/cp inspect files only; they never start the image command.
        container = subprocess.check_output([
            'docker', 'create', '--network=none', '--entrypoint', '/bin/false', image_id,
        ], text=True, timeout=30).strip()
        try:
            built = Path(directory) / 'builder-vigil'
            subprocess.run(['docker', 'cp',
                            f'{container}:/workspace/vigil-src/target/release/vigil', str(built)],
                           check=True, capture_output=True, text=True, timeout=30)
            if digest(built) != expected:
                raise ValueError('smoke builder does not match the exported package binary')
        finally:
            subprocess.run(['docker', 'rm', '-f', container], check=True,
                           capture_output=True, text=True, timeout=30)
        archive_sha256 = digest(archive)
        for argument in ('--help', '--version'):
            result = subprocess.run([
                'docker', 'run', '--rm', '--pull=never', '--platform', 'linux/amd64',
                '--network=none', '--read-only', '--cap-drop=ALL',
                '--security-opt=no-new-privileges', '--user', '65534:65534',
                '--mount', f'type=bind,src={extracted},dst=/smoke/vigil,readonly',
                '--entrypoint', '/usr/bin/env', image_id,
                '-i', 'PATH=/usr/local/bin:/usr/bin:/bin', '/smoke/vigil', argument,
            ], check=True, text=True, capture_output=True, timeout=30)
            if argument == '--help' and not all(
                    token in result.stdout for token in ('Commands:', '--help', '--version')):
                raise ValueError('packaged binary did not return Vigil help')
            if argument == '--version' and not result.stdout.startswith('vigil '):
                raise ValueError('packaged binary did not return Vigil version')
            outputs[argument] = {'stdout': result.stdout, 'stderr': result.stderr}
        if digest(archive) != archive_sha256:
            raise ValueError('package changed during smoke')
        if digest(extracted) != expected or digest(binary) != expected:
            raise ValueError('binary changed during smoke')
    receipt.write_text(json.dumps({
        'kind': 'packaged-cli-smoke', 'binary_sha256': expected,
        'archive_sha256': archive_sha256, 'runtime_image_id': image_id,
        'network': 'none', 'commands': outputs,
        'qualification': 'help/version only; no hardware, HAOS or performance qualification',
    }, indent=2) + '\n')


if __name__ == '__main__':
    if len(sys.argv) != 5:
        raise SystemExit('usage: ci-packaged-smoke.py BINARY ARCHIVE BUILDER_IMAGE RECEIPT')
    smoke(Path(sys.argv[1]).absolute(), Path(sys.argv[2]).resolve(),
          sys.argv[3], Path(sys.argv[4]).resolve())
