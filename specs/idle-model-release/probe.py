#!/usr/bin/env python3
"""Capture two isolated real-daemon idle-model experiments on macOS."""
import contextlib
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import subprocess
import tempfile
import time

HERE = Path(__file__).resolve().parent
BIN = Path('/Users/brianbruggeman/repos/slot-0/.cargo_target_ss/release')
WINDOW = 5.0
FIXTURE = 'Resource lifecycle\n\nA bounded worker waits for requests and releases its work after completion.\n'
QUERY = 'bounded worker releases work'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, env, limit=120):
    started = time.monotonic()
    result = subprocess.run([str(part) for part in command], env=env, text=True,
                            capture_output=True, timeout=limit)
    return {'command': [str(part) for part in command], 'returncode': result.returncode,
            'stdout': result.stdout, 'stderr': result.stderr,
            'elapsed_seconds_measured': time.monotonic() - started}


def sample(pid):
    return subprocess.run(['ps', '-p', str(pid), '-o', 'pid,ppid,rss,time,command'],
                          text=True, capture_output=True, check=True).stdout


def wait_log(path, predicate, duration):
    """Wait for file writes or deadline, with no periodic polling."""
    deadline = time.monotonic() + duration
    with path.open('rb') as stream, contextlib.closing(select.kqueue()) as queue:
        event = select.kevent(stream.fileno(), filter=select.KQ_FILTER_VNODE,
                              flags=select.KQ_EV_ADD | select.KQ_EV_CLEAR,
                              fflags=select.KQ_NOTE_WRITE | select.KQ_NOTE_EXTEND)
        queue.control([event], 0, 0)
        while True:
            text = path.read_text(errors='replace')
            if predicate(text):
                return True, text
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False, text
            queue.control(None, 1, remaining)


def record(output, **fields):
    fields['timestamp_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    fields['monotonic_seconds'] = time.monotonic()
    output.write(json.dumps(fields, sort_keys=True) + '\n')
    output.flush()


def identifies_fixture(value):
    if isinstance(value, dict):
        return any(identifies_fixture(item) for item in value.values())
    if isinstance(value, list):
        return any(identifies_fixture(item) for item in value)
    return isinstance(value, str) and 'example.md' in value


def query(corpus, env):
    result = run([BIN / 'ss', '--json', '--path', corpus, QUERY], env)
    if result['returncode'] != 0:
        raise RuntimeError(f'query failed: {result}')
    result['payload'] = json.loads(result['stdout'])
    if not identifies_fixture(result['payload']):
        raise RuntimeError(f'query payload missing fixture: {result}')
    return result


def arm(name, idle, output):
    with tempfile.TemporaryDirectory(prefix='ss-idle-probe-') as directory:
        root = Path(directory)
        home = root / 'home'
        corpus = root / 'corpus'
        corpus.mkdir()
        (corpus / 'example.md').write_text(FIXTURE)
        incumbent = run(['rg', '--files', corpus], os.environ)
        incumbent_paths = incumbent['stdout'].splitlines()
        incumbent_accepts = incumbent['returncode'] == 0 and identifies_fixture(incumbent_paths)
        empty_rejected = not identifies_fixture([])
        record(output, arm=name, event='fixture_membership_incumbent', command=incumbent['command'],
               returncode=incumbent['returncode'], raw_stdout=incumbent['stdout'], paths=incumbent_paths,
               fixture_accepted=incumbent_accepts, empty_paths_rejected=empty_rejected,
               interpretation='fixture membership only; not a ranking oracle')
        if not incumbent_accepts or not empty_rejected:
            raise RuntimeError('fixture membership incumbent or empty-set control failed')
        env = {key: value for key, value in os.environ.items() if not key.startswith('SS_')}
        env.update(SS_HOME=str(home), SS_EMBEDDER='fastembed', SS_EMBED_MODEL='bge-small-en-v1.5',
                   SS_EMBED_PROVIDER='cpu', SS_WARM_START='false', SS_IDLE_SECS='60',
                   SS_EMBED_IDLE_SECS=str(idle), RUST_LOG='warn,ss=debug')
        daemon_stdout = HERE / f'probe-{name}-console.log'
        log_copy = HERE / f'probe-{name}-ssd.log'
        with daemon_stdout.open('w') as console:
            daemon = subprocess.Popen([str(BIN / 'ssd'), 'serve'], env=env,
                                      stdout=console, stderr=subprocess.STDOUT)
            try:
                # Status is read-only and does not spawn a daemon. Each invocation blocks until completion.
                deadline = time.monotonic() + 20
                while True:
                    status = run([BIN / 'ssd', 'status'], env, 5)
                    if status['returncode'] == 0:
                        break
                    if daemon.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f'daemon startup failed: {status}')
                log = home / 'logs' / 'ssd.log'
                record(output, arm=name, event='started', pid=daemon.pid,
                       executable_ps=sample(daemon.pid),
                       executable_lsof=subprocess.run(['lsof', '-a', '-p', str(daemon.pid), '-d', 'txt', '-Fn'], text=True, capture_output=True).stdout, status=status,
                       home=str(home), fixture=FIXTURE, fixture_sha256=digest(corpus / 'example.md'),
                       timeout_seconds=idle, model='bge-small-en-v1.5', provider='cpu')
                indexed = run([BIN / 'ss', 'index', corpus], env)
                record(output, arm=name, event='index', **indexed)
                if indexed['returncode'] != 0 or not re.search(r'indexed 1 files .*1 embedded', indexed['stdout']):
                    raise RuntimeError(f'index did not produce one embedded file: {indexed}')
                record(output, arm=name, event='post_index', ps_raw=sample(daemon.pid))
                first = query(corpus, env)
                record(output, arm=name, event='query_before', **first)
                if first['returncode'] != 0 or 'example.md' not in first['stdout']:
                    raise RuntimeError(f'first query missing fixture: {first}')
                record(output, arm=name, event='post_query_loaded', ps_raw=sample(daemon.pid))
                def released(text):
                    return any('embedding model released because worker is idle' in line and (idle != 1 or 'Query' in line) for line in text.splitlines())
                window_started = time.monotonic()
                observed, contents = wait_log(log, released, WINDOW)
                window_elapsed = time.monotonic() - window_started
                release_lines = [line for line in contents.splitlines() if 'embedding model released because worker is idle' in line]
                record(output, arm=name, event='release_window', maximum_duration_seconds=WINDOW, actual_duration_seconds_measured=window_elapsed,
                       observed=observed, release_lines=release_lines, ps_raw=sample(daemon.pid),
                       interpretation='release-site telemetry after Rust model drop; native allocator retention is not observed')
                if observed != (idle == 1):
                    raise RuntimeError(f'unexpected release observation for {name}: {release_lines}')
                before = len([line for line in contents.splitlines() if 'embedding model loading because work arrived' in line])
                second = query(corpus, env)
                record(output, arm=name, event='query_after', **second)
                if second['returncode'] != 0 or 'example.md' not in second['stdout']:
                    raise RuntimeError(f'second query missing fixture: {second}')
                if idle == 1:
                    # Query must have expired, not merely an index model: verify its release then reload events.
                    def query_reloaded(text):
                        loads = [line for line in text.splitlines() if 'embedding model loading because work arrived' in line]
                        return len(loads) > before and any('Query' in line for line in release_lines)
                    observed_reload, _ = wait_log(log, query_reloaded, WINDOW)
                    if not observed_reload:
                        raise RuntimeError('query release and reload events were not observed')
                record(output, arm=name, event='post_query_after', ps_raw=sample(daemon.pid))
            finally:
                if daemon.poll() is None:
                    run([BIN / 'ssd', 'stop'], env, 10)
                    try:
                        daemon.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        daemon.terminate()
                        daemon.wait(timeout=10)
                if (home / 'logs' / 'ssd.log').exists():
                    shutil.copyfile(home / 'logs' / 'ssd.log', log_copy)


def main():
    with (HERE / 'probe.jsonl').open('w') as output:
        record(output, event='binaries', ss=str(BIN / 'ss'), ss_sha256=digest(BIN / 'ss'),
               ssd=str(BIN / 'ssd'), ssd_sha256=digest(BIN / 'ssd'),
               load_average=os.getloadavg(), query=QUERY, window_seconds=WINDOW)
        arm('retained', 3600, output)
        arm('expired', 1, output)
        record(output, event='counts', arms=2, index_completions=2, query_payloads=4,
               expired_release_events='>0', retained_controls_rejected=1)
    print('arms=2 index_completions=2 query_payloads=4 release_events>0 controls_rejected=1')


if __name__ == '__main__':
    main()
