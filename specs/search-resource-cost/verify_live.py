import json
import os
from pathlib import Path
import re
import select
import sqlite3
import subprocess
import time

import process_support as helpers

HERE = Path(__file__).resolve().parent
HOME = Path('/Users/brianbruggeman/Library/Caches/ss')
OWNER_SCOPE = Path('/Users/brianbruggeman/repos/slot-0/.long_ctx_backups/owner_messages_md')
MEMORY_SCOPE = Path('/Users/brianbruggeman/.claude/projects/-Users-brianbruggeman-repos-slot-0/memory')
RESULTS = HERE / 'live.jsonl'


def legacy_databases():
    captured = {}
    for path in sorted((HOME / 'roots').glob('*/index.db')):
        connection = sqlite3.connect(f'file:{path}?mode=ro', uri=True)
        meta = connection.execute('SELECT k,v FROM meta ORDER BY k').fetchall()
        rows = connection.execute('SELECT path,size FROM files ORDER BY path LIMIT 3').fetchall()
        chunks = connection.execute('SELECT path,text FROM chunks ORDER BY id LIMIT 1').fetchall()
        connection.close()
        captured[str(path)] = {'sha256': helpers.digest(path), 'meta': meta,
                               'actual_file_rows': rows, 'actual_chunk_rows': chunks}
    return captured


def stop_old(environment):
    current = helpers.status(environment)
    if current['returncode'] != 0:
        return current
    match = re.search(r'pid=(\d+)', current['stdout'])
    assert match, current
    pid = int(match.group(1))
    queue = select.kqueue()
    try:
        event = select.kevent(pid, filter=select.KQ_FILTER_PROC,
                              flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT,
                              fflags=select.KQ_NOTE_EXIT)
        queue.control([event], 0, 0)
        stopped = helpers.run([helpers.BIN / 'ssd', 'stop'], environment, 10)
        assert stopped['returncode'] == 0, stopped
        events = queue.control(None, 1, 20)
        assert events, f'old daemon {pid} did not exit'
        return {'before': current, 'stop': stopped}
    finally:
        queue.close()


def main():
    environment = helpers.environment_for(HOME)
    environment.update(SS_EMBED_IDLE_SECS='30', SS_IDLE_COMPACT_SECS='60')
    stopped = stop_old(environment)
    before = legacy_databases()
    assert before, 'expected preserved legacy databases'
    helpers.append(RESULTS, event='legacy-before-rollout', stopped=stopped, databases=before,
                   registry=json.loads((HOME / 'state.json').read_text()))
    started_ns = time.time_ns()
    console_path = HERE / 'live-console.log'
    with console_path.open('a') as console:
        daemon = subprocess.Popen([str(helpers.BIN / 'ssd'), 'serve', '--setsid'],
                                  env=environment, stdout=console, stderr=subprocess.STDOUT)
    ready = helpers.wait_ready(environment, daemon)
    executable = (helpers.BIN / 'ssd').resolve()
    mapped = subprocess.check_output(['lsof', '-a', '-p', str(daemon.pid), '-d', 'txt', '-Fn'], text=True)
    assert f'n{executable}\n' in mapped, mapped
    helpers.append(RESULTS, event='live-executable', pid=daemon.pid, ready=ready,
                   actual_process=helpers.process_sample(daemon.pid), mapped_executable=mapped,
                   executable=str(executable), built_sha256=helpers.digest(executable),
                   environment=helpers.experiment_environment(environment))
    completions = []
    payloads = []
    for scope in (OWNER_SCOPE, MEMORY_SCOPE):
        assert scope.is_dir(), scope
        expected, reference = helpers.fixture_oracle(scope, environment)
        indexed = helpers.run([helpers.BIN / 'ss', 'index', scope], environment, 180)
        assert indexed['returncode'] == 0, indexed
        assert re.search(r'indexed \d+ files', indexed['stdout']), indexed
        completions.append({'scope': str(scope), 'result': indexed})
        payload = helpers.query(scope, environment, expected)
        payloads.append({'scope': str(scope), 'independent_rg': reference, 'query': payload})
    helpers.append(RESULTS, event='live-scoped-indexes-and-queries', completions=completions,
                   actual_payloads=payloads, process=helpers.process_sample(daemon.pid))
    log = HOME / 'logs' / 'ssd.log'

    def fresh_maintenance(text):
        for line in text.splitlines():
            if 'idle maintenance complete' not in line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if record.get('ts_ns', 0) >= started_ns:
                return True
        return False

    observed, contents = helpers.wait_log(log, fresh_maintenance, 90)
    assert observed, 'live idle maintenance did not produce its event'
    maintenance = [json.loads(line) for line in contents.splitlines()
                   if 'idle maintenance complete' in line and json.loads(line).get('ts_ns', 0) >= started_ns]
    after = legacy_databases()
    assert before == after, 'legacy database contents changed during corrected-profile rollout'
    helpers.append(RESULTS, event='live-idle-maintenance', actual_maintenance=maintenance,
                   process=helpers.process_sample(daemon.pid), legacy_databases=after,
                   legacy_preserved=True, completed_scoped_indexes=len(completions),
                   query_payloads=len(payloads), pid=daemon.pid,
                   executable_sha256=helpers.digest(executable))
    print(f'live_executable=1 scoped_indexes={len(completions)} query_payloads={len(payloads)} idle_maintenance=1 legacy_preserved=true pid={daemon.pid}')


if __name__ == '__main__':
    main()
