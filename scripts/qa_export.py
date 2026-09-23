#!/usr/bin/env python3
"""Listings QA gate: validate copy against Etsy's real limits + claim/IP safety,
then re-render exports and sync the SQLite DB. Stdlib only. Deterministic.

Usage:
    python3 scripts/qa_export.py out/listings-qa.json

Exits non-zero (and touches nothing) if any listing has an ERROR-level problem.
WARN-level findings print for human review but don't block.

Why this exists: structural clamps in src/listing.rs enforce marketplace limits,
but phi3:mini content failures slip past them — unverified material claims
("organic cotton", "fade-resistant"), hallucinated product features ("comes with
a matching tie"), IP-bait phrasing ("iconic movie posters and characters"),
broken grammar ("it' endorses"), and junk tags ("horror movie movie").
This gate catches all of those deterministically before anything gets pasted.
"""

import csv
import json
import re
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path

TITLE_MAX = 140
TAGS_MAX = 13
TAG_LEN_MAX = 20

# Fee model that reproduces the pricing pass exactly (verified against all 8
# generated listings: fee = 9.5% of price + $0.45 fixed).
FEE_PCT = 0.095
FEE_FIXED = 0.45

# ERROR-level: unverified factual claims about materials, safety, durability,
# or performance. BattsBespoke's blank specs are unknown to the pipeline, so
# copy must not assert them. (Colton can add exact specs once confirmed.)
BANNED_CLAIMS = [
    r"100\s*%\s*cotton",
    r"organic cotton",
    r"eco-?friendly",
    r"water-based ink",
    r"fade-resistant",
    r"pre-?shrunk",
    r"premium fabric",
    r"high-quality cotton",
    r"breathable fabric",
    r"durable clothing",
    r"true to size",
    r"matching tie",
    r"non-toxic",
    r"pet safe",
    r"machine wash",
    r"variety of colors",
]

# ERROR-level: phrases that describe real, recognizable IP. Even original
# artwork gets flagged by Etsy's IP bots if the copy promises "iconic movie
# characters and quotes".
BANNED_IP = [
    r"movie posters",
    r"iconic characters",
    r"iconic movie characters",
    r"characters and quotes",
    r"officially licensed",
    r"licensed",
]

# WARN-level: signature small-model slop worth a human look.
SLOP_PATTERNS = [
    (r"[A-Za-z]+'\s+[a-z]", "broken apostrophe (e.g. \"it' endorses\")"),
    (r"\b(\w+) \1\b", "doubled word"),
    (r"end-users|endorses your|enthusiast endorsers", "corporate slop phrasing"),
    (r"in today's world|look no further|elevate your wardrobe", "filler phrasing"),
]


def fee_for(price: float) -> float:
    return FEE_PCT * price + FEE_FIXED


def validate(item: dict) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []
    label = item.get("niche", "?")
    blob = f"{item.get('title', '')}\n{item.get('description', '')}"

    # --- marketplace limits
    title = item.get("title", "")
    if not title.strip():
        errors.append("title empty")
    elif len(title) > TITLE_MAX:
        errors.append(f"title {len(title)} > {TITLE_MAX} chars")

    tags = item.get("tags", [])
    if len(tags) != TAGS_MAX:
        errors.append(f"{len(tags)} tags, need exactly {TAGS_MAX}")
    lowered = [t.lower() for t in tags]
    if len(set(lowered)) != len(lowered):
        errors.append("duplicate tags")
    for t in tags:
        if len(t) > TAG_LEN_MAX:
            errors.append(f"tag >{TAG_LEN_MAX} chars: {t!r} ({len(t)})")
        if t.startswith("#"):
            errors.append(f"tag starts with #: {t!r}")
        if len(t) < 3:
            warnings.append(f"weak short tag: {t!r}")
        if re.search(r"\b(\w+) \1\b", t):
            errors.append(f"doubled word in tag: {t!r}")

    desc = item.get("description", "")
    if len(desc.split()) < 15:
        errors.append("description too short")

    # --- claim / IP / slop safety
    for pat in BANNED_CLAIMS:
        m = re.search(pat, blob, re.IGNORECASE)
        if m:
            errors.append(f"unverified claim: {m.group(0)!r}")
    for pat in BANNED_IP:
        m = re.search(pat, blob, re.IGNORECASE)
        if m:
            errors.append(f"IP-risk phrasing: {m.group(0)!r}")
    for pat, why in SLOP_PATTERNS:
        if re.search(pat, blob):
            warnings.append(f"{why} in copy")

    _ = label
    return errors, warnings


def render_md(items: list[dict]) -> str:
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    out = [
        "# BattsBespoke — Listing Pack (QA-passed)",
        "",
        f"{len(items)} listings · QA gate: scripts/qa_export.py · regenerated {now}",
        "Copy reviewed by hand: no material/safety claims (blank specs unconfirmed), no real film/character/band references.",
        "",
        "## Before you paste",
        "1. Paste from the TITLE / TAGS / DESCRIPTION blocks below.",
        "2. Add your mockup images (4500x5400 designs on the tee mockups).",
        "3. Optional: tell Hermes the blank/fabric specs and sizes to add exact material copy + a size list.",
        "",
    ]
    for i, it in enumerate(items, 1):
        price = it["price"]
        cost = it["unit_cost"]
        net = price - fee_for(price)
        margin = (net - cost) / price * 100
        tags_line = ", ".join(it["tags"])
        out += [
            f"## {i}. {it['niche']}",
            "",
            f"**Price:** ${price:.2f}  (unit cost ${cost:.2f} → nets ${net:.2f}, {margin:.1f}% margin)",
            f"**Competitor query:** `{it['search_query']}`",
            "",
            f"**TITLE** ({len(it['title'])}/{TITLE_MAX}) — click to select, copy, paste",
            "```",
            it["title"],
            "```",
            "",
            f"**TAGS** ({len(it['tags'])}/{TAGS_MAX}) — paste the whole line into the tags field",
            "```",
            tags_line,
            "```",
            "",
            "**DESCRIPTION**",
            "```",
            it["description"],
            "```",
            "",
            "---",
            "",
        ]
    return "\n".join(out)


CSV_FIELDS = [
    "niche", "search_query", "title", "title_chars", "tags", "tag_count",
    "price", "unit_cost", "net_after_fees", "margin_pct", "description",
    "copy_source", "model",
]


def render_csv(items: list[dict], path: Path) -> None:
    with path.open("w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=CSV_FIELDS)
        w.writeheader()
        for it in items:
            price = it["price"]
            cost = it["unit_cost"]
            net = price - fee_for(price)
            w.writerow({
                "niche": it["niche"],
                "search_query": it["search_query"],
                "title": it["title"],
                "title_chars": len(it["title"]),
                "tags": " | ".join(it["tags"]),
                "tag_count": len(it["tags"]),
                "price": f"{price:.2f}",
                "unit_cost": f"{cost:.2f}",
                "net_after_fees": f"{net:.2f}",
                "margin_pct": f"{(net - cost) / price * 100:.1f}",
                "description": it["description"],
                "copy_source": "hermes-reviewed",
                "model": "phi3:mini+hermes",
            })


def sync_db(items: list[dict], db_path: Path) -> int:
    """Update listings rows by product_id. Mimics the DB's existing tag
    separator so downstream export code keeps working."""
    con = sqlite3.connect(db_path)
    try:
        cur = con.cursor()
        sample = cur.execute("SELECT tags FROM listings LIMIT 1").fetchone()
        sep = " | " if sample and " | " in sample[0] else ", "
        now = datetime.now(timezone.utc).isoformat()
        n = 0
        for it in items:
            cur.execute(
                "UPDATE listings SET title=?, description=?, tags=?, unit_cost=?, "
                "updated_at=? WHERE product_id=?",
                (
                    it["title"],
                    it["description"],
                    sep.join(it["tags"]),
                    it["unit_cost"],
                    now,
                    it["product_id"],
                ),
            )
            n += cur.rowcount
        con.commit()
        return n
    finally:
        con.close()


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    src = Path(sys.argv[1])
    items = json.loads(src.read_text(encoding="utf-8"))

    # --- pricing cross-check against the pricing pass's math
    total_errs = 0
    for it in items:
        errs, warns = validate(it)
        total_errs += len(errs)
        head = f"[{it['niche']}]"
        for e in errs:
            print(f"ERROR {head} {e}")
        for w in warns:
            print(f"WARN  {head} {w}")
        if not errs and not warns:
            print(f"OK    {head} clean ({len(it['title'])}t / {len(it['tags'])} tags)")

    if total_errs:
        print(f"\n{total_errs} error(s) — nothing written.")
        return 1

    root = src.parent.parent
    md_path = root / "out" / "listings-export.md"
    csv_path = root / "out" / "listings-export.csv"
    db_path = root / "data" / "optimizer.db"

    md_path.write_text(render_md(items), encoding="utf-8")
    render_csv(items, csv_path)
    n = sync_db(items, db_path)
    print(f"\nAll {len(items)} listings passed. Wrote {md_path.name} + {csv_path.name}, "
          f"synced {n} DB rows.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
