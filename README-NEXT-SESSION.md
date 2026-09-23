# Next steps

State as of this session. The pipeline is real end to end — this file is what's
actually left, not what was already done.

## Working now

- `scan` → 16 opportunities (seed provider; Etsy v3 provider implemented, key-gated)
- `generate` → **16/16 listings written by phi3:mini locally**, ~54s for all 16
- `optimize` → 16/16 priced, margin floor verified independently at ≥29.99%
- `watch`, `status`, `doctor`, `--json`, `--dry-run` all exercised
- 13 unit tests green

## Blocked on you

### 1. Etsy API key — unblocks two things at once

`src/scanner.rs::scan_etsy` and `src/pricing.rs::fetch_competitors` are written
and compile, but have never run against live data because no key exists.

- Register at https://developers.etsy.com
- The `x-api-key` header needs `keystring:shared_secret` (bare keystring → HTTP 403)
- Then: `--api-key "ks:ss"` or put it in `config.json`

No code changes needed. That single input turns the seed provider into live
market research and the offline estimator into measured competitor prices.

**Verify after:** `scan --api-key ...` should report `source: etsy_api`, and
`optimize` rows should show `source: etsy_api` with a non-zero sample size.

### 2. Real unit costs

`cost_ratio` (default 0.45) guesses unit cost as a share of the competitor
median. That makes the margin *floor* a guess. If you know real costs, the
`listings.unit_cost` column is already wired: set it and pricing uses it instead
of the heuristic.

## Not started

### 3. Publishing to Etsy

The tool optimizes and stores listings locally; it does not submit them. That
needs OAuth2 (not just an API key) and the `createDraftListing` endpoint. Scope
this deliberately — it's a different auth model from everything built so far.

### 4. Real daemon

`watch` runs in the foreground and dies with the terminal. For actual unattended
operation, wrap it in a launchd plist or just run it under `nohup`/`tmux`.

### 5. Model quality

`phi3:mini` writes usable copy but over-generates tags (15–40 where 13 are
allowed) and repeats the same phrase reordered. The repair layer handles it
correctly — every listing is compliant — but a stronger model
(`llama3.2:3b`, `qwen2.5:7b`) would need less repair. Compare
`GenerateReport.warnings` across models; fewer warnings per listing = better.

## Design invariants — keep these

- Idempotent: products upsert on `name`, listings on `product_id`. Re-running
  anything must stay a no-op.
- Never silently fall back. If the LLM or the API is unavailable, the run says so.
- Never ship a listing that violates marketplace limits.
- Deterministic where possible (fixed seeds, offline estimator).
- No new dependencies without a reason.
