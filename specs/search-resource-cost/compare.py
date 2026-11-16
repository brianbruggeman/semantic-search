#!/usr/bin/env python3
"""Append isolated process measurements for the search-resource-cost spec."""

from __future__ import annotations

import argparse
import copy
import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
import threading
import time
import uuid


HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
SNAPSHOT = FIXTURES / "owner_messages_md"
OUTPUTS = HERE / "outputs"
JOURNAL = OUTPUTS / "compare.jsonl"
MODEL_CACHE = OUTPUTS / ".model-cache"
BASELINE = Path("/private/tmp/ss-idle-rollback-20261102")
CORRECTED = Path("/Users/brianbruggeman/repos/slot-0/.cargo_target_ss/release")
CORRECTED_PRESERVED = Path("/private/tmp/ss-cpu-policy-20261102")
TINY_TEXT = (
    "Resource lifecycle\n\n"
    "A bounded worker waits for requests and releases its work after completion.\n"
)
TINY_QUERY = "bounded worker releases work"
WARM_QUERY_COUNT = 8
HARNESS_VERSION = 2
SAMPLE_INTERVAL_SECONDS = 0.2
COMMAND_TIMEOUT_SECONDS = 600
COREML_ENGINE_CLASS_PATTERN = re.compile(
    r"\s(MLNeuralNetwork(?:V1)?Engine)\s+(?:ObjC|C\+\+|C)\s+CoreML\s*$"
)
INDEX_PATTERN = re.compile(
    r"indexed (\d+) files? \((\d+) unchanged, (\d+) removed\): "
    r"(\d+) chunks, (\d+) embedded, (\d+) skipped"
)


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def tree_manifest(root: Path) -> dict:
    files = sorted(path for path in root.rglob("*") if path.is_file())
    entries = [
        {
            "path": path.relative_to(root).as_posix(),
            "bytes": path.stat().st_size,
            "sha256": sha256_file(path),
        }
        for path in files
    ]
    canonical = "\n".join(f"{entry['path']} {entry['sha256']}" for entry in entries)
    return {
        "file_count": len(entries),
        "total_bytes": sum(entry["bytes"] for entry in entries),
        "tree_sha256": hashlib.sha256(canonical.encode()).hexdigest(),
        "entries": entries,
    }


def append_record(event: str, **fields: object) -> dict:
    record = {
        "event": event,
        "record_id": str(uuid.uuid4()),
        "timestamp_utc": utc_now(),
        **fields,
    }
    OUTPUTS.mkdir(parents=True, exist_ok=True)
    with JOURNAL.open("a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True, ensure_ascii=False) + "\n")
        output.flush()
        os.fsync(output.fileno())
    return record


def command_result(command: list[str | Path], env: dict[str, str], timeout: int = COMMAND_TIMEOUT_SECONDS) -> dict:
    started = time.monotonic()
    completed = subprocess.run(
        [str(item) for item in command],
        env=env,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )
    return {
        "command": [str(item) for item in command],
        "returncode": completed.returncode,
        "stdout": completed.stdout,
        "stderr": completed.stderr,
        "elapsed_seconds_measured": time.monotonic() - started,
    }


def clean_environment(
    home: Path,
    provider: str,
    daemon_binary: Path,
    stack_logging: bool = False,
) -> dict[str, str]:
    environment = {
        key: value for key, value in os.environ.items() if not key.startswith("SS_")
    }
    environment.update(
        {
            "SS_HOME": str(home),
            "SS_DAEMON_BIN": str(daemon_binary),
            "SS_EMBEDDER": "fastembed",
            "SS_EMBED_MODEL": "bge-small-en-v1.5",
            "SS_EMBED_PROVIDER": provider,
            "SS_EMBED_MAX_LENGTH": "256",
            "SS_EMBED_QUERY_WORKERS": "1",
            "SS_EMBED_WORKERS": "2",
            "SS_EMBED_THREADS": "2",
            "SS_WARM_START": "false",
            "SS_EMBED_IDLE_SECS": "3600",
            "SS_IDLE_SECS": "3600",
            "RUST_LOG": "warn,ss=debug",
        }
    )
    if stack_logging:
        environment["MallocStackLogging"] = "1"
    else:
        environment.pop("MallocStackLogging", None)
    return environment


def ps_snapshot(pid: int) -> dict | None:
    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "pid=,rss=,%cpu=,time=,etime=,command="],
        text=True,
        capture_output=True,
        check=False,
    )
    line = result.stdout.strip()
    if result.returncode != 0 or not line:
        return None
    fields = line.split(None, 5)
    if len(fields) < 6:
        return None
    return {
        "pid": int(fields[0]),
        "rss_kb": int(fields[1]),
        "cpu_percent_ps": float(fields[2]),
        "cpu_time_raw": fields[3],
        "elapsed_raw": fields[4],
        "command": fields[5],
    }


def cpu_seconds(raw: str) -> float:
    days_text, separator, clock = raw.partition("-")
    if not separator:
        clock = days_text
        days_text = "0"
    parts = clock.split(":")
    if len(parts) not in (2, 3):
        raise ValueError(f"unrecognized ps CPU time: {raw!r}")
    if len(parts) == 3:
        hours, minutes, seconds = parts
        clock_seconds = int(hours) * 3600 + int(minutes) * 60 + float(seconds)
    else:
        minutes, seconds = parts
        clock_seconds = int(minutes) * 60 + float(seconds)
    return (int(days_text) * 86400 if separator else 0) + clock_seconds


class ProcessSampler:
    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.samples: list[dict] = []
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self._sample_loop, name=f"ps-sampler-{pid}")

    def _sample_loop(self) -> None:
        while not self.stopped.is_set():
            sample = ps_snapshot(self.pid)
            if sample is not None:
                sample["monotonic_seconds"] = time.monotonic()
                self.samples.append(sample)
            self.stopped.wait(SAMPLE_INTERVAL_SECONDS)

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> list[dict]:
        self.stopped.set()
        self.thread.join()
        return self.samples


def host_loadout() -> dict:
    processes = subprocess.run(
        ["ps", "-A", "-o", "%cpu=,comm="],
        text=True,
        capture_output=True,
        check=False,
    )
    tenants = []
    cpu_sum = 0.0
    if processes.returncode == 0:
        for line in processes.stdout.splitlines():
            fields = line.strip().split(None, 1)
            if len(fields) != 2:
                continue
            try:
                cpu = float(fields[0])
            except ValueError:
                continue
            cpu_sum += cpu
            tenants.append({"cpu_percent_ps": cpu, "command": fields[1]})
    tenants.sort(key=lambda value: value["cpu_percent_ps"], reverse=True)
    try:
        load_average = os.getloadavg()
    except OSError:
        load_average = None
    return {
        "load_average_1_5_15": load_average,
        "logical_cpu_count": os.cpu_count(),
        "process_cpu_percent_sum_ps": cpu_sum,
        "top_processes_ps": tenants[:12],
    }


def path_matches_fixture(value: object, expected_path: str) -> bool:
    if isinstance(value, dict):
        for key, item in value.items():
            if key == "path" and isinstance(item, str):
                normalized_item = item.replace("\\", "/")
                normalized_expected = expected_path.replace("\\", "/").strip("/")
                if (
                    normalized_item == normalized_expected
                    or normalized_item.endswith("/" + normalized_expected)
                ):
                    return True
            if path_matches_fixture(item, expected_path):
                return True
    elif isinstance(value, list):
        return any(path_matches_fixture(item, expected_path) for item in value)
    elif isinstance(value, str):
        normalized_value = value.replace("\\", "/")
        normalized_expected = expected_path.replace("\\", "/").strip("/")
        return (
            normalized_value == normalized_expected
            or normalized_value.endswith("/" + normalized_expected)
        )
    return False


def percentile(values: list[float], percentile_value: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = percentile_value * (len(ordered) - 1)
    low = math.floor(rank)
    high = math.ceil(rank)
    return ordered[low] + (ordered[high] - ordered[low]) * (rank - low)


def latency_summary(values: list[float]) -> dict:
    return {
        "request_count": len(values),
        "p50_seconds": percentile(values, 0.50),
        "p90_seconds": percentile(values, 0.90),
        "p99_seconds": percentile(values, 0.99),
        "p99_9_seconds": percentile(values, 0.999),
        "max_seconds": max(values) if values else None,
        "raw_seconds": values,
    }


def coefficient_of_variation(values: list[float]) -> float | None:
    if len(values) < 2:
        return None
    mean = statistics.mean(values)
    if mean == 0:
        return None
    return statistics.pstdev(values) / abs(mean)


def prepare_fixtures() -> dict:
    if not SNAPSHOT.is_dir():
        raise FileNotFoundError(f"snapshot fixture is missing: {SNAPSHOT}")
    tiny_dir = FIXTURES / "tiny"
    tiny_dir.mkdir(parents=True, exist_ok=True)
    tiny_path = tiny_dir / "example.md"
    if tiny_path.exists() and tiny_path.read_text(encoding="utf-8") != TINY_TEXT:
        raise RuntimeError(f"tiny fixture changed after creation: {tiny_path}")
    tiny_path.write_text(TINY_TEXT, encoding="utf-8")
    manifest = tree_manifest(SNAPSHOT)
    if manifest["file_count"] != 123 or manifest["total_bytes"] != 21272:
        raise RuntimeError(f"owner_messages snapshot count/size changed: {manifest}")
    candidates = [
        path for path in sorted(SNAPSHOT.rglob("*.md"))
        if path.is_file() and path.stat().st_size >= 120
    ]
    if not candidates:
        raise RuntimeError("owner_messages snapshot has no query fixture candidate")
    target = candidates[0]
    text = target.read_text(encoding="utf-8", errors="replace")
    paragraph = next((item.strip() for item in re.split(r"\n\s*\n", text) if len(item.split()) >= 6), "")
    words = re.findall(r"[A-Za-z0-9_'-]+", paragraph)
    if len(words) < 6:
        raise RuntimeError(f"no usable query text in fixture: {target}")
    query = " ".join(words[:12])
    fixture_info = {
        "tiny": {
            "path": str(tiny_path),
            "relative_path": "example.md",
            "query": TINY_QUERY,
            "bytes": len(TINY_TEXT.encode()),
            "sha256": hashlib.sha256(TINY_TEXT.encode()).hexdigest(),
        },
        "owner_messages": {
            "root": str(SNAPSHOT),
            "file_count": manifest["file_count"],
            "total_bytes": manifest["total_bytes"],
            "tree_sha256": manifest["tree_sha256"],
            "target_path": target.relative_to(SNAPSHOT).as_posix(),
            "target_sha256": sha256_file(target),
            "query": query,
        },
    }
    manifest_path = FIXTURES / "owner_messages_md.manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return fixture_info


def fixture_membership_incumbent(corpus: Path, expected_relative_path: str) -> dict:
    result = command_result(["rg", "--files", corpus], os.environ)
    paths = [line.strip() for line in result["stdout"].splitlines() if line.strip()]
    accepted = result["returncode"] == 0 and path_matches_fixture(paths, expected_relative_path)
    empty_rejected = not path_matches_fixture([], expected_relative_path)
    record = {
        "command": result["command"],
        "returncode": result["returncode"],
        "stdout": result["stdout"],
        "stderr": result["stderr"],
        "paths_count": len(paths),
        "fixture_path": expected_relative_path,
        "fixture_accepted": accepted,
        "empty_fixture_list_rejected": empty_rejected,
        "interpretation": "path-membership assertion only; not a ranking oracle",
    }
    if not accepted or not empty_rejected:
        raise RuntimeError(f"independent rg fixture check failed: {record}")
    return record


def replay_saved_fixture_assertions() -> dict:
    journal_records = read_journal_records()
    incumbent_by_run: dict[str, dict] = {}
    for item in journal_records:
        if item.get("event") == "fixture_incumbent" and item.get("run_id"):
            incumbent_by_run[item["run_id"]] = item
    unique_runs: dict[str, dict] = {}
    for item in journal_records:
        if item.get("event") != "run" or item.get("status") != "captured":
            continue
        run_id = item["run_id"]
        current = unique_runs.get(run_id)
        if current is None or (
            current.get("derived_from_record_id") is not None
            and item.get("derived_from_record_id") is None
        ):
            unique_runs[run_id] = item
    checks = []
    for run_id, row in sorted(unique_runs.items()):
        expected_path = (
            row["input_fixture"].get("relative_path")
            or row["input_fixture"]["target_path"]
        )
        incumbent = incumbent_by_run.get(run_id)
        if incumbent is None:
            raise RuntimeError(f"saved run has no rg incumbent record: {run_id}")
        membership = incumbent["fixture_membership"]
        paths = [line.strip() for line in membership["stdout"].splitlines() if line.strip()]
        incumbent_match = membership["returncode"] == 0 and path_matches_fixture(paths, expected_path)
        empty_rejected = not path_matches_fixture([], expected_path)
        completed_queries = [
            query for query in row["queries"]
            if query["returncode"] == 0 and query["payload_parse_error"] is None
        ]
        query_matches = [
            path_matches_fixture(query["payload"], expected_path)
            for query in completed_queries
        ]
        check = {
            "run_id": run_id,
            "condition": row["condition"],
            "size": row["size"],
            "expected_path": expected_path,
            "rg_incumbent_accepted_by_shared_checker": incumbent_match,
            "empty_control_rejected_by_shared_checker": empty_rejected,
            "completed_query_count": len(completed_queries),
            "query_fixture_membership_results": query_matches,
            "query_misses": sum(not matched for matched in query_matches),
        }
        if not incumbent_match or not empty_rejected or not completed_queries or check["query_misses"]:
            raise RuntimeError(f"saved fixture assertion replay failed: {check}")
        checks.append(check)
    result = {
        "harness_version": HARNESS_VERSION,
        "checker": "path_matches_fixture shared by rg incumbent, empty control, and query payloads",
        "saved_run_count": len(checks),
        "successful_completed_query_count": sum(item["completed_query_count"] for item in checks),
        "incumbent_accept_count": sum(item["rg_incumbent_accepted_by_shared_checker"] for item in checks),
        "empty_control_rejection_count": sum(item["empty_control_rejected_by_shared_checker"] for item in checks),
        "query_miss_count": sum(item["query_misses"] for item in checks),
        "checks": checks,
    }
    append_record("fixture_assertion_replay", **result)
    return result


def heap_capture(pid: int, output_directory: Path, stem: str) -> dict:
    result = subprocess.run(
        ["heap", "-s", "--showSizes", str(pid)],
        text=True,
        capture_output=True,
        timeout=120,
        check=False,
    )
    path = output_directory / f"{stem}-heap.txt"
    path.write_text(result.stdout + result.stderr, encoding="utf-8")
    lines = [
        line for line in result.stdout.splitlines()
        if COREML_ENGINE_CLASS_PATTERN.search(line)
    ]
    objects = []
    for line in lines:
        match = re.match(r"\s*(\d+)\s+(\d+)\s+", line)
        if match:
            objects.append({"count": int(match.group(1)), "bytes": int(match.group(2)), "raw": line})
    return {
        "command": ["heap", "-s", "--showSizes", str(pid)],
        "returncode": result.returncode,
        "stdout_path": str(path),
        "matched_coreml_engine_lines": lines,
        "coreml_engine_object_count": sum(entry["count"] for entry in objects),
        "coreml_engine_object_bytes": sum(entry["bytes"] for entry in objects),
    }


def native_stack_capture(pid: int, output_directory: Path, stem: str) -> dict:
    address_results = []
    addresses = []
    for pattern in ("Espresso::espresso_plan", "MLNeuralNetworkV1Engine"):
        address_result = subprocess.run(
            ["heap", f"--addresses={pattern}", str(pid)],
            text=True,
            capture_output=True,
            timeout=120,
            check=False,
        )
        marker = "Active blocks in all zones that match pattern"
        active_section = address_result.stdout.split(marker, 1)[-1] if marker in address_result.stdout else ""
        matched_addresses = re.findall(r"0x[0-9a-fA-F]+", active_section)
        address_results.append(
            {
                "pattern": pattern,
                "command": ["heap", f"--addresses={pattern}", str(pid)],
                "returncode": address_result.returncode,
                "stdout": address_result.stdout,
                "stderr": address_result.stderr,
                "active_addresses": matched_addresses,
            }
        )
        addresses.extend(matched_addresses)
        if addresses:
            break
    addresses = list(dict.fromkeys(addresses))[:4]
    histories = []
    for address in addresses:
        history = subprocess.run(
            ["malloc_history", str(pid), "-fullStacks", address],
            text=True,
            capture_output=True,
            timeout=120,
            check=False,
        )
        histories.append(
            {
                "address": address,
                "returncode": history.returncode,
                "stdout": history.stdout,
                "stderr": history.stderr,
                "contains_coreml_frame": "CoreML" in history.stdout or "Espresso" in history.stdout,
            }
        )
    call_trees = []
    for symbol_pattern in (
        "malloc in Espresso::load_network_layers_post_dispatch",
        "memalign in Espresso::kern_alloc_uninitialized",
        "onnxruntime::CoreMLExecutionProvider",
    ):
        call_tree = subprocess.run(
            ["malloc_history", str(pid), "-callTree", symbol_pattern],
            text=True,
            capture_output=True,
            timeout=120,
            check=False,
        )
        allocation_rows = [
            line for line in call_tree.stdout.split("Call graph:", 1)[-1].splitlines()
            if re.match(r"\s*\d+\s+\([^)]*bytes\)\s+", line)
        ]
        call_trees.append(
            {
                "pattern": symbol_pattern,
                "command": ["malloc_history", str(pid), "-callTree", symbol_pattern],
                "returncode": call_tree.returncode,
                "stdout": call_tree.stdout,
                "stderr": call_tree.stderr,
                "matched_target_symbol": symbol_pattern.split(" in ")[-1] in call_tree.stdout,
                "call_graph_allocation_rows": allocation_rows,
                "has_nonempty_call_tree": bool(allocation_rows),
            }
        )
    path = output_directory / f"{stem}-native-allocation-stacks.json"
    path.write_text(json.dumps(histories, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    call_tree_path = output_directory / f"{stem}-targeted-native-calltrees.json"
    call_tree_path.write_text(json.dumps(call_trees, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return {
        "address_results": address_results,
        "addresses_sampled": addresses,
        "histories": histories,
        "targeted_call_trees": call_trees,
        "history_output_path": str(path),
        "targeted_call_trees_path": str(call_tree_path),
        "actual_native_allocation_stack_observed": any(
            entry["returncode"] == 0 and entry["contains_coreml_frame"] for entry in histories
        ) or any(
            entry["returncode"] == 0
            and entry["pattern"] == "onnxruntime::CoreMLExecutionProvider"
            and entry["has_nonempty_call_tree"]
            and "CoreMLProviderFactory::CreateProvider" in entry["stdout"]
            and "com.apple.CoreML" in entry["stdout"]
            for entry in call_trees
        ),
    }


def run_once(condition: str, size_name: str, repetition: int, fixture: dict, control: bool = False) -> dict:
    executable_root = BASELINE if condition == "baseline" else CORRECTED_PRESERVED
    provider = "coreml" if control else "cpu"
    stack_logging = control
    client = executable_root / "ss"
    daemon_binary = executable_root / "ssd"
    if not client.is_file() or not daemon_binary.is_file():
        raise FileNotFoundError(f"artifact pair is incomplete: {client}, {daemon_binary}")
    corpus_source = Path(fixture["corpus"])
    expected_relative_path = str(fixture["target_path"])
    query_text = str(fixture["query"])
    run_id = f"{condition}-{size_name}-r{repetition}" + ("-coreml-control" if control else "")
    log_directory = OUTPUTS / "logs"
    log_directory.mkdir(parents=True, exist_ok=True)
    daemon_log_path = log_directory / f"{run_id}-ssd.log"
    daemon_console_path = log_directory / f"{run_id}-console.log"
    with tempfile.TemporaryDirectory(prefix="sc-") as temporary:
        root = Path(temporary)
        home = root / "home"
        home.mkdir()
        shared_model_cache = MODEL_CACHE
        shared_model_cache.mkdir(parents=True, exist_ok=True)
        (home / "models").symlink_to(shared_model_cache, target_is_directory=True)
        environment = clean_environment(home, provider, daemon_binary, stack_logging=stack_logging)
        incumbent = fixture_membership_incumbent(corpus_source, expected_relative_path)
        append_record(
            "fixture_incumbent",
            run_id=run_id,
            condition=condition,
            size=size_name,
            fixture_membership=incumbent,
        )
        host_before = host_loadout()
        with daemon_console_path.open("w", encoding="utf-8") as console:
            daemon = subprocess.Popen(
                [str(daemon_binary), "serve"],
                env=environment,
                stdout=console,
                stderr=subprocess.STDOUT,
                text=True,
            )
            sampler = None
            try:
                ready_result = None
                ready_started = time.monotonic()
                while time.monotonic() - ready_started < 60:
                    ready_result = command_result([str(daemon_binary), "status"], environment, timeout=5)
                    if ready_result["returncode"] == 0:
                        break
                    if daemon.poll() is not None:
                        raise RuntimeError(f"daemon exited before readiness: {ready_result}")
                if ready_result is None or ready_result["returncode"] != 0:
                    raise RuntimeError(f"daemon readiness failed: {ready_result}")
                pid_start = ps_snapshot(daemon.pid)
                if pid_start is None:
                    raise RuntimeError("daemon process disappeared before measurement")
                sampler = ProcessSampler(daemon.pid)
                sampler.start()
                start_wall = time.monotonic()
                cpu_start = ps_snapshot(daemon.pid)
                if cpu_start is None:
                    raise RuntimeError("daemon process disappeared before index")
                index_command = command_result([str(client), "index", corpus_source], environment)
                index_stdout = index_command["stdout"]
                index_stats_match = INDEX_PATTERN.search(index_stdout)
                index_stats = None
                if index_stats_match:
                    index_stats = {
                        "files": int(index_stats_match.group(1)),
                        "unchanged": int(index_stats_match.group(2)),
                        "removed": int(index_stats_match.group(3)),
                        "chunks": int(index_stats_match.group(4)),
                        "embedded": int(index_stats_match.group(5)),
                        "skipped": int(index_stats_match.group(6)),
                    }
                index_end = time.monotonic()
                index_bytes = int(fixture["input_bytes"])
                index_duration = index_command["elapsed_seconds_measured"]
                if index_command["returncode"] != 0:
                    raise RuntimeError(f"index command failed: {index_command}")
                if index_stats is None:
                    raise RuntimeError(f"index output has no parseable stats: {index_command}")

                query_observations = []
                total_queries = WARM_QUERY_COUNT + 1
                for query_index in range(total_queries):
                    query_command = command_result(
                        [str(client), "--json", "--path", corpus_source, query_text],
                        environment,
                    )
                    payload = None
                    parse_error = None
                    try:
                        payload = json.loads(query_command["stdout"])
                    except json.JSONDecodeError as error:
                        parse_error = str(error)
                    matched = payload is not None and path_matches_fixture(payload, expected_relative_path)
                    query_observations.append(
                        {
                            "request_index": query_index,
                            "phase": "cold" if query_index == 0 else "warm",
                            **query_command,
                            "payload": payload,
                            "payload_parse_error": parse_error,
                            "fixture_path": expected_relative_path,
                            "fixture_match": matched,
                        }
                    )
                query_end = time.monotonic()
                query_errors = sum(
                    item["returncode"] != 0 or item["payload_parse_error"] is not None
                    for item in query_observations
                )
                completed_queries = [
                    item for item in query_observations
                    if item["returncode"] == 0 and item["payload_parse_error"] is None
                ]
                query_misses = sum(not item["fixture_match"] for item in completed_queries)
                if not completed_queries or query_misses:
                    raise RuntimeError(
                        f"completed_queries={len(completed_queries)} query_errors={query_errors} "
                        f"fixture_misses={query_misses}; "
                        f"observations={query_observations}"
                    )

                log_path = home / "logs" / "ssd.log"
                if log_path.is_file():
                    shutil.copyfile(log_path, daemon_log_path)
                    daemon_log = log_path.read_text(encoding="utf-8", errors="replace")
                else:
                    daemon_log = ""
                daemon_errors = [
                    line for line in daemon_log.splitlines()
                    if re.search(r"\b(error|fatal)\b", line, re.IGNORECASE)
                ]
                heap = None
                native_stack = None
                if size_name == "tiny" and (repetition == 1 or control):
                    heap = heap_capture(daemon.pid, log_directory, run_id)
                    if control:
                        native_stack = native_stack_capture(daemon.pid, log_directory, run_id)
                        append_record(
                            "native_control_capture",
                            run_id=run_id,
                            provider=provider,
                            fixture_path=expected_relative_path,
                            completed_fixture_query_count=sum(
                                item["returncode"] == 0 and item["fixture_match"]
                                for item in query_observations
                            ),
                            query_error_count=query_errors,
                            heap_capture=heap,
                            native_stack_capture=native_stack,
                        )
                        if not native_stack["actual_native_allocation_stack_observed"]:
                            raise RuntimeError(f"explicit CoreML control has no CoreML native allocation stack: {native_stack}")
                    elif condition == "baseline" and heap["coreml_engine_object_count"] <= 0:
                        raise RuntimeError(f"baseline CPU heap has no CoreML engine objects: {heap}")
                    elif condition == "corrected" and heap["coreml_engine_object_count"] != 0:
                        raise RuntimeError(f"corrected CPU heap has CoreML engine objects: {heap}")
                cpu_end = ps_snapshot(daemon.pid)
                if cpu_end is None:
                    raise RuntimeError("daemon exited before final process measurement")
                elapsed_measurement = time.monotonic() - start_wall
                sample_values = sampler.stop()
                sampler = None
                rss_values = [item["rss_kb"] for item in sample_values]
                cpu_seconds_delta = cpu_seconds(cpu_end["cpu_time_raw"]) - cpu_seconds(cpu_start["cpu_time_raw"])
                input_fixture_hash = fixture["sha256"] if size_name == "tiny" else fixture["tree_sha256"]
                row = {
                    "run_id": run_id,
                    "harness_version": HARNESS_VERSION,
                    "condition": condition,
                    "provider": provider,
                    "control": control,
                    "size": size_name,
                    "repetition": repetition,
                    "status": "captured",
                    "artifact_identity": {
                        "ss_path": str(client),
                        "ss_sha256": sha256_file(client),
                        "ssd_path": str(daemon_binary),
                        "ssd_sha256": sha256_file(daemon_binary),
                        "pid": daemon.pid,
                        "pid_command": pid_start["command"],
                    },
                    "environment": {
                        key: value for key, value in environment.items()
                        if key.startswith("SS_") or key == "MallocStackLogging"
                    },
                    "input_fixture": {
                        **fixture,
                        "corpus": str(corpus_source),
                        "sha256_or_tree_sha256": input_fixture_hash,
                        "incumbent": incumbent,
                    },
                    "index": {
                        "command": index_command["command"],
                        "returncode": index_command["returncode"],
                        "stdout": index_stdout,
                        "stderr": index_command["stderr"],
                        "stats": index_stats,
                        "elapsed_seconds_measured": index_duration,
                        "elapsed_wall_seconds_measured": index_end - start_wall,
                        "input_bytes": index_bytes,
                        "throughput_bytes_per_second_derived": (
                            index_bytes / index_duration if index_duration > 0 else None
                        ),
                    },
                    "queries": query_observations,
                    "latency": {
                        "cold_single_request_seconds": query_observations[0]["elapsed_seconds_measured"],
                        "warm_single_request_seconds": [
                            item["elapsed_seconds_measured"] for item in query_observations[1:]
                        ],
                        "all_request_percentiles": latency_summary(
                            [item["elapsed_seconds_measured"] for item in query_observations]
                        ),
                    },
                    "process": {
                        "rss_peak_kb_sampled_during_work": max(rss_values) if rss_values else None,
                        "rss_steady_last_five_samples_kb": (
                            statistics.mean(rss_values[-5:]) if rss_values else None
                        ),
                        "rss_samples_kb": rss_values,
                        "ps_cpu_time_before_work": cpu_start["cpu_time_raw"],
                        "ps_cpu_time_after_work": cpu_end["cpu_time_raw"],
                        "ps_cumulative_cpu_delta_seconds_measured": cpu_seconds_delta,
                        "measurement_wall_seconds_measured": elapsed_measurement,
                        "elapsed_cpu_percent_derived": (
                            100 * cpu_seconds_delta / elapsed_measurement
                            if elapsed_measurement > 0 else None
                        ),
                        "ps_cpu_percent_final": cpu_end["cpu_percent_ps"],
                        "sampled_ps_rows": sample_values,
                        "heap_capture": heap,
                        "native_stack_capture": native_stack,
                    },
                    "errors": {
                        "index_command_count": 1,
                        "index_command_errors": int(index_command["returncode"] != 0),
                        "query_command_count": len(query_observations),
                        "query_command_errors": query_errors,
                        "query_fixture_membership_misses": query_misses,
                        "daemon_error_or_fatal_log_lines": daemon_errors,
                        "daemon_error_or_fatal_log_line_count": len(daemon_errors),
                    },
                    "host_loadout_before": host_before,
                    "host_loadout_after": host_loadout(),
                    "process_work_elapsed_seconds_measured": query_end - start_wall,
                }
                append_record("run", **row)
                return row
            except Exception as error:
                if sampler is not None:
                    sample_values = sampler.stop()
                else:
                    sample_values = []
                if daemon.poll() is None:
                    current = ps_snapshot(daemon.pid)
                else:
                    current = None
                append_record(
                    "run_failure",
                    run_id=run_id,
                    condition=condition,
                    provider=provider,
                    size=size_name,
                    repetition=repetition,
                    error_type=type(error).__name__,
                    error=str(error),
                    daemon_pid=daemon.pid,
                    daemon_ps=current,
                    sampled_ps_rows=sample_values,
                    host_loadout=host_loadout(),
                )
                raise
            finally:
                if sampler is not None:
                    sampler.stop()
                if daemon.poll() is None:
                    command_result([str(daemon_binary), "stop"], environment, timeout=10)
                    try:
                        daemon.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        daemon.terminate()
                        daemon.wait(timeout=10)
                if (home / "logs" / "ssd.log").is_file() and not daemon_log_path.exists():
                    shutil.copyfile(home / "logs" / "ssd.log", daemon_log_path)


def read_completed_runs() -> list[dict]:
    if not JOURNAL.exists():
        return []
    by_run_id: dict[str, dict] = {}
    for line in JOURNAL.read_text(encoding="utf-8").splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            item.get("event") == "run"
            and item.get("status") == "captured"
            and item.get("harness_version") == HARNESS_VERSION
        ):
            run_id = item["run_id"]
            current = by_run_id.get(run_id)
            if current is None or (
                current.get("derived_from_record_id") is not None
                and item.get("derived_from_record_id") is None
            ):
                by_run_id[run_id] = item
    return list(by_run_id.values())


def read_journal_records() -> list[dict]:
    if not JOURNAL.exists():
        return []
    records = []
    for line in JOURNAL.read_text(encoding="utf-8").splitlines():
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return records


def derive_version_two_rows_from_raw_artifacts() -> int:
    records = read_journal_records()
    derived_sources = {
        item.get("derived_from_record_id")
        for item in records
        if item.get("event") == "run" and item.get("harness_version") == HARNESS_VERSION
    }
    derived_count = 0
    for source in records:
        if source.get("event") != "run" or source.get("status") != "captured":
            continue
        if source.get("harness_version") is not None or source.get("record_id") in derived_sources:
            continue
        row = copy.deepcopy(source)
        source_id = row.pop("record_id")
        row.pop("timestamp_utc", None)
        row.pop("event", None)
        row["harness_version"] = HARNESS_VERSION
        row["derived_from_record_id"] = source_id
        row["derivation_note"] = (
            "process and query observations copied unchanged from the source row; "
            "CoreML engine-object count reparsed from its preserved raw heap output"
        )
        heap = row.get("process", {}).get("heap_capture")
        if heap is not None:
            heap.pop("coreml_engine_allocation_count", None)
            heap.pop("coreml_engine_allocation_bytes", None)
            raw_path = Path(heap["stdout_path"])
            if not raw_path.is_file():
                raise FileNotFoundError(f"raw heap evidence is missing: {raw_path}")
            raw_heap = raw_path.read_text(encoding="utf-8", errors="replace")
            exact_lines = [
                line for line in raw_heap.splitlines()
                if COREML_ENGINE_CLASS_PATTERN.search(line)
            ]
            objects = []
            for line in exact_lines:
                match = re.match(r"\s*(\d+)\s+(\d+)\s+", line)
                if match:
                    objects.append(
                        {"count": int(match.group(1)), "bytes": int(match.group(2)), "raw": line}
                    )
            heap["matched_coreml_engine_lines"] = exact_lines
            heap["coreml_engine_object_count"] = sum(entry["count"] for entry in objects)
            heap["coreml_engine_object_bytes"] = sum(entry["bytes"] for entry in objects)
            heap["metric_source"] = str(raw_path)
            heap["metric_source_rows"] = exact_lines
            if row["condition"] == "baseline" and heap["coreml_engine_object_count"] <= 0:
                raise RuntimeError(f"saved baseline heap has no exact CoreML engine class row: {raw_path}")
        append_record("run", **row)
        derived_count += 1
    return derived_count


def emit_summary() -> None:
    records = read_completed_runs()
    groups: dict[tuple[str, str, bool], list[dict]] = {}
    for row in records:
        groups.setdefault((row["condition"], row["size"], bool(row["control"])), []).append(row)
    summaries = []
    for (condition, size_name, control), rows in sorted(groups.items()):
        rows.sort(key=lambda item: item["repetition"])
        throughputs = [row["index"]["throughput_bytes_per_second_derived"] for row in rows]
        cpu_values = [row["process"]["ps_cumulative_cpu_delta_seconds_measured"] for row in rows]
        rss_values = [row["process"]["rss_peak_kb_sampled_during_work"] for row in rows]
        summaries.append(
            {
                "condition": condition,
                "size": size_name,
                "control": control,
                "completed_repetitions": len(rows),
                "run_ids": [row["run_id"] for row in rows],
                "throughput_bytes_per_second_derived": {
                    "values": throughputs,
                    "coefficient_of_variation": coefficient_of_variation(throughputs),
                },
                "cpu_delta_seconds_measured": {
                    "values": cpu_values,
                    "coefficient_of_variation": coefficient_of_variation(cpu_values),
                },
                "rss_peak_kb_sampled_during_work": {
                    "values": rss_values,
                    "coefficient_of_variation": coefficient_of_variation(rss_values),
                },
                "latency_seconds": {
                    "cold_single_request": {
                        "values": [row["latency"]["cold_single_request_seconds"] for row in rows],
                        "coefficient_of_variation": coefficient_of_variation(
                            [row["latency"]["cold_single_request_seconds"] for row in rows]
                        ),
                    },
                    "warm_mean_per_run": {
                        "values": [
                            statistics.mean(row["latency"]["warm_single_request_seconds"])
                            for row in rows
                        ],
                        "coefficient_of_variation": coefficient_of_variation(
                            [
                                statistics.mean(row["latency"]["warm_single_request_seconds"])
                                for row in rows
                            ]
                        ),
                    },
                    "all_request_p50": {
                        "values": [row["latency"]["all_request_percentiles"]["p50_seconds"] for row in rows],
                        "coefficient_of_variation": coefficient_of_variation(
                            [row["latency"]["all_request_percentiles"]["p50_seconds"] for row in rows]
                        ),
                    },
                    "all_request_p90": {
                        "values": [row["latency"]["all_request_percentiles"]["p90_seconds"] for row in rows],
                        "coefficient_of_variation": coefficient_of_variation(
                            [row["latency"]["all_request_percentiles"]["p90_seconds"] for row in rows]
                        ),
                    },
                    "all_request_p99": {
                        "values": [row["latency"]["all_request_percentiles"]["p99_seconds"] for row in rows],
                        "coefficient_of_variation": coefficient_of_variation(
                            [row["latency"]["all_request_percentiles"]["p99_seconds"] for row in rows]
                        ),
                    },
                    "all_request_p99_9": {
                        "values": [row["latency"]["all_request_percentiles"]["p99_9_seconds"] for row in rows],
                        "coefficient_of_variation": coefficient_of_variation(
                            [row["latency"]["all_request_percentiles"]["p99_9_seconds"] for row in rows]
                        ),
                    },
                },
            }
        )
    append_record(
        "summary",
        summaries=summaries,
        interpretation="descriptive distributions only; per-run payloads and process traces remain the evidence",
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--condition", choices=("baseline", "corrected", "all"), default="all")
    parser.add_argument("--resume", action="store_true", default=True)
    parser.add_argument("--no-resume", dest="resume", action="store_false")
    arguments = parser.parse_args()
    fixtures = prepare_fixtures()
    append_record(
        "harness_start",
        harness_version=HARNESS_VERSION,
        condition_selection=arguments.condition,
        tiny_fixture=fixtures["tiny"],
        owner_messages_fixture={key: value for key, value in fixtures["owner_messages"].items() if key != "query"},
        tiny_fixture_claim="literal copied from idle-model-release/probe.py",
        tiny_fixture_bytes=fixtures["tiny"]["bytes"],
        sampling_interval_seconds=SAMPLE_INTERVAL_SECONDS,
        warm_query_count=WARM_QUERY_COUNT,
        host_loadout=host_loadout(),
    )
    condition_names = ["baseline", "corrected"] if arguments.condition == "all" else [arguments.condition]
    migrated_count = derive_version_two_rows_from_raw_artifacts()
    if migrated_count:
        append_record(
            "artifact_derivation",
            harness_version=HARNESS_VERSION,
            derived_run_rows=migrated_count,
            method="reparse preserved raw heap artifacts and copy unchanged process/query payload rows",
        )
    completed = {
        (row["condition"], row["size"], row["repetition"], bool(row["control"]))
        for row in read_completed_runs()
    }
    replay_saved_fixture_assertions()
    sizes = {
        "tiny": {
            "corpus": str(FIXTURES / "tiny"),
            "target_path": fixtures["tiny"]["relative_path"],
            "query": fixtures["tiny"]["query"],
            "input_bytes": fixtures["tiny"]["bytes"],
            "sha256": fixtures["tiny"]["sha256"],
        },
        "owner_messages": {
            "corpus": fixtures["owner_messages"]["root"],
            "target_path": fixtures["owner_messages"]["target_path"],
            "query": fixtures["owner_messages"]["query"],
            "input_bytes": fixtures["owner_messages"]["total_bytes"],
            "tree_sha256": fixtures["owner_messages"]["tree_sha256"],
            "file_count": fixtures["owner_messages"]["file_count"],
        },
    }
    for condition in condition_names:
        for size_name, fixture in sizes.items():
            for repetition in range(1, 4):
                key = (condition, size_name, repetition, False)
                if arguments.resume and key in completed:
                    continue
                run_once(condition, size_name, repetition, fixture)
    if arguments.condition in ("all", "corrected"):
        control_key = ("corrected", "tiny", 1, True)
        if not arguments.resume or control_key not in completed:
            run_once("corrected", "tiny", 1, sizes["tiny"], control=True)
    emit_summary()
    print(f"compare output appended to {JOURNAL}")


if __name__ == "__main__":
    main()
