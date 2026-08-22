#!/usr/bin/env python3
"""Derive the K1 summary tables from the raw capture files.

Every number quoted in this directory's README comes out of here rather than
being typed by hand, so the README and the raw JSONL cannot drift apart.

Usage: python3 evidence/mooshik-k1-metal/summarize.py <evidence-dir>
"""

import json
import statistics
import sys
from pathlib import Path

# The recorded branch baseline: src/writeq.rs MEASURED_LOCAL_EMBEDDER_RPS and
# the surrounding doc comment. 4-wide at the 35-byte PROBE_TEXT.
BASELINE_35_4WIDE = (110, 141)
# The same comment records ~19-22 items/s 4-wide at PROBE_TEXT_BYTES = 1024.
BASELINE_1024_4WIDE = (19, 22)


def load(p):
    if not p.exists():
        return []
    return [json.loads(l) for l in p.read_text().splitlines() if l.strip()]


def med(xs):
    return statistics.median(xs) if xs else None


def main() -> None:
    ev = Path(sys.argv[1] if len(sys.argv) > 1 else ".")

    # ---- throughput -------------------------------------------------------
    legs = {
        "candle metal f16": ev / "bench-repeats-candle-metal-f16.jsonl",
        "candle metal f32": ev / "bench-repeats-candle-metal-f32.jsonl",
        "candle cpu f32 (Accelerate)": ev / "bench-repeats-candle-cpu-accelerate-f32.jsonl",
        "llama.cpp q8_0 (reference)": ev / "bench-repeats-llama-q8.jsonl",
    }

    throughput = {}
    for name, path in legs.items():
        recs = load(path)
        for size in (35, 1024):
            rows = [r for r in recs if r["input_bytes"] == size]
            if not rows:
                continue
            throughput.setdefault(str(size), {})[name] = {
                "n_repeats": len(rows),
                "serial_items_per_s": med([r["serial_items_per_s"] for r in rows]),
                "concurrent_items_per_s": med([r["concurrent_items_per_s"] for r in rows]),
                "batched_items_per_s": med(
                    [r["batched_items_per_s"] for r in rows if "batched_items_per_s" in r]
                ),
                "input_tokens": rows[0].get("input_tokens"),
            }

    # ---- falsifier 2 ------------------------------------------------------
    def half_band(band):
        return (band[0] / 2.0, band[1] / 2.0)

    f2 = {}
    for size, band in (("35", BASELINE_35_4WIDE), ("1024", BASELINE_1024_4WIDE)):
        lo, hi = half_band(band)
        entry = {"recorded_baseline_4wide": list(band), "half_baseline": [lo, hi], "legs": {}}
        for name, vals in throughput.get(size, {}).items():
            if name.startswith("llama"):
                continue
            conc = vals["concurrent_items_per_s"]
            batch = vals["batched_items_per_s"]
            entry["legs"][name] = {
                "concurrent": conc,
                "batched": batch,
                "concurrent_vs_half": "clears" if conc >= lo else "under",
                "best_vs_half": "clears" if max(conc, batch or 0) >= lo else "under",
            }
        f2[size] = entry

    # ---- cold start -------------------------------------------------------
    cold = {}
    for r in load(ev / "coldstart.jsonl"):
        cold.setdefault(r["backend"], []).append(r)
    coldstart = {
        b: {
            "runs": len(rs),
            "weight_load_ms_median": med([r["weight_load_ms"] for r in rs]),
            "process_to_first_vector_ms_median": med(
                [r["process_to_first_vector_ms"] for r in rs]
            ),
            "process_to_first_vector_ms_max": max(r["process_to_first_vector_ms"] for r in rs),
            "second_embed_ms_median": med([r["second_embed_ms"] for r in rs]),
        }
        for b, rs in sorted(cold.items())
    }

    # ---- parity -----------------------------------------------------------
    parity = {}
    for p in sorted(ev.glob("parity-*.json")):
        d = json.loads(p.read_text())
        parity[p.stem] = {
            "label": d["label"],
            "n": d["n"],
            "median": d["median"],
            "min": d["min"],
            "max": d["max"],
            "p05": d["p05"],
            "below_gate": len(d["below_gate"]),
            "verdict": d["verdict"],
        }

    summary = {
        "throughput_items_per_s_median_of_repeats": throughput,
        "falsifier_2_throughput": f2,
        "coldstart_ms": coldstart,
        "parity": parity,
        "cost_build": load(ev / "cost-build.jsonl"),
    }

    out = ev / "summary.json"
    out.write_text(json.dumps(summary, indent=2) + "\n")

    # ---- human-readable ---------------------------------------------------
    for size in ("35", "1024"):
        print(f"\n### input {size} B  (tokens: "
              f"{next(iter(throughput[size].values()))['input_tokens']})")
        print(f"{'backend':<30} {'serial':>9} {'4-wide':>9} {'batch-4':>9}")
        for name, v in throughput[size].items():
            b = v["batched_items_per_s"]
            print(
                f"{name:<30} {v['serial_items_per_s']:>9.1f} "
                f"{v['concurrent_items_per_s']:>9.1f} "
                f"{(f'{b:.1f}' if b else '-'):>9}"
            )
        band = f2[size]["half_baseline"]
        print(f"  half-baseline band: {band[0]:.1f}-{band[1]:.1f} items/s")
        for name, leg in f2[size]["legs"].items():
            print(f"    {name:<30} 4-wide {leg['concurrent_vs_half']:<7} "
                  f"best {leg['best_vs_half']}")

    print("\n### cold start (ms)")
    print(f"{'backend':<14} {'load':>9} {'->1st vec':>11} {'max':>9} {'2nd embed':>10}")
    for b, v in coldstart.items():
        print(
            f"{b:<14} {v['weight_load_ms_median']:>9.0f} "
            f"{v['process_to_first_vector_ms_median']:>11.0f} "
            f"{v['process_to_first_vector_ms_max']:>9.0f} "
            f"{v['second_embed_ms_median']:>10.1f}"
        )

    print("\n### parity")
    for k, v in parity.items():
        print(f"  {v['label']:<46} median={v['median']:.6f} min={v['min']:.6f} "
              f"below={v['below_gate']} {v['verdict']}")

    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
