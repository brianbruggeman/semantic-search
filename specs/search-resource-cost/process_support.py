import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import time
import uuid

BIN = Path('/Users/brianbruggeman/repos/slot-0/.cargo_target_ss/release')


def run(arguments, environment, timeout=90):
    started = time.monotonic()
    result = subprocess.run([str(value) for value in arguments], env=environment,
                            text=True, capture_output=True, timeout=timeout)
    return {'command': [str(value) for value in arguments], 'returncode': result.returncode,
            'stdout': result.stdout, 'stderr': result.stderr,
            'elapsed_seconds': time.monotonic() - started}


def status(environment):
    return run([BIN / 'ssd', 'status'], environment, 5)


def wait_ready(environment, process):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        current = status(environment)
        if current['returncode'] == 0:
            return current
        if process.poll() is not None:
            raise RuntimeError(f'daemon exited: {current}')
    raise RuntimeError('daemon startup deadline exceeded')


def process_sample(pid):
    return subprocess.check_output(['ps', '-p', str(pid), '-o',
                                    'pid,ppid,%cpu,rss,time,command'], text=True)


def wait_log(path, predicate, seconds):
    deadline = time.monotonic() + seconds
    queue = select.kqueue()
    descriptor = None
    try:
        while True:
            contents = path.read_text(errors='replace') if path.exists() else ''
            if predicate(contents):
                return True, contents
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False, contents
            if descriptor is not None:
                descriptor.close()
            descriptor = path.open('rb')
            event = select.kevent(descriptor.fileno(), filter=select.KQ_FILTER_VNODE,
                                 flags=select.KQ_EV_ADD | select.KQ_EV_CLEAR,
                                 fflags=select.KQ_NOTE_WRITE | select.KQ_NOTE_EXTEND | select.KQ_NOTE_RENAME)
            queue.control([event], 0, 0)
            contents = path.read_text(errors='replace')
            if predicate(contents):
                return True, contents
            queue.control(None, 1, remaining)
    finally:
        if descriptor is not None:
            descriptor.close()
        queue.close()


def payload_paths(value):
    if isinstance(value, dict):
        for name, item in value.items():
            if name == 'path' and isinstance(item, str):
                yield str(Path(item).resolve())
            else:
                yield from payload_paths(item)
    elif isinstance(value, list):
        for item in value:
            yield from payload_paths(item)


def fixture_membership(paths, expected):
    resolved = [str(Path(path).resolve()) for path in paths]
    return bool(resolved) and all(path in expected for path in resolved)


def fixture_manifest(scope):
    root = Path(scope).resolve()
    files = sorted(path for path in root.rglob('*') if path.is_file())
    entries = []
    for path in files:
        digest = hashlib.sha256()
        with path.open('rb') as source:
            while block := source.read(1024 * 1024):
                digest.update(block)
        entries.append({'path': path.relative_to(root).as_posix(),
                        'bytes': path.stat().st_size, 'sha256': digest.hexdigest()})
    canonical = '\n'.join(f"{entry['path']} {entry['sha256']}" for entry in entries)
    return {'entries': entries, 'file_count': len(entries),
            'total_bytes': sum(entry['bytes'] for entry in entries),
            'tree_sha256': hashlib.sha256(canonical.encode()).hexdigest()}


def telemetry_events(text, body, module_path=None, tag_fields=None):
    required_tags = tag_fields or {}
    records = []
    for line in text.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(record, dict) or record.get('body') != body:
            continue
        if module_path is not None and record.get('module_path') != module_path:
            continue
        tags = record.get('tags')
        if not isinstance(tags, dict) or any(tags.get(key) != value for key, value in required_tags.items()):
            continue
        records.append(record)
    return records


def experiment_environment(environment):
    recorded_names = {
        'SS_HOME', 'SS_DAEMON_BIN', 'SS_EMBEDDER', 'SS_EMBED_PROVIDER',
        'SS_EMBED_MODEL', 'SS_EMBED_MAX_LENGTH', 'SS_EMBED_QUERY_WORKERS',
        'SS_EMBED_WORKERS', 'SS_EMBED_THREADS', 'SS_WARM_START',
        'SS_EMBED_IDLE_SECS', 'SS_IDLE_COMPACT_SECS', 'SS_IDLE_SECS',
        'RUST_LOG', 'HF_HOME', 'SS_MODEL_DIR',
    }
    return {key: value for key, value in environment.items() if key in recorded_names}


def invocation_id():
    return str(uuid.uuid4())


def fixture_oracle(scope, environment, expected_manifest=None):
    if expected_manifest is not None:
        observed_manifest = fixture_manifest(scope)
        if observed_manifest != expected_manifest:
            raise RuntimeError({'expected_manifest': expected_manifest,
                                'observed_manifest': observed_manifest})
    reference = run(['rg', '--files', scope], environment)
    if reference['returncode'] != 0:
        raise RuntimeError({'rg_returncode': reference['returncode'], 'reference': reference})
    rg_paths = reference['stdout'].splitlines()
    if expected_manifest is None:
        expected = {str(Path(line).resolve()) for line in rg_paths}
    else:
        root = Path(scope).resolve()
        expected = {str((root / entry['path']).resolve())
                    for entry in expected_manifest['entries']}
        observed = {str(Path(line).resolve()) for line in rg_paths}
        if observed != expected:
            raise RuntimeError({'rg_paths': sorted(observed),
                                'manifest_paths': sorted(expected)})
    if not fixture_membership(rg_paths, expected):
        raise RuntimeError({'rg_paths': rg_paths, 'expected': sorted(expected)})
    if fixture_membership([], expected):
        raise RuntimeError('empty path control was accepted')
    return expected, reference


def query(scope, environment, expected, text='bounded worker'):
    result = run([BIN / 'ss', '--json', '--path', scope, text], environment)
    assert result['returncode'] == 0, result
    payload = json.loads(result['stdout'])
    paths = list(payload_paths(payload))
    assert fixture_membership(paths, expected), payload
    return {'execution': result, 'payload': payload, 'actual_paths': paths,
            'fixture_membership_passed': True}


def environment_for(home):
    environment = {key: value for key, value in os.environ.items() if not key.startswith('SS_')}
    environment.update(SS_HOME=str(home), SS_EMBEDDER='fastembed', SS_EMBED_PROVIDER='cpu',
                       SS_EMBED_MODEL='bge-small-en-v1.5', SS_EMBED_MAX_LENGTH='256',
                       SS_WARM_START='false', RUST_LOG='warn,ss=debug')
    return environment


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def binary_pair_identity():
    client = BIN / 'ss'
    daemon = BIN / 'ssd'
    return {'ss_path': str(client.resolve()), 'ss_sha256': digest(client),
            'ssd_path': str(daemon.resolve()), 'ssd_sha256': digest(daemon)}


def append(path, **record):
    record['timestamp_ns'] = time.time_ns()
    with path.open('a') as output:
        output.write(json.dumps(record) + '\n')
