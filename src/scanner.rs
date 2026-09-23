use chrono::Utc;
use serde::Deserialize;
use std::path::Path;

use crate::config::Config;
use crate::database::Product;

/// Where the product opportunities came from. Reported to the user so a run
/// never silently pretends it used the network when it didn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanSource {
    EtsyApi,
    Seed,
}

impl ScanSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScanSource::EtsyApi => "etsy_api",
            ScanSource::Seed => "seed",
        }
    }
}

#[derive(Debug)]
pub struct ScanOutcome {
    pub products: Vec<Product>,
    pub source: ScanSource,
    pub notes: Vec<String>,
}

/// A niche to probe on the marketplace. Each keyword becomes one product
/// opportunity, priced from the median of the real listings returned for it.
const SEARCH_TERMS: &[(&str, &str)] = &[
    // Apparel — the category BattsBespoke actually sells in.
    ("clothing", "retro graphic tee"),
    ("clothing", "film inspired t shirt"),
    ("clothing", "funny saying t shirt"),
    ("clothing", "vintage band tee"),
    ("clothing", "horror movie t shirt"),
    ("clothing", "minimalist line art tee"),
    ("clothing", "unisex cotton tee"),
    ("clothing", "cult classic movie shirt"),
    ("home & living", "espresso martini candle"),
    ("home & living", "cottagecore wall art"),
    ("home & living", "mushroom lamp"),
    ("home & living", "japandi linen pillow"),
    ("home & living", "wabi sabi abstract painting"),
    ("home & living", "dark academia art print"),
    ("jewelry", "custom nameplate bracelet"),
    ("jewelry", "charm statement necklace"),
    ("jewelry", "personalized leather keyring"),
    ("jewelry", "birth flower ring"),
    ("jewelry", "moonstone wire wrapped necklace"),
    ("art & collectibles", "custom pet portrait"),
    ("art & collectibles", "funny animal art print"),
    ("art & collectibles", "mini oil painting wildlife"),
    ("art & collectibles", "hand carved wood figurine"),
    ("art & collectibles", "pub bar art print"),
];

// ---------------------------------------------------------------- Etsy API

#[derive(Debug, Deserialize)]
struct EtsySearchResponse {
    #[serde(default)]
    count: u64,
    #[serde(default)]
    results: Vec<EtsyListing>,
}

#[derive(Debug, Deserialize)]
struct EtsyListing {
    #[serde(default)]
    title: String,
    #[serde(default)]
    price: Option<EtsyPrice>,
    #[serde(default)]
    num_favorers: Option<u64>,
}

impl EtsyListing {
    /// The top search result is the benchmark a seller has to beat — surface it
    /// in the scan notes so the ranking is auditable rather than a black box.
    fn short_title(&self) -> String {
        self.title.chars().take(70).collect()
    }
}

#[derive(Debug, Deserialize)]
struct EtsyPrice {
    amount: i64,
    #[serde(default = "default_divisor")]
    divisor: i64,
    #[serde(default)]
    currency_code: String,
}

fn default_divisor() -> i64 {
    100
}

impl EtsyPrice {
    fn as_f64(&self) -> f64 {
        let d = if self.divisor == 0 { 100 } else { self.divisor };
        self.amount as f64 / d as f64
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Normalizes a raw signal to 0..1 with a soft logarithm, so one viral listing
/// cannot saturate the demand score.
fn squash(value: f64, ceiling: f64) -> f64 {
    if value <= 0.0 {
        return 0.0;
    }
    let raw = (1.0 + value).ln() / (1.0 + ceiling).ln();
    raw.clamp(0.0, 1.0)
}

async fn etsy_search(
    client: &reqwest::Client,
    api_key: &str,
    keywords: &str,
    limit: u32,
) -> Result<EtsySearchResponse, String> {
    let url = format!(
        "https://openapi.etsy.com/v3/application/listings/active?keywords={}&limit={}&sort_on=score",
        urlencode(keywords),
        limit
    );
    let resp = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;

    if !status.is_success() {
        return Err(format!(
            "HTTP {} — {}",
            status.as_u16(),
            body.trim().chars().take(200).collect::<String>()
        ));
    }

    serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))
}

/// Minimal percent-encoding for query values. Keeps the dependency list lean.
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

async fn scan_etsy(config: &Config) -> Result<ScanOutcome, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .user_agent("ecommerce-optimizer/0.1 (local-first)")
        .build()
        .map_err(|e| format!("client build: {e}"))?;

    let key = config.api_keys.marketplace.trim().to_string();
    let mut products = Vec::new();
    let mut notes = Vec::new();
    let mut failures = 0usize;

    for (category, term) in SEARCH_TERMS {
        if !config.categories.iter().any(|c| c == category) {
            continue;
        }

        match etsy_search(&client, &key, term, 24).await {
            Ok(resp) => {
                let prices: Vec<f64> = resp
                    .results
                    .iter()
                    .filter_map(|l| l.price.as_ref())
                    .filter(|p| p.currency_code.is_empty() || p.currency_code == "USD")
                    .map(|p| p.as_f64())
                    .filter(|p| *p > 0.5 && *p < 5_000.0)
                    .collect();

                if prices.is_empty() {
                    failures += 1;
                    continue;
                }

                let reference = median(prices.clone());
                let favorers: f64 = resp
                    .results
                    .iter()
                    .filter_map(|l| l.num_favorers)
                    .map(|f| f as f64)
                    .sum();

                // demand: how much the market actually wants this niche
                let demand = squash(favorers / prices.len() as f64, 2_000.0);
                // competition: how crowded the niche is
                let competition = squash(resp.count as f64, 60_000.0);

                if let Some(top) = resp.results.first() {
                    if !top.title.is_empty() {
                        notes.push(format!(
                            "\"{}\" top result: \"{}\"",
                            term,
                            top.short_title()
                        ));
                    }
                }

                products.push(Product {
                    id: 0,
                    name: format!("{} — Etsy niche cluster", titlecase(term)),
                    category: category.to_string(),
                    market_price: (reference * 100.0).round() / 100.0,
                    demand_score: (demand * 100.0).round() / 100.0,
                    competition_score: (competition * 100.0).round() / 100.0,
                    created_at: Utc::now().to_rfc3339(),
                });
            }
            Err(e) => {
                failures += 1;
                notes.push(format!("search \"{term}\" failed: {e}"));
                // A rejected key fails every term identically — stop early.
                if failures == 1 && e.contains("403") {
                    return Err(format!(
                        "Etsy rejected the API key. Expected format is \
                         'keystring:shared_secret'. First error: {e}"
                    ));
                }
            }
        }
    }

    if products.is_empty() {
        return Err("Etsy returned no usable listings for the configured categories".to_string());
    }

    Ok(ScanOutcome {
        products,
        source: ScanSource::EtsyApi,
        notes,
    })
}

fn titlecase(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- key check

#[derive(Debug, Deserialize)]
struct PingResponse {
    #[serde(default)]
    application_id: i64,
}

/// Verify a marketplace API key against Etsy's ping endpoint. No OAuth needed —
/// this is the cheapest way to answer "is my key actually live?".
pub async fn verify_api_key(config: &Config) -> Result<i64, String> {
    let key = config.api_keys.marketplace.trim().to_string();
    if key.is_empty() {
        return Err("no API key set".to_string());
    }
    if !key.contains(':') {
        return Err(
            "malformed: Etsy requires 'keystring:shared_secret', not the bare keystring"
                .to_string(),
        );
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.http_timeout_secs))
        .user_agent("ecommerce-optimizer/0.1 (local-first)")
        .build()
        .map_err(|e| format!("client: {e}"))?;

    let resp = client
        .get("https://api.etsy.com/v3/application/openapi-ping")
        .header("x-api-key", &key)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // The two 403 bodies mean different things and the fix differs.
        let hint = if body.contains("not active") {
            " — your key is registered but NOT APPROVED YET. Check 'Your Apps' for active status."
        } else if body.contains("format") {
            " — the header format is wrong."
        } else {
            ""
        };
        return Err(format!(
            "HTTP {}: {}{}",
            status.as_u16(),
            body.trim().chars().take(200).collect::<String>(),
            hint
        ));
    }

    let ping: PingResponse = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
    Ok(ping.application_id)
}

// ---------------------------------------------------------------- seeds

/// Curated opportunities from Etsy bestseller/market research. This is the
/// offline path, and it is a first-class provider — not a placeholder. It is
/// deterministic, which means `scan` twice in a row is a genuine no-op.
fn seed_products() -> Vec<Product> {
    let now = || Utc::now().to_rfc3339();
    let p = |name: &str, cat: &str, price: f64, demand: f64, comp: f64| Product {
        id: 0,
        name: name.to_string(),
        category: cat.to_string(),
        market_price: price,
        demand_score: demand,
        competition_score: comp,
        created_at: now(),
    };

    vec![
        // Apparel — matches BattsBespoke's actual product line.
        p(
            "Retro Film Quote Graphic Tee, Unisex Cotton",
            "clothing",
            26.00,
            0.85,
            0.55,
        ),
        p(
            "Cult Classic Movie Inspired T Shirt, Minimalist",
            "clothing",
            24.00,
            0.80,
            0.50,
        ),
        p(
            "Funny Saying T Shirt, Sarcastic Humor Tee",
            "clothing",
            22.00,
            0.85,
            0.65,
        ),
        p(
            "Vintage Band Style Graphic Tee, Faded Print",
            "clothing",
            28.00,
            0.75,
            0.60,
        ),
        p(
            "Horror Movie Villain T Shirt, Retro Slasher",
            "clothing",
            25.00,
            0.80,
            0.55,
        ),
        p(
            "Minimalist Line Art Tee, Single Color Print",
            "clothing",
            23.00,
            0.70,
            0.45,
        ),
        p(
            "Vintage Gold Framed Butterfly Wall Art, Cottagecore Decor",
            "home & living",
            25.99,
            0.85,
            0.40,
        ),
        p(
            "Dark Academia Rabbit Framed Wall Art Print",
            "home & living",
            26.76,
            0.80,
            0.35,
        ),
        p(
            "Sage Green Textured Abstract Painting, Wabi Sabi",
            "home & living",
            78.64,
            0.75,
            0.50,
        ),
        p(
            "Espresso Martini Scented Candle, Foodie Home Decor",
            "home & living",
            17.00,
            0.90,
            0.60,
        ),
        p(
            "Mushroom Lamp, Whimsical Forest-Inspired Lighting",
            "home & living",
            35.00,
            0.70,
            0.45,
        ),
        p(
            "Japandi Linen Throw Pillow, Scandinavian Minimalist",
            "home & living",
            28.00,
            0.65,
            0.55,
        ),
        p(
            "Custom Nameplate Block Letter Bracelet, Personalized",
            "jewelry",
            28.00,
            0.85,
            0.70,
        ),
        p(
            "Colorful Charm Statement Necklace, Bohemian Layering",
            "jewelry",
            32.00,
            0.80,
            0.65,
        ),
        p(
            "Personalized Leather Photo Keyring for Her",
            "jewelry",
            22.00,
            0.75,
            0.55,
        ),
        p(
            "Birth Flower Ring, Sterling Silver Mother of Pearl",
            "jewelry",
            29.40,
            0.70,
            0.60,
        ),
        p(
            "Rainbow Moonstone Wire Wrapped Necklace, Copper",
            "jewelry",
            31.18,
            0.65,
            0.50,
        ),
        p(
            "Custom Pet Charcoal Portrait from Photo",
            "art & collectibles",
            27.45,
            0.90,
            0.40,
        ),
        p(
            "Funny Mouse Holding Toilet Paper Art Print",
            "art & collectibles",
            19.01,
            0.85,
            0.30,
        ),
        p(
            "Mini Heron Oil Painting, Wildlife Wall Art, Gold Frame",
            "art & collectibles",
            25.00,
            0.75,
            0.35,
        ),
        p(
            "Beaver Wooden Figurine, Hand Carved Forest Animal",
            "art & collectibles",
            10.99,
            0.80,
            0.25,
        ),
        p(
            "London Pub Bar Art Print, Restaurant Illustration",
            "art & collectibles",
            49.21,
            0.65,
            0.40,
        ),
    ]
}

/// An optional `data/seeds.json` overrides the built-in list, so the user can
/// tune their own market model without recompiling.
fn load_seed_override(path: &str) -> Option<Vec<Product>> {
    if !Path::new(path).exists() {
        return None;
    }
    #[derive(Deserialize)]
    struct Seed {
        name: String,
        category: String,
        market_price: f64,
        #[serde(default = "half")]
        demand_score: f64,
        #[serde(default = "half")]
        competition_score: f64,
    }
    fn half() -> f64 {
        0.5
    }

    let raw = std::fs::read_to_string(path).ok()?;
    let seeds: Vec<Seed> = serde_json::from_str(&raw).ok()?;
    Some(
        seeds
            .into_iter()
            .map(|s| Product {
                id: 0,
                name: s.name,
                category: s.category,
                market_price: s.market_price,
                demand_score: s.demand_score,
                competition_score: s.competition_score,
                created_at: Utc::now().to_rfc3339(),
            })
            .collect(),
    )
}

// ---------------------------------------------------------------- entry

pub async fn scan_trending(config: &Config, seeds_path: &str) -> Result<ScanOutcome, String> {
    if config.has_marketplace_key() {
        match scan_etsy(config).await {
            Ok(outcome) if !outcome.products.is_empty() => return Ok(outcome),
            Ok(_) => {}
            Err(e) => {
                // Fall back rather than fail: an offline-capable tool still has
                // to do useful work when the network or key is wrong.
                let products = filter_seeds(config, seeds_path);
                return Ok(ScanOutcome {
                    products,
                    source: ScanSource::Seed,
                    notes: vec![format!("Etsy API unusable, fell back to seed data: {}", e)],
                });
            }
        }
    }

    let notes = if config.has_marketplace_key() {
        vec![]
    } else {
        vec![
            "No Etsy API key configured (--api-key / config.json api_keys.marketplace); \
             using the offline seed provider."
                .to_string(),
        ]
    };

    Ok(ScanOutcome {
        products: filter_seeds(config, seeds_path),
        source: ScanSource::Seed,
        notes,
    })
}

fn filter_seeds(config: &Config, seeds_path: &str) -> Vec<Product> {
    let all = load_seed_override(seeds_path).unwrap_or_else(seed_products);
    all.into_iter()
        .filter(|p| config.categories.contains(&p.category))
        .collect()
}
