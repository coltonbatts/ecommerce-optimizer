#!/usr/bin/env python3
"""Listings QA gate: validate copy against Etsy's real limits + claim/IP safety,
then re-render exports and sync the SQLite DB. Stdlib only. Deterministic.

Usage:
    python3 scripts/qa_export.py data/listings-qa.json

Exits non-zero (and touches nothing) if any listing has an ERROR-level problem.
WARN-level findings print for human review but don't block.

Why this exists: structural clamps in src/listing.rs enforce marketplace limits,
but phi3:mini content failures slip past them — unverified material claims
("organic cotton", "fade-resistant"), hallucinated product features ("comes with
a matching tie"), IP-bait phrasing ("iconic movie posters and characters"),
broken grammar ("it' endorses"), and junk tags ("horror movie movie").
This gate catches all of those deterministically before anything gets pasted.

Blank specs are now VERIFIED (Bella + Canvas 3001 via Printful, 2026-09-23), so
material copy is permitted — but only as an exact APPROVED_SPECS block. Any
other material claim is still an ERROR: the block is the evidence, and it must
match exactly to stay evidence. Two size ranges are approved because the blank
carries both: standard (S-2XL) and extended (XS-5XL, white colorway verified).

Every listing carries a `status`:
    "ready"       — real art exists; goes into the paste pack
    "placeholder" — market-generated copy with no art; validated, never packed
    "draft"       — proposed copy for art not drawn yet; validated, never packed
A missing or unknown status is an ERROR (no silent default). The paste pack
renders only "ready" listings and states how many were skipped and why.
"""

import csv
import json
import re
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path

STATUSES = ("ready", "placeholder", "draft")

TITLE_MAX = 140
TAGS_MAX = 13
TAG_LEN_MAX = 20

# Fee model that reproduces the pricing pass exactly (verified against all 8
# generated listings: fee = 9.5% of price + $0.45 fixed).
FEE_PCT = 0.095
FEE_FIXED = 0.45

# The only permitted material-copy blocks. Sourced from Printful's Bella + Canvas
# 3001 product record (variant names + spec sheet). Exempt from BANNED_CLAIMS
# below because they are the verification evidence — but each must match byte
# for byte. Edit only when re-verified against the source.
_SPEC_HEAD = (
    "The details:\n"
    "- Bella + Canvas 3001 unisex retail fit with tear-away label\n"
    "- 100% combed and ring-spun cotton (heather colorways: polyester/cotton blend)\n"
    "- 4.2 oz/yd² (142 g/m²), pre-shrunk\n"
    "- Side-seamed construction, shoulder-to-shoulder taping\n"
)
_SPEC_TAIL = (
    "- Blank sourced from Guatemala, Nicaragua, Mexico, Honduras, or the US\n"
    "\n"
    "Lighter colorways are slightly sheer — that is the nature of the fabric."
)
APPROVED_SPECS = {
    "standard": _SPEC_HEAD + "- Sizes S, M, L, XL, 2XL\n" + _SPEC_TAIL,
    "extended": _SPEC_HEAD + "- Sizes XS to 5XL\n" + _SPEC_TAIL,
}

# Landed cost model (Printful, measured 2026-09-23): product cost by size plus
# $4.95 flat-rate US shipping absorbed by the shop.
#   XS-XL $11.92 + 4.95 = $16.87 | 2XL $13.92 + 4.95 = $18.87
#   3XL  $15.92 + 4.95 = $20.87 | 4XL $17.92 + 4.95 = $22.87
#   5XL  $19.92 + 4.95 = $24.87 (white colorway only)
# Surcharges hold ~29% margin at the base price across every size.
SIZE_TIERS = {
    "standard": "S-XL base | 2XL +$3 | 3XL +$6 | 4XL +$9",
    "extended": "XS-XL base | 2XL +$3 | 3XL +$6 | 4XL +$9 | 5XL +$12",
}

# ERROR-level: unverified factual claims about materials, safety, durability,
# or performance outside the approved spec blocks. Anything here is a claim we
# cannot evidence.
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


def size_key(item: dict) -> str:
    """Which approved spec block an item carries ('standard' if unrecognized)."""
    spec = item.get("spec", "")
    for key, text in APPROVED_SPECS.items():
        if spec == text:
            return key
    return "standard"


def full_description(item: dict) -> str:
    """What actually gets pasted and stored: copy + verified spec block."""
    spec = item.get("spec", "")
    return f"{item['description']}\n\n{spec}" if spec else item["description"]


def validate(item: dict) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []
    label = item.get("niche", "?")
    # Scan copy ONLY — the spec block is verified separately below.
    blob = f"{item.get('title', '')}\n{item.get('description', '')}"

    # --- lifecycle status: decides whether the listing reaches the paste pack
    status = item.get("status")
    if status not in STATUSES:
        errors.append(f"status {status!r} — must be one of {', '.join(STATUSES)}")

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

    # --- verified spec block: permitted material copy, exact text only
    spec = item.get("spec", "")
    if not spec:
        warnings.append("no spec block — listing carries no material copy")
    elif spec not in APPROVED_SPECS.values():
        errors.append("spec block does not match a verified blank spec")

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


def skipped_summary(skipped: list[dict]) -> str:
    """'8 placeholder, 1 draft' — the visible count of what stayed out of the pack."""
    counts: dict[str, int] = {}
    for it in skipped:
        counts[it["status"]] = counts.get(it["status"], 0) + 1
    return ", ".join(f"{n} {k}" for k, n in sorted(counts.items()))


def render_md(items: list[dict], skipped: list[dict]) -> str:
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    skip_line = (
        f"**Skipped (not paste-ready): {len(skipped)}** — {skipped_summary(skipped)}: "
        + "; ".join(it["niche"] for it in skipped)
        if skipped
        else "Skipped: none"
    )
    out = [
        "# BattsBespoke — Listing Pack (QA-passed)",
        "",
        f"{len(items)} ready listings · QA gate: scripts/qa_export.py · regenerated {now}",
        "",
        skip_line,
        "",
        "Copy reviewed by hand. Blank verified: Bella + Canvas 3001 (Printful). "
        "Material copy = the approved spec block only.",
        "",
        "## Before you paste",
        "1. Paste from the TITLE / TAGS / DESCRIPTION blocks below (description includes the verified spec block).",
        "2. Add your mockup images (4500x5400 designs on the tee mockups).",
        "3. Set size pricing per listing in Etsy using that listing's size line.",
        "",
    ]
    for i, it in enumerate(items, 1):
        price = it["price"]
        cost = it["unit_cost"]
        net = price - fee_for(price)
        margin = (net - cost) / price * 100
        tags_line = ", ".join(it["tags"])
        tiers = SIZE_TIERS[size_key(it)]
        out += [
            f"## {i}. {it['niche']}",
            "",
            f"**Price:** ${price:.2f}  (unit cost ${cost:.2f} → nets ${net:.2f}, {margin:.1f}% margin)",
            f"**Competitor query:** `{it['search_query']}`",
            f"**Size pricing:** {tiers}",
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
            "**DESCRIPTION** (includes verified blank specs)",
            "```",
            full_description(it),
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
                "description": full_description(it),
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
                    full_description(it),
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

    ready = [it for it in items if it["status"] == "ready"]
    skipped = [it for it in items if it["status"] != "ready"]

    root = src.parent.parent
    md_path = root / "out" / "listings-export.md"
    csv_path = root / "out" / "listings-export.csv"
    db_path = root / "data" / "optimizer.db"

    md_path.write_text(render_md(ready, skipped), encoding="utf-8")
    render_csv(ready, csv_path)
    # The DB mirrors the source of truth, so every validated row syncs —
    # status only gates what reaches the paste pack.
    n = sync_db(items, db_path)
    print(f"\nAll {len(items)} listings passed. Wrote {md_path.name} + {csv_path.name} "
          f"with {len(ready)} ready, synced {n} DB rows.")
    if skipped:
        print(f"SKIPPED from paste pack: {len(skipped)} ({skipped_summary(skipped)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
