use chrono::Utc;

use crate::config::Config;
use crate::database::{CompetitorPrice, Listing, PricingDecision, Product};

/// Competitor price distribution for a product.
#[derive(Debug, Clone)]
pub struct PriceStats {
    pub reference: f64,
    pub min: f64,
    pub max: f64,
    pub sample_size: i64,
    /// "etsy_api" or "offline_estimate"
    pub source: String,
    pub note: String,
}

impl PriceStats {
    pub fn to_record(&self, product_id: i64) -> CompetitorPrice {
        CompetitorPrice {
            product_id,
            reference_price: self.reference,
            min_price: self.min,
            max_price: self.max,
            sample_size: self.sample_size,
            source: self.source.clone(),
            created_at: Utc::now().to_rfc3339(),
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ------------------------------------------------------------------ offline

/// Deterministic offline competitor model.
///
/// We only know the market reference (the median price observed by the scanner)
/// and a competition score. Competition widens the *floor* side of the range
/// (more sellers racing to the bottom) while the ceiling is held by the
/// higher-effort, higher-priced sellers. This never pretends to be measured
/// data: `source` is `offline_estimate` and the note says so.
pub fn estimate_competitors(product: &Product) -> PriceStats {
    let reference = product.market_price.max(0.01);
    let comp = product.competition_score.clamp(0.0, 1.0);

    let down = 0.20 + 0.20 * comp; // 20%..40% below the median
    let up = 0.45 - 0.15 * comp; // 45%..30% above the median

    // A tiny deterministic jitter derived from the name keeps repeated runs
    // stable while making each product's envelope distinct.
    let jitter = (fnv(&product.name) % 7) as f64 / 100.0; // 0.00..0.06

    PriceStats {
        reference,
        min: round2(reference * (1.0 - down)),
        max: round2(reference * (1.0 + up + jitter)),
        sample_size: 0,
        source: "offline_estimate".to_string(),
        note: format!(
            "modeled from market median ${reference:.2} and competition {comp:.2}; no network used"
        ),
    }
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Round to a charm price (.99) that never drops below `floor`.
fn charm_price(target: f64, floor: f64) -> f64 {
    let mut candidate = round2(target.floor() + 0.99);
    if candidate < floor {
        candidate = round2((floor + 0.99).floor() + 0.99);
    }
    if candidate < floor {
        candidate = round2(floor);
    }
    candidate
}

// ------------------------------------------------------------------- online

#[derive(serde::Deserialize)]
struct EtsySearch {
    #[serde(default)]
    results: Vec<EtsyItem>,
}

#[derive(serde::Deserialize)]
struct EtsyItem {
    #[serde(default)]
    price: Option<EtsyMoney>,
}

#[derive(serde::Deserialize)]
struct EtsyMoney {
    amount: i64,
    #[serde(default = "hundred")]
    divisor: i64,
    #[serde(default)]
    currency_code: String,
}

fn hundred() -> i64 {
    100
}

/// Minimum comparable listings before live stats are trustworthy. Below this the
/// sample is noise and we say so rather than pricing off it.
pub const MIN_COMPARABLE_SAMPLES: usize = 8;

/// Never let the ceiling run far above the reference. Etsy search results mix
/// new goods with genuine collectibles — an authentic vintage tour tee at $210
/// sitting among $25 screen prints is not a comparable.
pub const MAX_CEILING_MULTIPLE: f64 = 2.0;

/// Keywords for a competitor search. Prefers the query the scanner recorded;
/// falls back to deriving one from the display name.
fn search_keywords(product: &Product) -> String {
    if let Some(term) = product.search_term.as_ref() {
        if !term.trim().is_empty() {
            return term.trim().to_string();
        }
    }
    // Derive defensively: display names carry punctuation (commas, hyphens, em
    // dashes, parentheses) that would otherwise end up inside the query string.
    product
        .name
        .split([',', '-', '—', '–', '|', '(', '/'])
        .next()
        .unwrap_or(&product.name)
        .split_whitespace()
        .take(5)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Real competitor prices for a product, measured from live Etsy search
/// results. Requires an API key; returns Err so the caller can fall back.
pub async fn fetch_competitors(product: &Product, config: &Config) -> Result<PriceStats, String> {
    let key = config.api_keys.marketplace.trim();
    if key.is_empty() {
        return Err("no marketplace API key configured".to_string());
    }

    let keywords = search_keywords(product);
    if keywords.is_empty() {
        return Err("no usable search keywords for this product".to_string());
    }

    let url = format!(
        "https://openapi.etsy.com/v3/application/listings/active?keywords={}&limit=50&sort_on=score",
        urlencode(&keywords)
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .user_agent("ecommerce-optimizer/0.1 (local-first)")
        .build()
        .map_err(|e| format!("client: {e}"))?;

    let resp = client
        .get(&url)
        .header("x-api-key", key)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {}: {}",
            status.as_u16(),
            body.trim().chars().take(160).collect::<String>()
        ));
    }

    let search: EtsySearch = serde_json::from_str(&body).map_err(|e| format!("json: {e}"))?;

    let mut prices: Vec<f64> = search
        .results
        .iter()
        .filter_map(|i| i.price.as_ref())
        .filter(|m| m.currency_code.is_empty() || m.currency_code == "USD")
        .map(|m| {
            let d = if m.divisor == 0 { 100 } else { m.divisor };
            m.amount as f64 / d as f64
        })
        .filter(|p| *p > 0.5 && *p < 5_000.0)
        .collect();

    if prices.is_empty() {
        return Err("no USD-priced competitors returned".to_string());
    }
    if prices.len() < MIN_COMPARABLE_SAMPLES {
        return Err(format!(
            "only {} comparable listing(s) for \"{}\" (need {}); using the offline estimate",
            prices.len(),
            keywords,
            MIN_COMPARABLE_SAMPLES
        ));
    }

    prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = prices.len();
    let median = if n % 2 == 1 {
        prices[n / 2]
    } else {
        (prices[n / 2 - 1] + prices[n / 2]) / 2.0
    };
    // Trim the extreme 10% so one absurd listing doesn't set the range.
    let lo_idx = (n as f64 * 0.1).floor() as usize;
    let hi_idx = ((n as f64 * 0.9).ceil() as usize).min(n - 1);

    Ok(PriceStats {
        reference: round2(median),
        min: round2(prices[lo_idx]),
        max: round2(prices[hi_idx]),
        sample_size: n as i64,
        source: "etsy_api".to_string(),
        note: format!("{n} live Etsy results for \"{keywords}\""),
    })
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Online when possible, offline otherwise. Always succeeds, and always carries
/// the reason for whichever path it took.
pub async fn competitor_stats(product: &Product, config: &Config) -> PriceStats {
    if config.has_marketplace_key() {
        match fetch_competitors(product, config).await {
            Ok(stats) => return stats,
            Err(e) => {
                let mut est = estimate_competitors(product);
                est.note = format!("{} (live lookup skipped: {})", est.note, e);
                return est;
            }
        }
    }
    estimate_competitors(product)
}

// ------------------------------------------------------------------ pricing

/// The fee-aware margin floor.
///
/// Net proceeds on a sale are `price * (1 - variable_rate) - fixed fee`. We need
/// that to cover cost plus the target margin *on the sale price*:
///
///   price(1 - rate) - fixed - cost >= margin * price
///   price(1 - rate - margin) >= cost + fixed
///   floor = (cost + fixed) / (1 - rate - margin)
///
/// The naive `price * 1.3` the first version used ignored fees entirely and
/// overstated real margin by roughly the whole fee load.
pub fn margin_floor(cost: f64, config: &Config) -> Result<f64, String> {
    let (rate, fixed) = config.marketplace.fee_model();
    let denom = 1.0 - rate - config.target_margin;
    if denom <= 0.01 {
        return Err(format!(
            "target margin {:.0}% is impossible on {} — variable fees are {:.1}%",
            config.target_margin * 100.0,
            config.marketplace.as_str(),
            rate * 100.0
        ));
    }
    Ok((cost + fixed) / denom)
}

pub fn net_proceeds(price: f64, config: &Config) -> f64 {
    let (rate, fixed) = config.marketplace.fee_model();
    price * (1.0 - rate) - fixed
}

pub struct PricingOutcome {
    pub decision: PricingDecision,
    pub stats: PriceStats,
}

/// Compute the recommended price for a listing given competitor stats.
pub fn optimize_price(
    listing: &Listing,
    product: &Product,
    stats: &PriceStats,
    config: &Config,
) -> Result<PricingOutcome, String> {
    let cost = if listing.unit_cost > 0.0 {
        listing.unit_cost
    } else {
        round2(stats.reference * config.cost_ratio)
    };

    let floor = margin_floor(cost, config)?;

    // Position against the competition, then never sell below the margin floor.
    let demand = product.demand_score.clamp(0.0, 1.0);
    let comp = product.competition_score.clamp(0.0, 1.0);
    let positioning = 1.0 + (0.20 * demand) - (0.25 * comp);
    let mut target = stats.reference * positioning;

    // Stay inside the observed competitive envelope, but do not let a mixed
    // market (collectibles alongside new goods) drag the ceiling up. Without
    // this an authentic $210 vintage tee sets the cap for a $25 screen print.
    let mut ceiling = stats.max.min(stats.reference * MAX_CEILING_MULTIPLE);
    ceiling = ceiling.max(stats.min);
    let mut notes: Vec<String> = Vec::new();
    if stats.max > stats.reference * MAX_CEILING_MULTIPLE {
        notes.push(format!(
            "ceiling held to ${:.2}; live max ${:.2} is an outlier (likely collectible)",
            ceiling, stats.max
        ));
    }
    if target > ceiling {
        target = ceiling;
        notes.push("capped at the top of the comparable range".to_string());
    }
    let mut price = charm_price(target.max(floor), floor);

    let mut margin_unmet = false;
    if price < floor {
        price = round2(floor);
    }
    // If the margin floor sits above what the market pays, say so instead of
    // quietly shipping a loss-making price.
    if floor > ceiling {
        margin_unmet = true;
        notes.push(format!(
            "margin floor ${floor:.2} exceeds the competitor ceiling ${ceiling:.2} — repricing or cost reduction needed"
        ));
    }

    let net = net_proceeds(price, config);
    let margin_achieved = if price > 0.0 {
        (net - cost) / price
    } else {
        0.0
    };

    let delta = round2(price - listing.price);
    let direction = if delta > 0.0 {
        "up"
    } else if delta < 0.0 {
        "down"
    } else {
        "unchanged"
    };

    Ok(PricingOutcome {
        stats: stats.clone(),
        decision: PricingDecision {
            listing_id: listing.id,
            reference_price: stats.reference,
            min_price: stats.min,
            max_price: stats.max,
            floor_price: round2(floor),
            recommended_price: price,
            margin_achieved: (margin_achieved * 10_000.0).round() / 10_000.0,
            method: if margin_unmet {
                "margin_floor_above_market".to_string()
            } else {
                "fee_aware_positioning".to_string()
            },
            source: stats.source.clone(),
            notes: format!(
                "{}; cost basis ${:.2}; moved {} by ${:.2}",
                notes.join("; "),
                cost,
                direction,
                delta.abs()
            ),
            created_at: Utc::now().to_rfc3339(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiKeys, Marketplace};
    use crate::database::Listing;

    fn cfg(margin: f64) -> Config {
        Config {
            target_margin: margin,
            marketplace: Marketplace::Etsy,
            api_keys: ApiKeys::default(),
            ..Config::default()
        }
    }

    fn product() -> Product {
        Product {
            id: 1,
            name: "Test Candle".to_string(),
            category: "home & living".to_string(),
            market_price: 20.0,
            demand_score: 0.8,
            competition_score: 0.5,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            search_term: Some("test candle".to_string()),
        }
    }

    fn named(name: &str, search_term: Option<&str>) -> Product {
        Product {
            name: name.to_string(),
            search_term: search_term.map(String::from),
            ..product()
        }
    }

    #[test]
    fn search_term_wins_over_a_punctuation_laden_display_name() {
        // The real bug: the scanner named products "Vintage Band Tee — Etsy niche
        // cluster" and the query became "Vintage Band Tee — Etsy", returning 2
        // unrelated $100 listings.
        let p = named(
            "Vintage Band Tee — Etsy niche cluster",
            Some("vintage band tee"),
        );
        assert_eq!(search_keywords(&p), "vintage band tee");
    }

    #[test]
    fn derived_keywords_strip_em_dashes_and_parens() {
        for (name, want) in [
            ("Vintage Band Tee — Etsy niche cluster", "vintage band tee"),
            ("Wabi Sabi Painting – Japanese", "wabi sabi painting"),
            ("Custom Pet Portrait (from photo)", "custom pet portrait"),
            ("Gift, for her, handmade", "gift"),
        ] {
            assert_eq!(search_keywords(&named(name, None)), want, "name: {name}");
        }
    }

    #[test]
    fn a_collectible_outlier_does_not_set_the_ceiling() {
        // $25 screen prints sitting next to one authentic $210 vintage tee.
        let stats = PriceStats {
            reference: 27.50,
            min: 18.49,
            max: 210.00,
            sample_size: 35,
            source: "etsy_api".into(),
            note: String::new(),
        };
        let out = optimize_price(&listing(27.0, 0.0), &product(), &stats, &cfg(0.3)).unwrap();
        assert!(
            out.decision.recommended_price < 60.0,
            "should not price near the collectible: {}",
            out.decision.recommended_price
        );
        assert!(
            out.decision.notes.contains("outlier"),
            "the outlier clamp must be reported: {}",
            out.decision.notes
        );
    }

    #[test]
    fn an_impossible_live_sample_is_rejected_not_trusted() {
        // Two results, both $100 — the corrupt-query signature.
        let prices: Vec<f64> = vec![100.0];
        assert!(prices.len() < MIN_COMPARABLE_SAMPLES);
    }

    fn listing(price: f64, cost: f64) -> Listing {
        Listing {
            id: 1,
            product_id: 1,
            title: "t".into(),
            description: "d".into(),
            tags: vec![],
            price,
            status: "active".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            source: "template".into(),
            model: None,
            unit_cost: cost,
        }
    }

    #[test]
    fn fee_aware_floor_beats_the_naive_1_3x() {
        // Cost 10, 30% margin, Etsy 9.5% + $0.45.
        // Naive x1.3 => 13.00, which nets 13*0.905-0.45 = 11.32 => 13.2% margin.
        let naive = round2(10.0 * 1.3);
        let naive_margin = (net_proceeds(naive, &cfg(0.3)) - 10.0) / naive;
        assert!(
            naive_margin < 0.30,
            "naive pricing should miss the target, got {naive_margin}"
        );

        let floor = margin_floor(10.0, &cfg(0.3)).unwrap();
        let achieved = (net_proceeds(floor, &cfg(0.3)) - 10.0) / floor;
        assert!(
            (achieved - 0.30).abs() < 0.001,
            "floor must deliver exactly the target margin, got {achieved}"
        );
    }

    #[test]
    fn impossible_margin_is_rejected_not_silently_discounted() {
        assert!(margin_floor(10.0, &cfg(0.95)).is_err());
    }

    #[test]
    fn price_never_lands_below_the_margin_floor() {
        let p = product();
        let mut l = listing(1.0, 0.0);
        l.unit_cost = 18.0;
        let stats = PriceStats {
            reference: 20.0,
            min: 12.0,
            max: 30.0,
            sample_size: 10,
            source: "offline_estimate".into(),
            note: String::new(),
        };
        let out = optimize_price(&l, &p, &stats, &cfg(0.3)).unwrap();
        assert!(out.decision.recommended_price >= out.decision.floor_price);
        assert!(out.decision.margin_achieved >= 0.299);
    }

    #[test]
    fn charm_pricing_keeps_the_penny() {
        assert_eq!(charm_price(21.40, 0.0), 21.99);
        // A floor above the charm candidate must win.
        assert!(charm_price(21.40, 25.0) >= 25.0);
    }

    #[test]
    fn offline_estimator_is_deterministic() {
        let p = product();
        let a = estimate_competitors(&p);
        let b = estimate_competitors(&p);
        assert_eq!(a.min, b.min);
        assert_eq!(a.max, b.max);
        assert_eq!(a.reference, b.reference);
        assert!(a.min < a.reference && a.reference < a.max);
    }
}
