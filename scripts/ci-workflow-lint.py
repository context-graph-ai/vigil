#!/usr/bin/env python3
"""Keep actionlint checks; bridge only its missing job-level queue:max grammar."""
from pathlib import Path
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
import yaml
import importlib.util
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('delivery_controls', ROOT / 'scripts/ci-development-controls.py')
controls = importlib.util.module_from_spec(spec)
spec.loader.exec_module(controls)


def compatible(path):
    original = path.read_text()
    data = yaml.load(original, Loader=yaml.BaseLoader)
    def queues(value):
        if isinstance(value, dict):
            return sum((1 if key == 'queue' else 0) + queues(item) for key, item in value.items())
        if isinstance(value, list):
            return sum(queues(item) for item in value)
        return 0
    count = queues(data)
    if path.name == 'build.yml':
        if not controls.queue_valid(data) or count != 1:
            raise ValueError('only exact publisher concurrency queue:max is allowed')
        # Remove one reviewed line for old actionlint only. All other input is
        # byte-identical; unknown keys, actions, expressions and shells still lint.
        if original.count('      queue: max\n') != 1:
            raise ValueError('noncanonical queue placement')
        return original.replace('      queue: max\n', '', 1)
    if count:
        raise ValueError('queue outside build publisher')
    return original


def main():
    with tempfile.TemporaryDirectory(prefix='vigil-actionlint-') as directory:
        project = Path(directory)
        (project / '.git').mkdir()
        workflows = project / '.github/workflows'
        workflows.mkdir(parents=True)
        for name in ('actionlint.yaml', 'actionlint.yml'):
            config = ROOT / '.github' / name
            if config.is_file():
                (project / '.github' / name).write_bytes(config.read_bytes())
        paths = []
        for path in sorted((ROOT / '.github/workflows').glob('*.yml')):
            target = workflows / path.name
            target.write_text(compatible(path))
            paths.append(str(target))
        return subprocess.run(['actionlint', *paths], cwd=project).returncode


if __name__ == '__main__':
    sys.exit(main())
