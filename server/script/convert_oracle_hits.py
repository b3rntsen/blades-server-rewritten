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
    "D0-attacker-snapshot",
    "D1-permanent",
    "D2-conversion",
    "D3-situational",
    "D-bonuses",
    "E0-defender-snapshot",
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
        if key == "activePerks" and isinstance(value, list):
            keep[key] = compact_active_perks(value)
            continue
        if key == "bonusSources" and isinstance(value, dict):
            keep[key] = compact_bonus_sources(value)
            continue
        if key == "bonusSourceCounts" and isinstance(value, dict):
            keep[key] = {
                name: count
                for name, count in value.items()
                if isinstance(count, int) and count != 0
            }
            continue
        keep[key] = value
    return keep


def compact_active_perks(perks):
    """Keep stable identity/rank/value fields from v4 active perk snapshots."""

    out = []
    for perk in perks:
        if not isinstance(perk, dict):
            continue
        key = perk.get("key") if isinstance(perk.get("key"), dict) else {}
        magnitudes = perk.get("magnitudes") if isinstance(perk.get("magnitudes"), dict) else {}
        kept = {}
        for field, value in (
            ("name", key.get("editorName")),
            ("uid", perk.get("id") or key.get("uid")),
            ("cls", perk.get("cls")),
            ("rank", perk.get("rank")),
            ("bonusValue", magnitudes.get("bonusValue")),
        ):
            if value is not None:
                kept[field] = value
        if kept:
            out.append(kept)
    return out


def compact_bonus_sources(lists):
    """Keep the replay-relevant source fields from v3 attacker snapshots.

    v1/v2 snapshots stored `bonusSources` as a flat count map. v3 snapshots store
    per-list source arrays; most entries contain pointer-heavy debug context and
    full tier curves. The oracle only needs the identity, magnitude, rank/filter
    properties and raw stored scalar to replay the damage stages.
    """

    out = {}
    for name, value in lists.items():
        if isinstance(value, int):
            out[name] = value
            continue
        if not isinstance(value, dict):
            continue
        sources = []
        for source in value.get("sources") or []:
            if not isinstance(source, dict):
                continue
            kept = {}
            for field in (
                "cls",
                "propertyId",
                "tier",
                "staticClass",
                "magnitude",
                "damageType",
                "damageTypes",
                "damageSources",
                "stored",
            ):
                if field in source:
                    kept[field] = source[field]
            sources.append(kept)
        out[name] = {"count": value.get("count", len(sources)), "sources": sources}
    return out


def stage_key(event):
    if event.get("ev") == "D3-situational":
        return "D3-physical" if event.get("cat") == "Physical" else "D4-nonphysical"
    return event.get("ev")


def compact_hit(run_name, ordinal, hit):
    stages = {}
    for event in hit["stages"]:
        stages[stage_key(event)] = clean_event(event)
    attacker_snapshot = stages.get("D0-attacker-snapshot")
    defender_snapshot = stages.get("E0-defender-snapshot")
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
        "attacker_snapshot": attacker_snapshot,
        "defender_snapshot": defender_snapshot,
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
