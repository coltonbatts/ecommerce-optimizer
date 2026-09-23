# Next steps

Current state of the project. This file is what's actually left — not what was
already done.

## Working end to end

- `scan` — live Etsy v3 market data (API key) with a deterministic offline seed
  provider as fallback. Reports which one ran.
- `generate` — listings written by a local Ollama model, ~35s for 8 listings at
  concurrency 3. Template fallback when the daemon is down.
- `optimize` — competitor pricing from live Etsy results, with a fee-aware
  margin floor.
- `watch`, `status`, `doctor`, `--json`, `--dry-run`, `--limit`, `--force`.
- 20 unit tests, 0 clippy warnings.

## Known limitations

### Model quality

`phi3:mini` writes usable copy but over-generates tags (14–40 where 13 are
allowed) and repeats phrases. The repair layer handles all of it — every listing
ships compliant — but a stronger model needs less repair.

`GenerateReport.warnings` is the quality metric: fewer warnings per listing is
better. Compare models with:

```bash
cargo run --release --bin ecommerce-optimizer -- generate --force --model llama3.2:3b
```

Known residual artifact: occasional junk short tags from the model (e.g. `uni`
where it meant `unisex`). 1 in ~100 tags.

### Unit costs are estimated

The margin floor is only as good as the cost basis. By default it assumes unit
cost is 45% of the competitor median (`cost_ratio`). If you know real blank +
print costs, set `listings.unit_cost` and pricing uses it instead.

### Validation is inferred, not measured

Etsy validates price against a taxonomy `price` object and per-listing
`who_made` / `when_made` / `is_supply` fields. Those rules are not currently
modelled, so a listing that passes local checks may still be rejected by the
real `createDraftListing` call.

### Nothing is published

The tool optimizes and stores listings in local SQLite. It does not submit them
to Etsy. Publishing requires OAuth 2.0 (PKCE), which is a different auth model
from the API key — see `references/publishing.md`.

## Design invariants — keep these

- **Idempotent.** Products upsert on `name`, listings on `product_id`. Re-running
  any command must stay a no-op. Running `optimize` twice produces identical
  prices.
- **Never silently fall back.** If the LLM or the API is unavailable, the run
  says so and records why in the decision notes.
- **Never ship a listing that violates marketplace limits.** Title ≤140 chars,
  exactly 13 tags, each ≤20 chars — enforced before anything is written.
- **Never put display text into a query string.** Queries come from
  `products.search_term`, not from a formatted name.
- **Deterministic where possible.** Fixed per-product seeds, offline estimator,
  no wall-clock dependence in pricing.
- **No new dependencies without a reason.** Concurrency uses
  `tokio::task::JoinSet`; nothing was added for it.
