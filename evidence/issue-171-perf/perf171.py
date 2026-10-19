#!/usr/bin/env python3
"""#171 before/after driver: the issue's methodology, scripted.

    python3 evidence/issue-171-perf/perf171.py --bin path/to/lambo --label main --out main.json

Provisions a scratch SQLite store (fixture embedder, dim 1024), starts
`lambo serve --transport http --rate-limit-rps 0`, populates it the way the
issue did (142 x lambo_derive of 64 concepts, 200 lambo_record_action for the
SG-Base-VPC-* dependency edges), restarts the serve (session load), then
times: keyword/semantic recall, structural hit and miss, identifier-shaped
recall, 16 concurrent sessions x 20 recalls, 30 back-to-back single-concept
derives (ack and applied), and `lambo provision` on the loaded store.

Stdlib only; reuses scripts/loadtest/mcp_ab.Mcp. Aggregates only in the
output. Scratch files live under a mktemp dir that is removed afterwards.
"""
import argparse, json, os, shutil, statistics, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "scripts", "loadtest"))
import mcp_ab  # noqa: E402

TOPICS = ["vpc", "subnet", "route table", "security group", "load balancer", "rds instance",
          "lambda function", "s3 bucket", "iam role", "kms key", "cloudwatch alarm",
          "dynamodb table", "sqs queue", "sns topic", "ecs service", "eks cluster"]
VERBS = ["allows", "denies", "depends on", "replicates to", "logs to", "encrypts",
         "routes through", "scales with"]
KEYWORD_QUERIES = mcp_ab.QUERIES + [f"{t} configuration" for t in TOPICS] + \
    [f"why {v} {t}" for v in VERBS[:3] for t in TOPICS[:8]]


def summ(xs):
    return mcp_ab.summ(xs)


def concept_text(batch, i):
    t = TOPICS[(batch * 7 + i) % len(TOPICS)]
    v = VERBS[(batch + i) % len(VERBS)]
    u = TOPICS[(batch * 3 + i * 5) % len(TOPICS)]
    return f"{t} {batch}-{i} {v} {u} in region r{(batch + i) % 9} for workload w{batch % 13}"


def start(bin_, cfg, port, runtime, log):
    env = dict(os.environ, XDG_RUNTIME_DIR=runtime, RUST_LOG="lambo=warn")
    p = subprocess.Popen([bin_, "serve", "--config", cfg, "--session", "perf171", "--agent",
                          "perf171", "--transport", "http", "--port", str(port), "--bind",
                          "127.0.0.1", "--rate-limit-rps", "0"],
                         stdout=log, stderr=subprocess.STDOUT, env=env)
    url = f"http://127.0.0.1:{port}/mcp"
    t0 = time.perf_counter()
    for _ in range(400):
        try:
            c = mcp_ab.Mcp(url)
            c.tool("lambo_stats")
            return p, url, (time.perf_counter() - t0) * 1000
        except Exception:
            if p.poll() is not None:
                raise RuntimeError("serve exited")
            time.sleep(0.05)
    raise RuntimeError("serve never answered")


def stop(p):
    p.terminate()
    try:
        p.wait(30)
    except subprocess.TimeoutExpired:
        p.kill()


DRAIN_KEYS = ("write_queue_outstanding", "log_depth", "flush_depth")


def wait_drained(c, timeout=900):
    """Poll lambo_stats until the write queue, log and flush are all empty."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        _, st, err = c.tool("lambo_stats")
        if err:
            raise RuntimeError(f"lambo_stats refused while draining: {st}")
        if all((st.get(k) or 0) == 0 for k in DRAIN_KEYS):
            return round(time.time() - t0, 1)
        time.sleep(0.5)
    raise RuntimeError("write queue never drained")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--port", type=int, default=17931)
    ap.add_argument("--batches", type=int, default=142)
    ap.add_argument("--settle", action="store_true",
                    help="wait for the write queue to drain after every derive batch, so "
                         "the populated store does not depend on write-behind timing")
    o = ap.parse_args()
    work = tempfile.mkdtemp(prefix="perf171-")
    runtime = tempfile.mkdtemp(prefix="l171-", dir="/tmp")
    res = {"label": o.label}
    procs = []
    try:
        db = os.path.join(work, "perf171.db")
        cfg = os.path.join(work, "lambo.toml")
        with open(cfg, "w") as f:
            f.write(f'[store]\nkind = "sqlite"\npath = "{db}"\n\n[embedder]\nkind = "fixture"\ndim = 1024\n')
        subprocess.run([o.bin, "provision", "--config", cfg], check=True, capture_output=True)
        log = open(os.path.join(work, "serve.log"), "w")
        p, url, _ = start(o.bin, cfg, o.port, runtime, log)
        procs.append(p)
        c = mcp_ab.Mcp(url)
        t0 = time.perf_counter()
        batch_ack, batch_drain = [], []
        for b in range(o.batches):
            tb = time.perf_counter()
            concepts = [{"content": concept_text(b, i), "concept_type": "observation" if i % 3 else "entity"}
                        for i in range(64)]
            _, body, err = c.tool("lambo_derive", concepts=concepts)
            if err:
                raise RuntimeError(f"derive batch {b} refused")
            batch_ack.append((time.perf_counter() - tb) * 1000)
            if o.settle:
                td = time.perf_counter()
                wait_drained(c)
                batch_drain.append((time.perf_counter() - td) * 1000)
        for k in range(200):
            _, body, err = c.tool("lambo_record_action",
                                  action=f"deploy service s{k} into SG-Base-VPC-{k % 20}",
                                  produces=[f"service-s{k}"],
                                  depends_on=[f"SG-Base-VPC-{k % 20}"])
            if err:
                raise RuntimeError(f"record_action {k} refused")
        res["ack_all_writes_s"] = round(time.perf_counter() - t0, 1)
        res["derive_batch_ack_ms"] = summ(batch_ack)
        if batch_drain:
            res["derive_batch_drain_ms"] = summ(batch_drain)
            res["derive_batch_drain_total_s"] = round(sum(batch_drain) / 1000, 1)
        res["drain_after_acks_s"] = wait_drained(c)
        res["populate_s"] = round(time.perf_counter() - t0, 1)
        _, st, _ = c.tool("lambo_stats")
        res["store_after_populate"] = {k: st.get(k) for k in ("concept_count", "edge_count", "node_count", "embedded_concepts")}
        stop(p)
        res["db_mb"] = round(os.path.getsize(db) / 1e6, 1)

        # Provision on the loaded store (the #170 re-key pass, or its skip).
        prov = []
        for _ in range(5):
            t = time.perf_counter()
            subprocess.run([o.bin, "provision", "--config", cfg], check=True, capture_output=True)
            prov.append((time.perf_counter() - t) * 1000)
        res["provision_ms"] = summ(prov)

        p, url, startup = start(o.bin, cfg, o.port, runtime, log)
        procs.append(p)
        res["startup_loaded_ms"] = round(startup, 1)
        c = mcp_ab.Mcp(url)
        lat = {}
        for q in KEYWORD_QUERIES[:3]:
            c.tool("lambo_recall", query=q)

        def timed(name, queries, reps=1):
            xs = []
            for _ in range(reps):
                for q in queries:
                    ms, _, err = c.tool("lambo_recall", query=q, top_k=8)
                    if err:
                        raise RuntimeError(f"recall refused: {q}")
                    xs.append(ms)
            lat[name] = summ(xs)

        timed("keyword_semantic_recall", KEYWORD_QUERIES[:60])
        timed("structural_hit", [f"what depends on SG-Base-VPC-{k % 20}" for k in range(30)])
        timed("structural_miss", [f"what depends on nothing-named-{k}" for k in range(30)])
        timed("identifier_recall", [f"release v0.{k}.{k % 3} sha 2122de39e47{k % 10}" for k in range(30)])

        # 16 concurrent sessions x 20 recalls.
        clients = [mcp_ab.Mcp(url) for _ in range(16)]
        conc, errs, failures, lock = [], [0], [], threading.Lock()

        def worker(cl, w):
            try:
                for j in range(20):
                    q = KEYWORD_QUERIES[(w * 20 + j) % len(KEYWORD_QUERIES)] + f" {w}-{j}"
                    ms, _, err = cl.tool("lambo_recall", query=q, top_k=8)
                    with lock:
                        if err:
                            errs[0] += 1
                        else:
                            conc.append(ms)
            except Exception as e:  # surfaced on the main thread below
                with lock:
                    failures.append(repr(e))
        ts = [threading.Thread(target=worker, args=(cl, w)) for w, cl in enumerate(clients)]
        t = time.perf_counter()
        for th in ts:
            th.start()
        for th in ts:
            th.join()
        wall = time.perf_counter() - t
        if failures or errs[0] or len(conc) != 16 * 20:
            raise RuntimeError(
                f"concurrent recall: {len(conc)}/320 ok, {errs[0]} refused, failures {failures[:3]}")
        lat["recall_16_concurrent"] = dict(summ(conc), rps=round(len(conc) / wall, 1), errors=errs[0])

        # 30 back-to-back single-concept derives: ack, then applied.
        ack, applied = [], []
        for i in range(30):
            ms, body, err = c.tool("lambo_derive", concepts=[
                {"content": f"perf171 back-to-back observation {i} {time.time_ns()}", "concept_type": "observation"}])
            ack.append(ms)
            t = time.perf_counter()
            _, st, err = c.tool("lambo_stats", receipt=body["receipt"], wait_ms=10000)
            if err:
                raise RuntimeError(f"receipt poll {i} refused: {st}")
            applied.append(ms + (time.perf_counter() - t) * 1000)
        lat["derive_ack_back_to_back"] = summ(ack)
        lat["derive_applied_back_to_back"] = summ(applied)
        iso = []
        for i in range(15):
            time.sleep(0.7)
            ms, body, err = c.tool("lambo_derive", concepts=[
                {"content": f"perf171 isolated observation {i} {time.time_ns()}", "concept_type": "observation"}])
            iso.append(ms)
        lat["derive_ack_isolated"] = summ(iso)
        res["latency_ms"] = lat
        stop(p)
        log.close()
    finally:
        # A failed step skips the success-path stops: never leave a server on
        # the benchmark port.
        for p in procs:
            if p.poll() is None:
                stop(p)
        shutil.rmtree(work, ignore_errors=True)
        shutil.rmtree(runtime, ignore_errors=True)
    with open(o.out, "w") as f:
        json.dump(res, f, indent=2)
    print(json.dumps(res, indent=2))


if __name__ == "__main__":
    main()
