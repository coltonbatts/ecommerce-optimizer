# ecommerce-optimizer

Local-first e-commerce listing optimization bot. Scans market opportunities, writes
optimized listings with a **local** LLM, and prices them with fee-aware margin
targeting.

No accounts. No subscriptions. No cloud. The network is optional, not required.

## What actually works

| Stage | Status | Notes |
|---|---|---|
| `scan` | Real | Etsy v3 API when a key is set, curated seed provider otherwise |
| `generate` | Real | Ollama local LLM, template fallback when the daemon is down |
| `optimize` | Real | Fee-aware margin floor + competitor positioning |
| `watch` | Real | Interval scheduling, Ctrl-C clean shutdown |
| Persistence | Real | SQLite, idempotent upserts on natural keys |

Every listing produced has been checked against the marketplace's hard limits
(title ≤140 chars, exactly 13 tags, each ≤20 chars) *before* it is written to
disk.

## Quick start

```bash
cargo build --release

# Have a look at your environment first
cargo run --release --bin ecommerce-optimizer -- doctor

# Scan, generate, and price in one go
cargo run --release --bin ecommerce-optimizer -- all
```

Bare `cargo run` also runs the full cycle.

## Commands

```
scan                                    Find product opportunities, store them
generate [--force] [--limit N]          Write listings (only missing ones by default)
optimize [--dry-run]                    Re-price against competitors + margin floor
all [--dry-run]                         scan -> generate -> optimize
status                                  Database contents + recent pricing decisions
watch [--interval SECS] [--iterations N] Run the cycle on a schedule
doctor                                  Check Ollama, model, API key, margin math
config [--write]                        Show or write config.json
```

Global flags: `--config PATH` `--db PATH` `--marketplace` `--categories`
`--margin` `--api-key` `--model` `--ollama-url` `--concurrency` `--json`

Everything is non-interactive and scriptable. `--json` gives machine-readable
output for every command.

```bash
# Re-price only, write nothing, read the result as JSON
cargo run --release --bin ecommerce-optimizer -- optimize --dry-run --json

# Every 6 hours, forever
cargo run --release --bin ecommerce-optimizer -- watch --interval 21600
```

## Local LLM

Listing copy is written by Ollama on this machine. Nothing is uploaded.

```bash
ollama pull phi3:mini          # ~2.2 GB
cargo run --release --bin ecommerce-optimizer -- doctor
```

The generator uses Ollama's **JSON mode** and a fixed per-product seed, so output
is valid JSON and close to reproducible across runs. Model output is then
repaired to fit marketplace limits: over-length tags are clipped on a word or
hyphen boundary (never on a dangling connector like "pet portrait **from**"),
reordered duplicates are collapsed, and shortfalls are padded from a
category-specific pool so all 13 slots get used.

If Ollama isn't running, generation falls back to a deterministic template and
*says so* in the warnings. It never fails silently.

## Pricing

The first version priced at `cost * 1.3`, which ignores fees entirely. On Etsy
(6.5% transaction + 3% payment processing + $0.25 per order + $0.20 listing) a
"30% margin" priced that way actually nets about **10%**.

This version solves for the price that *actually* delivers the target margin:

```
net(price) = price * (1 - variable_rate) - fixed_fee
require:     net - cost >= margin * price
=> floor   = (cost + fixed_fee) / (1 - variable_rate - margin)
```

Then it positions inside the observed competitor envelope
(`1 + 0.20*demand - 0.25*competition`, capped at the top of the range) and rounds
to a charm price without ever crossing the floor. If the margin floor sits above
what the market pays, the run says so instead of quietly shipping a loss leader.

Fee models live in `Marketplace::fee_model()` in `src/config.rs`.

## Competitor monitoring

- **With an API key:** live Etsy search results, trimmed at the 10th/90th
  percentile so one absurd listing can't define the range.
- **Without one:** a deterministic offline estimator derived from the market
  median and competition score. Clearly labelled `offline_estimate` — it never
  pretends to be measured data.

Every decision is recorded in `pricing_decisions` with the reference, floor,
achieved margin, and an explanation, so any price can be audited after the fact.

### Etsy API key

The `x-api-key` header requires the `keystring:shared_secret` form — the bare
keystring returns HTTP 403.

```bash
cargo run --release --bin ecommerce-optimizer -- scan --api-key "keystring:shared_secret"
# or put it in config.json (gitignored)
```

## Configuration

`config --write` creates `config.json`, which is gitignored because it holds the
API key. CLI flags override file values, which override defaults.

Notable settings: `target_margin`, `cost_ratio` (share of the competitor median
assumed to be your unit cost), `concurrency`, `watch_interval_secs`.

`data/seeds.json` optionally overrides the built-in seed product list so you can
tune your own market model without recompiling.

## Tests

```bash
cargo test
```

Covers the marketplace limit enforcement, the tag-repair rules, the fee-aware
margin floor (asserting it beats the naive multiplier), charm pricing, and
offline estimator determinism.

## Design principles

Local-first · offline-capable · minimal dependencies · deterministic · CLI-first
· idempotent.

Idempotency is real, not aspirational: products upsert on `name` and listings on
`product_id`, so re-running any command is a no-op rather than a source of
duplicates. Running `optimize` twice produces byte-identical prices.

## Layout

```
src/
├── main.rs      CLI (clap), output formatting, `doctor`
├── lib.rs       Optimizer orchestration + report types
├── config.rs    Config, marketplace fee models
├── database.rs  SQLite schema, migrations, idempotent CRUD
├── scanner.rs   Etsy v3 provider + deterministic seed provider
├── listing.rs   Ollama JSON-mode generation, repair layer, template fallback
└── pricing.rs   Fee-aware margin floor, charm pricing, competitor stats
```

Dependencies: `tokio` `reqwest` `serde` `serde_json` `rusqlite` `chrono` `clap`.
No new ones were added for this work — concurrency uses `tokio::task::JoinSet`.
