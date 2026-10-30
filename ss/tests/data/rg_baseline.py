"""rg versus ss on the frozen corpus; definitions are in ai_docs/design.md ("rg baseline versus ss").

  rg_baseline.py metrics <chunks.tsv> <cpu256.1.run.json>      quality tables, wins and losses
  rg_baseline.py rgtime  <tree> [repeats]                      rg wall time per query (median) over a tree
  rg_baseline.py count   <tree>                                files and MB rg would search
  rg_baseline.py clihits <hits.json>                           ss CLI top-50 hybrid hits: chunk and exact file-level metrics
  rg_baseline.py sstime  <tree> <home> <ss_bin_dir> <index_timeout_s> [hits_out.json]
                                                               cold query, index, warm query latency through ssd

<chunks.tsv> is the stdout of `embed_eval chunks` (id<TAB>tokens<TAB>first line).
"""

import json
import os
import re
import statistics
import subprocess
import sys
import time
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "corpus")
QUERIES = os.path.join(HERE, "queries.tsv")
INF = 10**9
STOPWORDS = frozenset(
    "a an the of to in on for and or is are was be been it its that this these those with so only by as at from "
    "when how what which who does do did into not no if then than up per each all any can will would should "
    "has have had i you we they them their our your".split()
)


def load_queries():
    rows = []
    for line in open(QUERIES, encoding="utf-8").read().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        cells = line.split("\t")
        assert len(cells) == 4, line
        rows.append({"text": cells[0], "gold": f"{cells[1]}:{cells[2]}", "kind": cells[3]})
    return rows


def load_chunks(path):
    chunks = defaultdict(list)
    for line in open(path, encoding="utf-8").read().splitlines():
        ident = line.split("\t")[0]
        file, span = ident.rsplit(":", 1)
        start, end = map(int, span.split("-"))
        chunks[file].append((start, end, ident))
    return chunks


def words_of(token):
    spaced = re.sub(r"([a-z0-9])([A-Z])", r"\1 \2", token)
    return [part.lower() for part in re.split(r"[_\s]+", spaced) if part]


def is_identifier(token):
    return "_" in token or re.search(r"[a-z0-9][A-Z]", token) is not None


def terms_of(query):
    """returns (terms, words): terms are scored units; words feed the R2 pair forms."""
    terms, words = [], []
    for token in re.findall(r"[A-Za-z0-9_]+", query):
        parts = [w for w in words_of(token) if w not in STOPWORDS]
        words.extend(parts)
        if is_identifier(token) and token.lower() not in STOPWORDS:
            terms.append(token.lower())
        terms.extend(parts)
    return list(dict.fromkeys(terms)), words


def forms_of(word_seq):
    if len(word_seq) == 1:
        return {word_seq[0]}
    low = [w.lower() for w in word_seq]
    return {
        "_".join(low),
        low[0] + "".join(w.capitalize() for w in low[1:]),
        "".join(w.capitalize() for w in low),
        "".join(low),
    }


def variants_r1(terms, words):
    return {term: {term} for term in terms}, {}


def variants_r2(terms, words):
    variants = {term: {term} for term in terms}
    pairs = defaultdict(set)
    for left, right in zip(words, words[1:]):
        for form in forms_of([left, right]):
            pairs[form.lower()].add(left)
            pairs[form.lower()].add(right)
    return variants, pairs


def rg_command(patterns):
    command = ["rg", "-i", "-n", "--json"]
    for pattern in patterns:
        command += ["-e", pattern]
    return command + ["."]


def run_rg(patterns, tree=CORPUS):
    result = subprocess.run(rg_command(patterns), cwd=tree, capture_output=True, text=True, check=False)
    assert result.returncode in (0, 1), result.stderr
    lines = []
    for raw in result.stdout.splitlines():
        event = json.loads(raw)
        if event["type"] != "match":
            continue
        data = event["data"]
        path = data["path"]["text"].removeprefix("./")
        lines.append((path, data["line_number"], data["lines"]["text"].lower(), len(data["submatches"])))
    return lines


def score(query_terms, mode):
    """returns (chunk_scores, file_scores, unmapped) as {key: (distinct, total)}."""
    terms, words = query_terms
    variants, pairs = (variants_r1 if mode.startswith("r1") else variants_r2)(terms, words)
    patterns = sorted({v for vs in variants.values() for v in vs} | set(pairs))
    if mode.endswith("w"):
        patterns = [rf"\b{re.escape(p)}\b" for p in patterns]
    return patterns, variants, pairs


def rank_for(query, mode, chunks):
    terms, words = terms_of(query)
    if not terms:
        return {}, {}, 0
    patterns, variants, pairs = score((terms, words), mode)
    boundary = mode.endswith("w")
    chunk_hits = defaultdict(lambda: [set(), 0])
    file_hits = defaultdict(lambda: [set(), 0])
    unmapped = 0
    for path, number, text, count in run_rg(patterns):
        found = (lambda v: re.search(rf"\b{re.escape(v)}\b", text) is not None) if boundary else (lambda v: v in text)
        matched = {term for term, vs in variants.items() if any(found(v) for v in vs)}
        for form, owners in pairs.items():
            if found(form):
                matched |= owners
        if not matched:
            continue
        file_hits[path][0] |= matched
        file_hits[path][1] += count
        owner = next((ident for start, end, ident in chunks.get(path, []) if start <= number <= end), None)
        if owner is None:
            unmapped += 1
            continue
        chunk_hits[owner][0] |= matched
        chunk_hits[owner][1] += count
    return chunk_hits, file_hits, unmapped


def ordered(hits, key_order):
    return sorted(hits, key=lambda key: (-len(hits[key][0]), -hits[key][1], key_order(key)))


def chunk_order(key):
    file, span = key.rsplit(":", 1)
    return (file, int(span.split("-")[0]))


def rank_in(sequence, target):
    return sequence.index(target) + 1 if target in sequence else INF


def metrics(ranks):
    total = len(ranks)
    share = lambda k: sum(1 for r in ranks if r <= k) / total if total else float("nan")
    mrr = sum(1 / r for r in ranks if r < INF) / total if total else float("nan")
    return share(1), share(3), share(5), mrr


def fmt(ranks):
    h1, h3, h5, mrr = metrics(ranks)
    return f"{h1:.3f} | {h3:.3f} | {h5:.3f} | {mrr:.3f}"


def file_ranks_from_chunks(top, gold_file):
    """ss file rank from a chunk list of limited depth: returns (rank, exact) with rank INF when absent."""
    files = list(dict.fromkeys(ident.rsplit(":", 1)[0] for ident in top))
    if gold_file in files:
        return files.index(gold_file) + 1, True
    return INF, False


def cmd_metrics(chunks_path, run_path):
    queries = load_queries()
    chunks = load_chunks(chunks_path)
    run = json.load(open(run_path))
    assert len(queries) == 44, f"N={len(queries)}"
    assert len(run["queries"]) == 44
    known = {ident for items in chunks.values() for _, _, ident in items}
    assert all(q["gold"] in known for q in queries), "gold chunk missing"
    rg_files = subprocess.run(["rg", "--files"], cwd=CORPUS, capture_output=True, text=True, check=True).stdout.split()
    assert sorted(f.removeprefix("./") for f in rg_files) == sorted(chunks), "rg file set differs from chunk files"
    rows = []
    unmapped_total = {"r1": 0, "r2": 0, "r1w": 0, "r2w": 0}
    for query, ss in zip(queries, run["queries"]):
        assert query["text"] == ss["text"] and query["gold"] == ss["gold"]
        gold_file = query["gold"].rsplit(":", 1)[0]
        row = {"q": query, "ss_hybrid": ss["hybrid_rank"], "ss_vector": ss["vector_rank"], "ss_fts": ss["fts_rank"]}
        for mode in ("r1", "r2", "r1w", "r2w"):
            chunk_hits, file_hits, unmapped = rank_for(query["text"], mode, chunks)
            unmapped_total[mode] += unmapped
            row[f"{mode}_chunk"] = rank_in(ordered(chunk_hits, chunk_order), query["gold"]) if chunk_hits else INF
            row[f"{mode}_file"] = rank_in(ordered(file_hits, lambda key: key), gold_file) if file_hits else INF
            row[f"{mode}_nchunks"] = len(chunk_hits)
        for name in ("hybrid", "vector"):
            low, high = {}, {}
            rank, exact = file_ranks_from_chunks(ss[f"{name}_top5"], gold_file)
            row[f"ss_{name}_file"] = (rank, exact, len(dict.fromkeys(i.rsplit(":", 1)[0] for i in ss[f"{name}_top5"])))
        rows.append(row)
    for mode, count in unmapped_total.items():
        print(f"{mode}: match lines outside every chunk (dropped from chunk level, kept in file level): {count}")
    splits = [("all", lambda r: True), ("ident", lambda r: r["q"]["kind"] == "ident"), ("desc", lambda r: r["q"]["kind"] == "desc")]
    print("\nCHUNK LEVEL: scope(n) | system | hit@1 | hit@3 | hit@5 | MRR (rank>50 for ss counts 0; absent for rg counts 0)")
    for scope, pick in splits:
        sel = [r for r in rows if pick(r)]
        assert (scope != "all" or len(sel) == 44) and sel
        for label, key in (("rg R1", "r1_chunk"), ("rg R2", "r2_chunk"), ("rg R1w", "r1w_chunk"), ("rg R2w", "r2w_chunk"), ("ss hybrid", "ss_hybrid"), ("ss vector", "ss_vector"), ("ss fts", "ss_fts")):
            ranks = [r[key] if r[key] <= 50 or key.startswith("r") else INF for r in sel]
            print(f"{scope}(n={len(sel)}) | {label} | {fmt(ranks)}")
    print("\nFILE LEVEL: gold chunk's file in top k files")
    print("scope(n) | system | hit@1 | hit@3 | hit@5 | MRR")
    for scope, pick in splits:
        sel = [r for r in rows if pick(r)]
        for label, key in (("rg R1", "r1_file"), ("rg R2", "r2_file"), ("rg R1w", "r1w_file"), ("rg R2w", "r2w_file")):
            print(f"{scope}(n={len(sel)}) | {label} | {fmt([r[key] for r in sel])}")
        for name in ("hybrid", "vector"):
            key = f"ss_{name}_file"
            lower = [r[key][0] if r[key][1] else INF for r in sel]
            undetermined = sum(1 for r in sel if not r[key][1] and r[key][2] < 5)
            upper = [r[key][0] if r[key][1] else (INF if r[key][2] >= 5 else 5) for r in sel]
            print(f"{scope}(n={len(sel)}) | ss {name} lower bound (from top-5 chunks) | {fmt(lower)}")
            print(f"{scope}(n={len(sel)}) | ss {name} upper bound ({undetermined} queries undetermined, unknown file rank set to 5) | {fmt(upper)}")
    print("\nrg R1/R2 finds gold chunk in top 3, ss hybrid does not (rg R1 rank, rg R2 rank, ss hybrid, ss vector, ss fts)")
    for r in rows:
        if min(r["r1_chunk"], r["r2_chunk"]) <= 3 and r["ss_hybrid"] > 3:
            print(f"  {show(r)}")
    print("\nss hybrid finds gold chunk in top 3, neither rg R1 nor R2 does")
    for r in rows:
        if r["ss_hybrid"] <= 3 and min(r["r1_chunk"], r["r2_chunk"]) > 3:
            print(f"  {show(r)}")
    print("\nper-query ranks: R1c R2c R1wc R2wc | ss hybrid vector fts | kind | gold | query   (inf = not ranked; ss >50 shown 51)")
    for r in rows:
        print(f"  {show(r)}  files(R1/R2/R1w/R2w):{r['r1_file']}/{r['r2_file']}/{r['r1w_file']}/{r['r2w_file']}")


def show(r):
    inf = lambda v: "inf" if v >= INF else str(v)
    return (f"R1c={inf(r['r1_chunk'])} R2c={inf(r['r2_chunk'])} R1wc={inf(r['r1w_chunk'])} R2wc={inf(r['r2w_chunk'])} | ss h={r['ss_hybrid']} v={r['ss_vector']} f={r['ss_fts']} | "
            f"{r['q']['kind']} | {r['q']['gold']} | {r['q']['text']}")


def uptime():
    return subprocess.run(["uptime"], capture_output=True, text=True, check=True).stdout.strip()


def cmd_count(tree):
    files = subprocess.run(["rg", "--files"], cwd=tree, capture_output=True, text=True, check=False).stdout.split("\n")
    files = [f for f in files if f]
    size = sum(os.path.getsize(os.path.join(tree, f)) for f in files)
    print(f"{tree}: files={len(files)} MB={size / 1e6:.2f}")


def cmd_rgtime(tree, repeats):
    queries = load_queries()
    print(f"tree={tree} repeats={repeats}")
    print(f"uptime_before: {uptime()}")
    medians = {"r1": [], "r2": [], "r1w": [], "r2w": []}
    for query in queries:
        terms, words = terms_of(query["text"])
        for mode in ("r1", "r2", "r1w", "r2w"):
            patterns, _, _ = score((terms, words), mode)
            samples = []
            for _ in range(repeats):
                started = time.perf_counter()
                subprocess.run(rg_command(patterns), cwd=tree, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
                samples.append((time.perf_counter() - started) * 1000)
            medians[mode].append(statistics.median(samples))
    for mode, values in medians.items():
        values.sort()
        print(f"{mode}: n={len(values)} per-query median ms: min={values[0]:.1f} p50={statistics.median(values):.1f} "
              f"p95={values[int(len(values) * 0.95)]:.1f} max={values[-1]:.1f}")
    print(f"uptime_after: {uptime()}")


def ss_env(home, bin_dir):
    env = dict(os.environ)
    env.update({"SS_HOME": home, "SS_DAEMON_BIN": os.path.join(bin_dir, "ssd"), "SS_IDLE_SECS": "900", "RUST_LOG": "warn"})
    return env


def timed(command, env, cwd, timeout=None):
    started = time.perf_counter()
    try:
        done = subprocess.run(command, env=env, cwd=cwd, capture_output=True, text=True, timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        return (time.perf_counter() - started) * 1000, None, ""
    return (time.perf_counter() - started) * 1000, done.returncode, done.stdout + done.stderr


def percentile(values, share):
    ordered_values = sorted(values)
    return ordered_values[min(len(ordered_values) - 1, int(len(ordered_values) * share))]


def cmd_sstime(tree, home, bin_dir, index_timeout, hits_out):
    queries = load_queries()
    env = ss_env(home, bin_dir)
    ss = os.path.join(bin_dir, "ss")
    print(f"tree={tree} home={home}")
    print(f"uptime_before_cold: {uptime()}")
    first = queries[0]["text"].split()
    ms, code, out = timed([ss, "--path", tree, "-n", "3", *first], env, tree, 300)
    print(f"cold_query_no_daemon_no_index: ms={ms:.0f} exit={code} output_bytes={len(out)} head={out[:200]!r}")
    print(f"uptime_before_index: {uptime()}")
    ms, code, out = timed([ss, "index", tree], env, tree, index_timeout)
    print(f"index_real_model: ms={ms:.0f} s={ms / 1000:.1f} exit={code} timed_out={code is None} out={out.strip()[:300]!r}")
    if code is None:
        return
    timed([os.path.join(bin_dir, "ssd"), "stop"], env, tree, 30)
    print(f"uptime_before_cold_daemon: {uptime()}")
    ms, code, out = timed([ss, "--path", tree, "-n", "10", *first], env, tree, 300)
    print(f"cold_daemon_first_query_indexed: ms={ms:.0f} exit={code}")
    print(f"uptime_before_warm: {uptime()}")
    timed([ss, "--path", tree, "-n", "10", "warmup"], env, tree, 120)
    samples, hits = [], {}
    for round_number in range(3):
        for query in queries:
            ms, code, out = timed([ss, "--path", tree, "-n", "10", *query["text"].split()], env, tree, 120)
            assert code == 0, (query, out)
            samples.append(ms)
    print(f"warm_query_wall_ms (ss client, -n 10, includes exec): n={len(samples)} min={min(samples):.0f} "
          f"p50={statistics.median(samples):.0f} p95={percentile(samples, 0.95):.0f} max={max(samples):.0f}")
    print(f"uptime_after_warm: {uptime()}")
    if hits_out:
        for query in queries:
            ms, code, out = timed([ss, "--path", tree, "-n", "50", "--json", *query["text"].split()], env, tree, 120)
            hits[query["text"]] = [json.loads(line) for line in out.splitlines() if line.startswith("{")]
        json.dump(hits, open(hits_out, "w"))
    timed([os.path.join(bin_dir, "ssd"), "stop"], env, tree, 30)


def cmd_clihits(hits_path):
    queries = load_queries()
    hits = json.load(open(hits_path))
    assert len(queries) == 44 and len(hits) == 44
    chunk_ranks, file_ranks = [], []
    for query in queries:
        rows = hits[query["text"]]
        ids = [f"{os.path.relpath(r['path'], CORPUS)}:{r['start_line']}-{r['end_line']}" for r in rows]
        chunk_ranks.append(rank_in(ids, query["gold"]))
        file_ranks.append(file_ranks_from_chunks(ids, query["gold"].rsplit(":", 1)[0])[0])
    for label, ranks in (("chunk", chunk_ranks), ("file", file_ranks)):
        print(f"ss CLI hybrid {label} (n={len(ranks)}) | {fmt(ranks)}")
        for kind in ("ident", "desc"):
            sel = [r for r, q in zip(ranks, queries) if q["kind"] == kind]
            print(f"  {kind}(n={len(sel)}) | {fmt(sel)}")
    print("chunk ranks:", chunk_ranks)


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "metrics":
        cmd_metrics(sys.argv[2], sys.argv[3])
    elif mode == "rgtime":
        cmd_rgtime(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 20)
    elif mode == "clihits":
        cmd_clihits(sys.argv[2])
    elif mode == "sstime":
        cmd_sstime(sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]), sys.argv[6] if len(sys.argv) > 6 else "")
    elif mode == "count":
        cmd_count(sys.argv[2])
    else:
        sys.exit(__doc__)


main()
