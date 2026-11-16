import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
SUPPORT = HERE.parent / 'search-resource-cost' / 'process_support.py'
specification = importlib.util.spec_from_file_location('process_support', SUPPORT)
helpers = importlib.util.module_from_spec(specification)
specification.loader.exec_module(helpers)
FIXTURE = HERE.parent / 'search-resource-cost' / 'fixtures' / 'owner_messages_md'
FIXTURE_MANIFEST = HERE.parent / 'search-resource-cost' / 'fixtures' / 'owner_messages_md.manifest.json'
PINNED_TREE_SHA256 = '51edf3bc61fb78a25786e47be21b22b2658412eaee6f8d3e5f6ae444d9e4bb33'
PINNED_FILE_COUNT = 123
PINNED_TOTAL_BYTES = 21272
MAINTENANCE_BODY = 'idle maintenance complete'
MODEL_DROP_BODY = 'embedding model released because worker is idle'


def validate_fixture_snapshot():
    expected_manifest = json.loads(FIXTURE_MANIFEST.read_text())
    if (
        expected_manifest.get('file_count') != PINNED_FILE_COUNT
        or expected_manifest.get('total_bytes') != PINNED_TOTAL_BYTES
        or expected_manifest.get('tree_sha256') != PINNED_TREE_SHA256
    ):
        raise RuntimeError(f'pinned fixture manifest has unexpected metadata: {expected_manifest}')
    observed_manifest = helpers.fixture_manifest(FIXTURE)
    if observed_manifest != expected_manifest:
        raise RuntimeError({
            'fixture_path': str(FIXTURE),
            'expected_manifest': expected_manifest,
            'observed_manifest': observed_manifest,
        })
    return expected_manifest


def model_drop_events(text):
    events = helpers.telemetry_events(
        text,
        MODEL_DROP_BODY,
        module_path='ss::embed',
    )
    for event in events:
        tags = event.get('tags')
        if not isinstance(tags, dict) or tags.get('lane') not in ('Index', 'Query'):
            raise RuntimeError(f'model-drop event has invalid lane tag: {event}')
        if type(event.get('ts_ns')) is not int:
            raise RuntimeError(f'model-drop event has invalid timestamp: {event}')
    return events


def maintenance_events(text):
    return helpers.telemetry_events(
        text,
        MAINTENANCE_BODY,
        module_path='ss::daemon',
    )


def validate_maintenance_events(events, compact_seconds):
    expected_count = 1 if compact_seconds else 0
    if len(events) != expected_count:
        raise RuntimeError({
            'compact_seconds': compact_seconds,
            'expected_maintenance_event_count': expected_count,
            'actual_maintenance_events': events,
        })
    for event in events:
        tags = event.get('tags')
        if not isinstance(tags, dict):
            raise RuntimeError(f'maintenance event has no structured tags: {event}')
        required_integer_tags = (
            'allocator_released_bytes', 'compacted_roots', 'pending_roots',
            'activity_epoch', 'idle_ms'
        )
        for name in required_integer_tags:
            if type(tags.get(name)) is not int or tags[name] < 0:
                raise RuntimeError(f'maintenance tag {name} is not a nonnegative integer: {event}')
        if type(tags.get('allocator_supported')) is not bool:
            raise RuntimeError(f'maintenance allocator_supported tag is not boolean: {event}')
    return events


def capture_run(results, invocation, compact_seconds, repetition, manifest, expected_pair):
    label = f'{invocation}-compact-{compact_seconds}-r{repetition}'
    environment = None
    recorded_environment = None
    console_path = HERE / f'{label}-console.log'
    daemon_log_path = HERE / f'{label}-ssd.log'
    binary_pair_before = helpers.binary_pair_identity()
    run_record = None
    daemon = None

    try:
        if binary_pair_before != expected_pair:
            raise RuntimeError({
                'run_id': label,
                'expected_binary_pair': expected_pair,
                'actual_binary_pair': binary_pair_before,
            })
        with tempfile.TemporaryDirectory(prefix='ic-') as temporary:
            home = Path(temporary) / 'home'
            environment = helpers.environment_for(home)
            environment.update(
                SS_EMBED_QUERY_WORKERS='1',
                SS_EMBED_WORKERS='2',
                SS_EMBED_THREADS='2',
                SS_EMBED_IDLE_SECS='1',
                SS_IDLE_COMPACT_SECS=str(compact_seconds),
                SS_IDLE_SECS='3600',
            )
            recorded_environment = helpers.experiment_environment(environment)
            with console_path.open('w') as console:
                daemon = subprocess.Popen(
                    [str(helpers.BIN / 'ssd'), 'serve'],
                    env=environment,
                    stdout=console,
                    stderr=subprocess.STDOUT,
                )
                try:
                    ready = helpers.wait_ready(environment, daemon)
                    before_loaded = helpers.process_sample(daemon.pid)
                    fixture_manifest = validate_fixture_snapshot()
                    expected, reference = helpers.fixture_oracle(
                        FIXTURE,
                        environment,
                        expected_manifest=manifest,
                    )
                    indexed = helpers.run([helpers.BIN / 'ss', 'index', FIXTURE], environment)
                    if indexed['returncode'] != 0:
                        raise RuntimeError(f'index command failed: {indexed}')
                    before_query = helpers.query(FIXTURE, environment, expected)
                    loaded = helpers.process_sample(daemon.pid)

                    log = home / 'logs' / 'ssd.log'
                    dropped, drop_contents = helpers.wait_log(
                        log,
                        lambda text: any(
                            event['tags']['lane'] == 'Query'
                            for event in model_drop_events(text)
                        ),
                        15,
                    )
                    if not dropped:
                        raise RuntimeError(f'query model drop event was not observed: {drop_contents}')
                    dropped_events = model_drop_events(drop_contents)
                    if not any(event['tags']['lane'] == 'Query' for event in dropped_events):
                        raise RuntimeError(f'query model-drop event missing: {dropped_events}')
                    after_model_drop = helpers.process_sample(daemon.pid)

                    maintenance_observed, contents = helpers.wait_log(
                        log,
                        lambda text: bool(maintenance_events(text)),
                        10 if compact_seconds else 4,
                    )
                    if maintenance_observed != bool(compact_seconds):
                        raise RuntimeError({
                            'compact_seconds': compact_seconds,
                            'maintenance_observed': maintenance_observed,
                            'log_contents': contents,
                        })
                    captured_maintenance = validate_maintenance_events(
                        maintenance_events(contents),
                        compact_seconds,
                    )
                    after_maintenance = helpers.process_sample(daemon.pid)
                    after_query = helpers.query(FIXTURE, environment, expected)
                    observed_query_paths = (
                        before_query['actual_paths'] + after_query['actual_paths']
                    )
                    fixture_misses = sum(path not in expected for path in observed_query_paths)
                    binary_pair_after = helpers.binary_pair_identity()
                    binary_pair_unchanged = binary_pair_before == binary_pair_after
                    run_record = {
                        'event': 'run',
                        'invocation_id': invocation,
                        'run_id': label,
                        'status': 'captured' if binary_pair_unchanged else 'artifact_changed_during_run',
                        'compact_seconds': compact_seconds,
                        'repetition': repetition,
                        'environment': recorded_environment,
                        'fixture': str(FIXTURE),
                        'fixture_manifest': fixture_manifest,
                        'fixture_file_count': fixture_manifest['file_count'],
                        'fixture_total_bytes': fixture_manifest['total_bytes'],
                        'fixture_tree_sha256': fixture_manifest['tree_sha256'],
                        'binary_pair_before': binary_pair_before,
                        'binary_pair_after': binary_pair_after,
                        'binary_pair_unchanged_during_run': binary_pair_unchanged,
                        'daemon_process_start': helpers.process_sample(daemon.pid),
                        'ready': ready,
                        'independent_rg': reference,
                        'rg_incumbent_accepted_by_shared_checker': bool(reference['stdout'].splitlines()),
                        'empty_control_rejected_by_shared_checker': not helpers.fixture_membership([], expected),
                        'indexed': indexed,
                        'before_loaded_ps': before_loaded,
                        'before_query': before_query,
                        'loaded_ps': loaded,
                        'model_drop_events': dropped_events,
                        'after_model_drop_ps': after_model_drop,
                        'maintenance_events': captured_maintenance,
                        'after_maintenance_ps': after_maintenance,
                        'after_query': after_query,
                        'fixture_misses': fixture_misses,
                        'query_payload_count': len(observed_query_paths),
                        'model_drop_event_count': len(dropped_events),
                        'maintenance_event_count': len(captured_maintenance),
                    }
                    helpers.append(results, **run_record)
                    if not binary_pair_unchanged:
                        raise RuntimeError({
                            'run_id': label,
                            'binary_pair_before': binary_pair_before,
                            'binary_pair_after': binary_pair_after,
                        })
                finally:
                    if daemon.poll() is None:
                        helpers.run([helpers.BIN / 'ssd', 'stop'], environment, 10)
                        try:
                            daemon.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            daemon.terminate()
                            daemon.wait(timeout=10)
                    if (home / 'logs' / 'ssd.log').exists():
                        shutil.copy2(home / 'logs' / 'ssd.log', daemon_log_path)
    except Exception as error:
        helpers.append(
            results,
            event='run_failure',
            invocation_id=invocation,
            run_id=label,
            compact_seconds=compact_seconds,
            repetition=repetition,
            environment=recorded_environment,
            binary_pair_before=binary_pair_before,
            binary_pair_after=helpers.binary_pair_identity(),
            error_type=type(error).__name__,
            error=str(error),
        )
        raise
    return run_record


def main():
    results = HERE / 'probe.jsonl'
    manifest = validate_fixture_snapshot()
    invocation = helpers.invocation_id()
    binary_pair_at_start = helpers.binary_pair_identity()
    helpers.append(
        results,
        event='invocation_start',
        invocation_id=invocation,
        fixture=str(FIXTURE),
        fixture_manifest=manifest,
        binary_pair=binary_pair_at_start,
        settings={'model_idle_seconds': 1, 'compact_seconds': [0, 2], 'repetitions_per_setting': 3},
    )
    completed = []
    for compact_seconds in (0, 2):
        for repetition in range(1, 4):
            completed.append(capture_run(
                results, invocation, compact_seconds, repetition, manifest, binary_pair_at_start
            ))
    query_payloads = sum(run['query_payload_count'] for run in completed)
    fixture_misses = sum(run['fixture_misses'] for run in completed)
    helpers.append(
        results,
        event='counts',
        invocation_id=invocation,
        completed_process_repetitions=len(completed),
        query_payloads=query_payloads,
        fixture_misses=fixture_misses,
        maintenance_events=sum(run['maintenance_event_count'] for run in completed),
        binary_pair_unchanged=all(run['binary_pair_unchanged_during_run'] for run in completed),
    )
    print(
        f'invocation_id={invocation} process_repetitions={len(completed)} '
        f'query_payloads={query_payloads} fixture_misses={fixture_misses}'
    )


if __name__ == '__main__':
    main()
