#!/usr/bin/env python3
"""Convert Frida combat hooks into compact oracle fixture hits.

Input is one or more `.cre/runs/<run>/hooks.jsonl` files. Output is one JSONL file
per run containing only real (NotSimulated) hits that reached Actor.ReceiveDamage.
The grouping mirrors `.cre/summarize.py`, but keeps only the scalar and DamageList
fields the Rust oracle needs.
"""

import argparse
import json
from pathlib import Path

STAGES = {
    "attack-entry",
    "D1-permanent",
    "D2-conversion",
    "D3-situational",
    "D-bonuses",
    "E0-taken-entry",
    "E1-negation",
    "E3-preblock",
    "E4-blocking",
    "E5-outer",
    "E6-resistance",
    "E7-taken-exit",
    "G-receive",
    "apply-damage",
}
CONTEXT = {
    "weapon-attack",
    "generic-entry",
    "maneuver-entry",
    "status-dmg-entry",
    "attack-type-factor",
    "charge-damage-factor",
}


def load_events(path):
    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def build_hits(events):
    hits = []
    open_hits = {}
    context = []
    for event in events:
        name = event.get("ev")
        if name in CONTEXT:
            context.append(event)
            context = context[-8:]
            continue
        dl = event.get("dl")
        if not dl or name not in STAGES:
            continue
        hit = open_hits.get(dl)
        if hit is None:
            same_frame = [c for c in context if c.get("f") == event.get("f")]
            hit = {
                "dl": dl,
                "t": event.get("t"),
                "f": event.get("f"),
                "context": same_frame,
                "stages": [],
            }
            open_hits[dl] = hit
            hits.append(hit)
            context = [c for c in context if c not in same_frame]
        hit["stages"].append(event)
        if name == "E0-taken-entry":
            hit["sim"] = event.get("sim")
            hit["source"] = event.get("source")
            hit["atk"] = event.get("atk")
            hit["def"] = event.get("def")
        sim = hit.get("sim")
        if (
            name == "apply-damage"
            or (name == "E7-taken-exit" and sim not in (None, "NotSimulated"))
            or (name == "E1-negation" and event.get("negated"))
        ):
            open_hits.pop(dl, None)
    return hits


def clean_event(event):
    keep = {}
    for key, value in event.items():
        if key in {"ht", "t"}:
            continue
        keep[key] = value
    return keep


def stage_key(event):
    if event.get("ev") == "D3-situational":
        return "D3-physical" if event.get("cat") == "Physical" else "D4-nonphysical"
    return event.get("ev")


def compact_hit(run_name, ordinal, hit):
    stages = {}
    for event in hit["stages"]:
        stages[stage_key(event)] = clean_event(event)
    receive = stages.get("G-receive", {})
    before_h = ((receive.get("before") or {}).get("H") or [None])[0]
    after_h = ((receive.get("after") or {}).get("H") or [None])[0]
    health_delta = None
    if before_h is not None and after_h is not None:
        health_delta = round(before_h - after_h, 6)
    attack_type = None
    for event in hit.get("context", []):
        if (
            event.get("ev") == "attack-type-factor"
            and event.get("src") == hit.get("atk")
            and event.get("source") == hit.get("source")
        ):
            attack_type = clean_event(event)
    return {
        "run": run_name,
        "hit": ordinal,
        "dl": hit.get("dl"),
        "frame": hit.get("f"),
        "attacker": hit.get("atk"),
        "defender": hit.get("def"),
        "source": hit.get("source"),
        "sim": hit.get("sim"),
        "attack_type": attack_type,
        "stages": stages,
        "health_before": before_h,
        "health_after": after_h,
        "health_delta": health_delta,
    }


def convert_run(run_dir, out_dir):
    events = list(load_events(run_dir / "hooks.jsonl"))
    real = [
        h
        for h in build_hits(events)
        if h.get("sim") == "NotSimulated"
        and any(s.get("ev") == "G-receive" for s in h.get("stages", []))
    ]
    out_dir.mkdir(parents=True, exist_ok=True)
    out_path = out_dir / f"{run_dir.name}.jsonl"
    with out_path.open("w") as f:
        for idx, hit in enumerate(real):
            f.write(json.dumps(compact_hit(run_dir.name, idx, hit), separators=(",", ":")) + "\n")
    return out_path, len(real)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("runs", nargs="+", type=Path)
    parser.add_argument(
        "--out",
        type=Path,
        default=Path("server/src/arena/combat/testdata/oracle"),
    )
    args = parser.parse_args()
    for run in args.runs:
        out, count = convert_run(run, args.out)
        print(f"{run.name}: wrote {count} real hits to {out}")


if __name__ == "__main__":
    main()
