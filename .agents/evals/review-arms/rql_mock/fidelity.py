"""Host-only lean-pilot sampling, adjudication and gate arithmetic."""
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from fractions import Fraction
import hashlib
import json
from pathlib import Path
import re
from random import Random

from census_label import CENSUS
from .engine import REPAIR_STALLED, parse
from .engine.grammar import MENU
from .engine.validation import canonical


ROOT = Path("/mnt/optane/review-arms-pilot")
RANDOM_N = 10
DOCUMENTED_N = 20
STABILITY_N = 20
BOOTSTRAPS = 10_000
DOCUMENTED = "documented"

# Scope rewrites for covered ledger entries, with narrower selectors where the
# frozen worked case specifies them. Keys stay on the scoring host.
SEEDS = {
    ("contract-cases", "archive-resume-hint-without-prompt"): (1, "error", "function", "archived_session"),
    ("value-transfers", "mcp-default-advisory-reported-clean"): (1, "upstream argument", "call", "structured_only"),
    ("value-transfers", "pinned-worker-disappears"): (4, "downstream return", "function", "worker_binary_for_arch"),
    ("value-transfers", "export-relative-read-root"): (0, "upstream return", "function", "worker_launch_config"),
    ("value-transfers", "catalog-cache-key-collision"): (2, "upstream argument", "function", "discover_blocking"),
    ("guarded-paths", "metadata-checkpoint-overwrites-checkout"): (0, "outgoing call", "function", "restore_git_snapshot_for_session"),
    ("guarded-paths", "shortcut-escape-opens-unvisited-manager"): (1, "outgoing write", "callable", "handle_workspace_manager_event"),
    ("guarded-paths", "web-feed-self-deadlock"): (1, "incoming call", "callable", "active_lifecycles"),
}
LABEL_KINDS = {"contract-cases": "contract", "value-transfers": "value_flow",
               "guarded-paths": "guarded_path"}
LEDGER_HEADS = dict(zip(("Contract", "Value flow", "Guarded path"), LABEL_KINDS))
DEFAULT_SELECTORS = {"contract-cases": "all", "value-transfers": "upstream return",
                     "guarded-paths": "outgoing all"}
DOCUMENTED_FORMS = (
    "(procedure-of {seed})", "(callers {decl})", "(callees {decl})",
    "(references-of {decl})", "(uses {decl})", "(used-by {decl})",
    "(call-sites-from {decl})", "(call-sites-to {decl})", "(occurrences-of {decl})",
    "(callable-signature {decl})", "(file-of {seed})", "(bindings-in {seed})",
    "(occurrences-in {seed})", "(enclosing-decl {seed})",
)


@dataclass(frozen=True, order=True)
class Query:
    step: str
    stratum: str
    repo: str
    commit: str
    text: str
    census: tuple = ()

    @property
    def key(self):
        return sha(canonical([self.step, self.repo, self.commit, self.text]).encode())


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def frozen(path, expected):
    raw = path.read_bytes()
    if sha(raw) != expected:
        raise ValueError(f"{path} differs from the frozen manifest")
    return raw.decode("utf-8")


def jsonl(text):
    return [json.loads(line) for line in text.splitlines() if line.strip()]


def load_inputs(path):
    raw = path.read_bytes()
    manifest = json.loads(raw)
    inputs = manifest["inputs"]
    if manifest["version"] != 1:
        raise ValueError("unsupported pilot manifest version")
    served = manifest.get("served_steps", list(MENU))
    if (not isinstance(served, list) or any(not isinstance(step, str) or step not in MENU for step in served)
            or len(set(served)) != len(served)):
        raise ValueError("invalid manifest served_steps")
    paths = manifest["input_paths"]
    documents = {key: frozen(path.parent / paths[key], inputs[key + "_sha256"])
                 for key in ("catalogue", "steps", "cases")}
    selected = jsonl(frozen(path.parent / paths["labels_snapshot"], inputs["labels_snapshot_sha256"]))
    labels = {(row["repo"], row["id"]): row for row in selected}
    eligible = jsonl(frozen(path.parent / paths["eligible_labels"], inputs["eligible_labels_sha256"]))
    eligible_keys = {(row["repo"], row["id"]): row for row in eligible}
    if (len(eligible_keys) != len(eligible) or len(labels) != len(selected)
            or set(labels) != {(d["repo"], d["id"]) for d in manifest["diffs"]}):
        raise ValueError("frozen labels differ from the pilot cohort")
    for diff in manifest["diffs"]:
        key = diff["repo"], diff["id"]
        row = eligible_keys[key]
        if (row["intro"], row["base"], row["label"]) != (diff["intro"], diff["base"], labels[key]):
            raise ValueError(f"eligible and selected label snapshots disagree for {key}")
    return sha(raw), manifest, served, documents, eligible


def menu_query(step, depth, selector, seed):
    if step == "contract-cases":
        options = f':kind "{selector}"'
    else:
        direction, choice = selector.split()
        option = "port" if step == "value-transfers" else "event"
        options = f':direction "{direction}" :{option} "{choice}"'
    return f"({step} :depth {depth} {options} (procedure-of {seed}))"


def coverage_ledger(cases):
    ledger = {}
    current = identifier = None
    inside = cases.split("## Proposal coverage ledger\n", 1)[1].split("## Fidelity rulings", 1)[0]
    for line in inside.splitlines():
        if line.startswith("### "):
            current = LEDGER_HEADS[line[4:]]
        if line.startswith("| "):
            proposal, scope = line.split("|")[1:3]
            if "`" in proposal:
                identifier = proposal.split("`")[1]
            if identifier and scope.strip().startswith("Covered "):
                previous = ledger.get((current, identifier), "")
                ledger[current, identifier] = f"{previous} {scope.strip()}".strip()
    return ledger


def span(site):
    path, location = site.rsplit(":", 1)
    start, _, end = location.replace("–", "-").partition("-")
    return {"path": path, "start_line": int(start), "end_line": int(end or start)}


def decisive_queries(served, eligible, cases, catalogue):
    ledger = coverage_ledger(cases)
    grouped = {}
    for row in eligible:
        for step in served:
            key = step, row["id"]
            if key not in ledger:
                continue
            for group in row["label"]["decisive_sets"]:
                for fact in group["facts"]:
                    relation = fact.get("rql") or {}
                    proposal = relation.get("missing_step") or {}
                    if proposal.get("kind") != LABEL_KINDS[step]:
                        continue
                    if key in SEEDS:
                        depth, selector, pattern, name = SEEDS[key]
                    else:
                        original = relation["query"]
                        match = re.search(r'\((function|callable|call) :name ("(?:[^"\\]|\\.)*")\)', original)
                        if not match:
                            raise ValueError(f"no scoped seed for {key}: {original}")
                        pattern, name = match[1], json.loads(match[2])
                        bound = re.search(r":depth (\d+)", original)
                        depth = int(bound[1]) if bound else 0
                        selector = DEFAULT_SELECTORS[step]
                        if key == ("value-transfers", "cpp-template-inherited-base-signature"):
                            selector = "upstream argument"
                    seed = f"({pattern} :name {json.dumps(name)})"
                    query, _ = parse(menu_query(step, depth, selector, seed), catalogue)
                    pool = grouped.setdefault((step, row["repo"], row["intro"], query), {})
                    candidate = {"claim": fact["claim"], "scope": ledger[key],
                                 "sites": [span(site) for site in fact["sites"]]}
                    pool[canonical(candidate)] = candidate
    return [Query(step, "decisive", repo, commit, text, tuple(rows.values()))
            for (step, repo, commit, text), rows in sorted(grouped.items())]


def document_queries(diff, seed):
    decl = f"(enclosing-decl {seed})"
    for form in DOCUMENTED_FORMS:
        query = form.format(seed=seed, decl=decl)
        yield Query(DOCUMENTED, "random", diff["repo"], diff["intro"], query)


def draw(pool, n, rng):
    ordered = sorted({query.key: query for query in pool}.values())
    if len(ordered) < n:
        raise ValueError(f"need {n} distinct sample queries, have {len(ordered)}")
    return rng.sample(ordered, n)


def sample(manifest, served, documents, eligible, symbols):
    """Build disjoint strata and spread the fresh re-asks over all strata."""
    catalogue = documents["catalogue"]
    decisive = decisive_queries(served, eligible, documents["cases"], catalogue)
    # All-depth/all-kind variants on a decisive seed can still contain that
    # decisive row. Keep that symbol at its intro out of the same random step.
    excluded = {(query.step, query.commit, name)
                for query in decisive for name in re.findall(r':name ("(?:[^"\\]|\\.)*")', query.text)}
    pools = {step: [] for step in [*served, DOCUMENTED]}
    for diff in manifest["diffs"]:
        for path, kind, name in sorted(set(symbols(diff))):
            pattern = "callable" if kind == "definition" else "call"
            seed = f"(where {json.dumps(path)} ({pattern} :name {json.dumps(name)}))"
            for step in served:
                query = menu_query(step, 1, DEFAULT_SELECTORS[step], seed)
                item = Query(step, "random", diff["repo"], diff["intro"], query)
                if (step, diff["intro"], json.dumps(name)) not in excluded:
                    pools[step].append(item)
            pools[DOCUMENTED].extend(document_queries(diff, seed))
    seed = manifest["seed"]
    strata = {}
    for step in served:
        strata[step, "random"] = draw(pools[step], RANDOM_N, Random(f"{seed}/{step}/random"))
        strata[step, "decisive"] = [q for q in decisive if q.step == step]
    strata[DOCUMENTED, "random"] = draw(pools[DOCUMENTED], DOCUMENTED_N, Random(f"{seed}/documented/random"))

    rng = Random(f"{seed}/fresh")
    fresh_pools = [rows.copy() for rows in strata.values() if rows]
    for pool in fresh_pools:
        rng.shuffle(pool)
    rng.shuffle(fresh_pools)
    fresh = set()
    while fresh_pools and len(fresh) < STABILITY_N:
        for pool in fresh_pools:
            fresh.add(pool.pop().key)
            if len(fresh) == STABILITY_N:
                break
        fresh_pools = [pool for pool in fresh_pools if pool]
    queries = [q for rows in strata.values() for q in rows]
    for query in queries:
        parse(query.text, catalogue)
    return queries, fresh


def fraction(numerator, denominator):
    return numerator / denominator if denominator else 1.0


def row_counts(record):
    engine, gold = set(record["engine"]), set(record["gold"])
    return len(engine & gold), len(engine), len(gold)


def accuracy(records, seed):
    if not records:
        return {"precision": None, "recall": None, "precision_lower95": None,
                "recall_lower95": None, "true_positive": 0, "returned": 0, "gold": 0}
    counts = [row_counts(record) for record in records]
    correct, returned, relevant = map(sum, zip(*counts))
    rng = Random(seed)
    precision, recall = [], []
    for _ in range(BOOTSTRAPS):
        tp, predicted, gold = map(sum, zip(*rng.choices(counts, k=len(counts))))
        precision.append(fraction(tp, predicted))
        recall.append(fraction(tp, gold))
    index = int(0.025 * BOOTSTRAPS)
    return {"precision": fraction(correct, returned), "recall": fraction(correct, relevant),
            "precision_lower95": sorted(precision)[index], "recall_lower95": sorted(recall)[index],
            "true_positive": correct, "returned": returned, "gold": relevant}


def is_repair_stall(answer):
    return (answer is not None and answer.get("status") == "engine_error"
            and answer.get("failure") == REPAIR_STALLED)


def stratum_metrics(records, seed):
    metric = accuracy(records, seed)
    similarities = []
    for record in records:
        if record["fresh"] is None:
            continue
        first, again = set(record["engine"]), set(record["fresh"])
        # Either side failing leaves nothing to reproduce, so the pair scores zero;
        # two empty sets would otherwise count as a perfect match.
        if is_repair_stall(record.get("fresh_answer")) or is_repair_stall(record.get("engine_answer")):
            similarities.append(Fraction(0))
        else:
            similarities.append(Fraction(len(first & again), len(first | again)) if first or again else Fraction(1))
    stability = float(sum(similarities) / len(similarities)) if similarities else None
    return {**metric, "stability_n": len(similarities), "stability_jaccard": stability}


def prescience_counts(record):
    decisive, gold = set(record["decisive"]), set(record["gold"])
    engine, found = set(record["engine"]), set(record["enumerated"])
    comparable = decisive & found
    other = (gold - decisive) & found
    return (len(comparable & engine), len(comparable), len(other & engine), len(other),
            len(decisive - found))


def prescience_gap(counts):
    decisive_hit, decisive_found, other_hit, other_found, _ = counts
    if decisive_found and other_found:
        return Fraction(decisive_hit, decisive_found) - Fraction(other_hit, other_found)
    return None


def prescience_metrics(records):
    counts = [prescience_counts(record) for record in records]
    totals = tuple(map(sum, zip(*counts))) if counts else (0,) * 5
    decisive_hit, decisive_found, other_hit, other_found, shared_blind_spots = totals
    gap = prescience_gap(totals) if len(records) >= 10 else None
    estimable = gap is not None
    return {"status": "estimated" if estimable else "not_estimable",
            "gap": float(gap) if estimable else None,
            "decisive_recall": decisive_hit / decisive_found if estimable else None,
            "other_recall": other_hit / other_found if estimable else None,
            "decisive_enumerated": decisive_found, "other_enumerated": other_found,
            "shared_blind_spots": shared_blind_spots}


def pooled_prescience(records, seed, *, applicable=True):
    counts = [prescience_counts(record) for record in records]
    gap = prescience_gap(map(sum, zip(*counts))) if counts else None
    if gap is None:
        return {"gap": None, "interval": None, "decisive_n": len(records), "pass": not applicable,
                "status": "not_estimable" if applicable else "inapplicable",
                "reason": ("no comparable decisive and other rows in the pooled sample" if applicable
                           else "no added steps are served")}
    interval = None
    rng = Random(seed)
    estimates = []
    for _ in range(BOOTSTRAPS):
        replicate = prescience_gap(map(sum, zip(*rng.choices(counts, k=len(counts)))))
        if replicate is not None:
            estimates.append(float(replicate))
    # A resample may omit all evidence for one row class. Its gap is undefined.
    if estimates:
        estimates.sort()
        low = int(0.025 * len(estimates))
        interval = [estimates[low], estimates[len(estimates) - low - 1]]
    return {"gap": float(gap), "interval": interval, "decisive_n": len(records), "pass": gap <= Fraction(1, 10)}


BARS = {"precision": 0.95, "recall": 0.90, "precision_lower95": 0.90,
        "recall_lower95": 0.80, "stability_jaccard": 0.90}


def bar_failures(stratum, metrics):
    reasons = []
    for name, bound in BARS.items():
        observed = metrics[name]
        if observed is None:
            reasons.append(f"{stratum} {name} not measured")
        elif observed < bound:
            reasons.append(f"{stratum} {name} {observed:.4f} < {bound:.2f}")
    return reasons


def step_result(random_rows, decisive, seed):
    metrics = {"random": stratum_metrics(random_rows, f"{seed}/random"),
               "decisive": stratum_metrics(decisive, f"{seed}/decisive"),
               "prescience": prescience_metrics(decisive)}
    reasons = bar_failures("random", metrics["random"])
    if len(random_rows) != RANDOM_N:
        reasons.append(f"expected {RANDOM_N} random queries, got {len(random_rows)}")
    if len(decisive) >= 5:
        reasons.extend(bar_failures("decisive", metrics["decisive"]))
    return {"pass": not reasons, "reasons": reasons, "random_n": len(random_rows),
            "decisive_n": len(decisive), "prescience": metrics["prescience"]["status"], "metrics": metrics}


def validate_decisions(query, candidates, decisions):
    if not isinstance(decisions, dict):
        raise ValueError(f"{query.key}: adjudicator output is not an object")
    if set(decisions) != set(candidates):
        raise ValueError(f"{query.key}: adjudicator must classify every candidate")

    validity = {}
    for decision in decisions.values():
        if (not isinstance(decision, dict) or set(decision) != {"key", "in_scope", "reason"}
                or not isinstance(decision["key"], str) or not decision["key"].strip()
                or type(decision["in_scope"]) is not bool
                or not isinstance(decision["reason"], str) or not decision["reason"].strip()):
            raise ValueError(f"{query.key}: invalid adjudication {decision!r}")
        key, valid = decision["key"], decision["in_scope"]
        if validity.setdefault(key, valid) != valid:
            raise ValueError(f"{query.key}: conflicting decisions for semantic row {key!r}")
    return validity


def measure(query, fresh, engine, enumerate_rows, adjudicate):
    """Pool anonymous candidates and count typed semantic row identities."""
    def answer(again):
        result = engine(query.commit, query.text, fresh=again)
        if result["status"] != "ok" and not is_repair_stall(result):
            raise RuntimeError(f"{query.key}: engine {result['status']}: {result.get('message', '')}")
        return result

    first = answer(False)
    independent = enumerate_rows(query.repo, query.commit, query.text)
    again = answer(True) if fresh else None
    sources = {"engine": first["rows"] if first["status"] == "ok" else (),
               "enumerated": independent["rows"], "census": query.census,
               "fresh": again["rows"] if again and again["status"] == "ok" else ()}
    unique = sorted({canonical(row) for rows in sources.values() for row in rows})
    ids = {row: str(index) for index, row in enumerate(unique)}
    candidates = {ids[row]: json.loads(row) for row in unique}
    decisions = adjudicate(query, candidates) if candidates else {}
    validity = validate_decisions(query, candidates, decisions)

    def keys(source):
        return {decisions[ids[canonical(row)]]["key"] for row in sources[source]}

    engine_keys, enum_keys, census_keys = keys("engine"), keys("enumerated"), keys("census")
    return {"engine": sorted(engine_keys), "enumerated": sorted(engine_keys | enum_keys),
            "gold": sorted(k for k in engine_keys | enum_keys | census_keys if validity[k]),
            "decisive": sorted(k for k in census_keys if validity[k]),
            "fresh": sorted(keys("fresh")) if again else None,
            "engine_answer": first, "terra_answer": independent, "fresh_answer": again,
            "candidates": candidates, "decisions": decisions}


def gate_report(manifest_hash, served, queries, records, seed):
    groups = {}
    failures = []
    failure_counts = Counter()
    by_step = {}
    for query in queries:
        record = records[query.key]
        groups.setdefault((query.step, query.stratum), []).append(record)
        step = query.step
        if step == DOCUMENTED:
            step = query.text.split()[0][1:]
            by_step.setdefault(step, []).append(record)
        for field, fresh in (("engine_answer", False), ("fresh_answer", True)):
            answer = record.get(field)
            if is_repair_stall(answer):
                failures.append({"query_key": query.key, "step": step, "stratum": query.stratum,
                                 "fresh": fresh, "reason": answer["failure"],
                                 "repair_error_counts": answer["repair_error_counts"]})
                failure_counts[query.step, step] += 1
    documented = groups.get((DOCUMENTED, "random"), [])
    docs = stratum_metrics(documented, f"{seed}/documented")
    decisive = {step: groups.get((step, "decisive"), []) for step in served}
    pooled = pooled_prescience([row for step in served for row in decisive[step]], f"{seed}/prescience",
                               applicable=bool(served))
    steps = {step: {**step_result(groups[step, "random"], decisive[step], f"{seed}/{step}"),
                    "engine_failure_count": failure_counts[step, step]}
             for step in served}
    return {"manifest_sha256": manifest_hash, "served_steps": served,
            "steps": steps, "prescience_pooled": pooled,
            "engine_failures": failures, "engine_failure_count": len(failures),
            "documented": {"reported_only": True, "random_n": len(documented), "metrics": docs,
                           "reasons": bar_failures("documented pooled random", docs),
                           "prescience": "not_estimable",
                           "engine_failure_count": sum(failure_counts[DOCUMENTED, step] for step in by_step),
                           "by_step": {step: {"random_n": len(rows),
                                              "engine_failure_count": failure_counts[DOCUMENTED, step],
                                              "metrics": stratum_metrics(rows, f"{seed}/{step}")}
                                       for step, rows in sorted(by_step.items())}},
            "candidate_sources": ["engine", "terra_enumerator", "census"],
            "candidate_sources_note": "The pilot omits real Bifrost from the documented gold pool. The full gate includes it.",
            "prescience_method": "Compare decisive and other gold rows within decisive queries, "
                                 "restricted to rows the engine or Terra enumerated. "
                                 "Pool counts across added steps and report a percentile 95% bootstrap interval "
                                 "over queries, omitting replicates with an undefined gap. "
                                 "A point estimate above +0.10 blocks the pilot. "
                                 "Report census-only misses separately.",
            "accuracy_method": "Pool row counts within each stratum and bootstrap queries with replacement. "
                               "The lower bound is the 2.5th percentile of 10,000 seeded replicates. "
                               "An empty denominator scores 1. Stability averages query Jaccard, "
                               "with zero for a failed re-ask.",
            "created_at": datetime.now(timezone.utc).isoformat()}


if __name__ == "__main__":
    from .fidelity_live import main

    raise SystemExit(main())
