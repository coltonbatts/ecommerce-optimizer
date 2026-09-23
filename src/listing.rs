use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::database::{Listing, Product};

/// Hard marketplace limits. Etsy enforces all three; violating them means the
/// listing is rejected on submission, so we enforce them before it hits disk.
pub const TITLE_MAX: usize = 140;
pub const TAGS_MAX: usize = 13;
pub const TAG_LEN_MAX: usize = 20;

#[derive(Serialize)]
struct GenerateRequest {
    model: String,
    prompt: String,
    stream: bool,
    /// Ollama's JSON mode. This is the single biggest reliability win: it
    /// grammar-constrains the output to valid JSON instead of hoping the model
    /// complies with "return ONLY JSON".
    format: String,
    options: GenerateOptions,
}

#[derive(Serialize)]
struct GenerateOptions {
    temperature: f64,
    num_predict: i64,
    num_ctx: i64,
    /// Fixed seed per product => nearly reproducible copy across runs.
    seed: u64,
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    response: String,
}

/// Result of a generation attempt, including anything we had to repair.
#[derive(Debug)]
pub struct GenerationOutcome {
    pub listing: Listing,
    pub warnings: Vec<String>,
}

pub fn is_llm_backed(listing: &Listing) -> bool {
    listing.source == "llm"
}

// ------------------------------------------------------------ LLM path

/// Probe Ollama for reachability and available models.
pub async fn ollama_models(config: &Config) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Tags {
        #[serde(default)]
        models: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        #[serde(default)]
        name: String,
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("client: {e}"))?;

    let url = format!("{}/api/tags", config.ollama_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Ollama not reachable at {url}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Ollama returned HTTP {}", resp.status().as_u16()));
    }

    let tags: Tags = resp
        .json()
        .await
        .map_err(|e| format!("bad response: {e}"))?;
    Ok(tags.models.into_iter().map(|m| m.name).collect())
}

fn prompt_for(product: &Product, repair_hint: Option<&str>) -> String {
    let base = format!(
        r#"You are an Etsy listing copywriter. Write ONE listing for the product below.

PRODUCT: {name}
CATEGORY: {category}
MARKET REFERENCE PRICE: ${price:.2}

Return a single JSON object with exactly these keys:

"title": string. Max {title_max} characters. Front-load the highest-intent search keywords. No ALL CAPS, no emoji, no shop name.
"description": string. 2 short paragraphs. Lead with the benefit, then materials/details. Include keywords naturally. DO NOT mention any price, discount, or shipping cost.
"tags": array of exactly {tags_max} strings. Each tag max {tag_len} characters, lowercase, single words or short 2-word phrases. No duplicates. No '#' symbols.

Output JSON only."#,
        name = product.name,
        category = product.category,
        price = product.market_price,
        title_max = TITLE_MAX,
        tags_max = TAGS_MAX,
        tag_len = TAG_LEN_MAX,
    );

    match repair_hint {
        Some(hint) => {
            format!("{base}\n\nYour previous attempt was rejected. Fix this and try again: {hint}")
        }
        None => base,
    }
}

fn stable_seed(name: &str) -> u64 {
    // FNV-1a, so the same product always gets the same seed.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Ask the local model for a listing. Returns Err with a human-readable reason
/// so the caller can fall back honestly and report why.
pub async fn generate_with_llm(
    product: &Product,
    config: &Config,
) -> Result<GenerationOutcome, String> {
    let model = config.llm_model();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.llm_timeout_secs))
        .build()
        .map_err(|e| format!("client: {e}"))?;

    let url = format!("{}/api/generate", config.ollama_url.trim_end_matches('/'));
    let seed = stable_seed(&product.name);

    let mut warnings: Vec<String> = Vec::new();
    let mut repair_hint: Option<String> = None;

    // Two attempts: the second one tells the model exactly what was wrong.
    for attempt in 0..2 {
        let request = GenerateRequest {
            model: model.clone(),
            prompt: prompt_for(product, repair_hint.as_deref()),
            stream: false,
            format: "json".to_string(),
            options: GenerateOptions {
                temperature: if attempt == 0 { 0.4 } else { 0.1 },
                num_predict: 900,
                num_ctx: 4096,
                seed: seed.wrapping_add(attempt as u64),
            },
        };

        let body = serde_json::to_string(&request).map_err(|e| format!("serialize: {e}"))?;

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("Ollama request failed ({url}): {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Ollama HTTP {} for model '{}': {}",
                status.as_u16(),
                model,
                text.trim().chars().take(200).collect::<String>()
            ));
        }

        let parsed: GenerateResponse = resp
            .json()
            .await
            .map_err(|e| format!("decode Ollama response: {e}"))?;

        match extract_listing(&parsed.response, product, &model) {
            Ok((listing, mut w)) => {
                warnings.append(&mut w);
                if attempt > 0 {
                    warnings.push(format!("recovered on attempt {}", attempt + 1));
                }
                return Ok(GenerationOutcome { listing, warnings });
            }
            Err(reason) => {
                repair_hint = Some(reason);
            }
        }
    }

    Err(format!(
        "model returned unusable copy twice ({})",
        repair_hint.unwrap_or_else(|| "unknown".to_string())
    ))
}

/// Pull the JSON object out of the model's reply, then clamp every field to the
/// marketplace's real limits.
fn extract_listing(
    raw: &str,
    product: &Product,
    model: &str,
) -> Result<(Listing, Vec<String>), String> {
    let mut warnings = Vec::new();

    let json_str = extract_json_object(raw).ok_or("no JSON object in response")?;
    let value: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| format!("invalid JSON: {e}"))?;

    let title_raw = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if title_raw.is_empty() {
        return Err("title was empty".to_string());
    }

    let desc_raw = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if desc_raw.split_whitespace().count() < 15 {
        return Err("description was too short to be a real listing".to_string());
    }

    let raw_tags: Vec<String> = value
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if raw_tags.is_empty() {
        return Err("tags were empty".to_string());
    }

    // --- repair, and say what was repaired ---
    let title_clean = remove_price_tokens(&title_raw);
    if title_clean.len() != title_raw.len() {
        warnings.push("removed price from title".to_string());
    }
    let title = clamp_title(&title_clean);
    if title.len() != title_clean.len() {
        warnings.push(format!(
            "title trimmed {} -> {} chars",
            title_clean.chars().count(),
            title.chars().count()
        ));
    }

    let description = strip_price_mentions(&desc_raw);
    if description.len() != desc_raw.len() {
        warnings.push("removed price mention from description".to_string());
    }

    let (mut tags, mut tag_warnings) = normalize_tags(&raw_tags, &product.category);
    warnings.append(&mut tag_warnings);
    // Keep exactly TAGS_MAX: Etsy silently truncates otherwise.
    if tags.len() > TAGS_MAX {
        warnings.push(format!("tags trimmed {} -> {}", tags.len(), TAGS_MAX));
        tags.truncate(TAGS_MAX);
    }
    pad_tags(&mut tags, &product.category, &mut warnings);

    if tags.len() != TAGS_MAX {
        return Err(format!("only {} usable tags after repair", tags.len()));
    }

    Ok((
        Listing {
            id: 0,
            product_id: product.id,
            title,
            description,
            tags,
            price: product.market_price,
            status: "active".to_string(),
            created_at: Utc::now().to_rfc3339(),
            source: "llm".to_string(),
            model: Some(model.to_string()),
            unit_cost: 0.0,
        },
        warnings,
    ))
}

/// Tolerates fenced blocks and leading prose despite JSON mode.
fn extract_json_object(raw: &str) -> Option<String> {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        if v.is_object() {
            return Some(t.to_string());
        }
    }
    let stripped = t
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) {
        if v.is_object() {
            return Some(stripped.to_string());
        }
    }
    let start = t.find('{')?;
    let end = t.rfind('}')?;
    if end <= start {
        return None;
    }
    let slice = &t[start..=end];
    serde_json::from_str::<serde_json::Value>(slice)
        .ok()
        .filter(|v| v.is_object())
        .map(|_| slice.to_string())
}

fn clamp_title(raw: &str) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= TITLE_MAX {
        return one_line;
    }
    // Truncate on a word boundary so we never emit half a word.
    let mut out = String::new();
    for word in one_line.split(' ') {
        let candidate = if out.is_empty() {
            word.to_string()
        } else {
            format!("{out} {word}")
        };
        if candidate.chars().count() > TITLE_MAX {
            break;
        }
        out = candidate;
    }
    if out.is_empty() {
        one_line.chars().take(TITLE_MAX).collect()
    } else {
        out
    }
}

fn contains_price(s: &str) -> bool {
    let bytes: Vec<char> = s.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if (*c == '$' || *c == '£' || *c == '€')
            && bytes
                .get(i + 1)
                .map(|n| n.is_ascii_digit())
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Remove currency runs from text, plus any separator left dangling by the
/// removal. Unlike `strip_price_mentions` this works on fragments with no
/// sentence punctuation, which is what a listing title is.
fn remove_price_tokens(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '$' | '£' | '€') && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
            // Consume the amount: digits, commas, thousands, one decimal run.
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == ',' || chars[i] == '.')
            {
                i += 1;
            }
            // Drop a separator this price was hanging off, so we don't emit
            // "Tee -" or "Tee ,".
            while out.ends_with(' ')
                || out.ends_with('-')
                || out.ends_with('|')
                || out.ends_with(',')
                || out.ends_with('–')
                || out.ends_with('—')
            {
                out.pop();
            }
            continue;
        }
        out.push(c);
        i += 1;
    }

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Prices go stale the moment the pricing pass runs, so they never belong in
/// listing copy — including titles.
fn strip_price_mentions(s: &str) -> String {
    let sentences: Vec<&str> = s.split_inclusive(['.', '!', '?']).collect();
    let kept: Vec<&str> = sentences
        .into_iter()
        .filter(|sent| !contains_price(sent))
        .collect();
    let out = kept.join("").trim().to_string();
    if out.is_empty() {
        // Every sentence carried a price. Strip the tokens instead of dropping
        // the whole body, so we never return something emptier than we started.
        let tokenised = remove_price_tokens(s);
        return if tokenised.is_empty() {
            s.to_string()
        } else {
            tokenised
        };
    }
    out
}

/// Lowercase, strip disallowed characters, enforce the 20-char ceiling, dedupe.
fn normalize_tags(raw: &[String], category: &str) -> (Vec<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let mut seen_word_keys: Vec<String> = Vec::new();
    let mut truncated = 0usize;
    let mut dropped = 0usize;
    let mut reordered = 0usize;
    let mut swapped = 0usize;

    for tag in raw {
        match sanitize_tag(tag) {
            Some(t) => {
                if t.chars().count() < tag.trim().chars().count() {
                    truncated += 1;
                }
                if out.contains(&t) {
                    dropped += 1;
                    continue;
                }
                // "custom pet portrait" and "pet portrait custom" are the same
                // search phrase. Keeping both wastes a limited tag slot.
                let key = word_key(&t);
                if seen_word_keys.contains(&key) {
                    reordered += 1;
                    continue;
                }
                // Same phrase, swapped garment noun — no extra reach either.
                if out.iter().any(|kept| garment_variant_of(kept, &t)) {
                    swapped += 1;
                    continue;
                }
                seen_word_keys.push(key);
                out.push(t);
            }
            None => dropped += 1,
        }
    }

    if truncated > 0 {
        warnings.push(format!(
            "{truncated} tag(s) shortened to fit the {TAG_LEN_MAX}-char limit"
        ));
    }
    if dropped > 0 {
        warnings.push(format!("{dropped} duplicate/empty tag(s) dropped"));
    }
    if reordered > 0 {
        warnings.push(format!(
            "{reordered} reordered-duplicate tag(s) dropped (same words, no extra reach)"
        ));
    }
    if swapped > 0 {
        warnings.push(format!(
            "{swapped} garment-swap variant tag(s) dropped (e.g. tee/shirt/blouse)"
        ));
    }

    let _ = category;
    (out, warnings)
}

/// Order-insensitive identity of a tag's words.
fn word_key(tag: &str) -> String {
    let mut words: Vec<&str> = tag.split_whitespace().collect();
    words.sort_unstable();
    words.join(" ")
}

/// Nouns small models swap to invent "new" tags that search identically:
/// "retro graphic tee" becomes tee/shirt/blouse/top/dress.
fn is_garment_noun(w: &str) -> bool {
    matches!(
        w,
        "tee"
            | "tees"
            | "tshirt"
            | "tshirts"
            | "t-shirt"
            | "t-shirts"
            | "shirt"
            | "shirts"
            | "blouse"
            | "blouses"
            | "top"
            | "tops"
            | "dress"
            | "dresses"
            | "sweatshirt"
            | "hoodie"
            | "tank"
            | "jumper"
            | "pullover"
            | "apparel"
            | "clothing"
    )
}

/// True when two multi-word tags are the same phrase with only the trailing
/// garment noun swapped. Deliberately narrow: "gift for her" and "gift for him"
/// differ in a non-garment word, so both survive.
fn garment_variant_of(a: &str, b: &str) -> bool {
    let wa: Vec<&str> = a.split_whitespace().collect();
    let wb: Vec<&str> = b.split_whitespace().collect();
    if wa.len() < 2 || wa.len() != wb.len() {
        return false;
    }
    let (ha, hb) = (&wa[..wa.len() - 1], &wb[..wb.len() - 1]);
    let (ta, tb) = (wa[wa.len() - 1], wb[wb.len() - 1]);
    ha == hb && ta != tb && is_garment_noun(ta) && is_garment_noun(tb)
}

fn sanitize_tag(raw: &str) -> Option<String> {
    let mapped: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let collapsed = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    // "t-shirt" is one word, not two. Without this the hyphen-segment logic in
    // fit_tag clips it to a useless "retro graphic t".
    let collapsed = collapsed.replace("t-shirt", "tshirt");
    Some(fit_tag(&collapsed))
}

/// Trim to the ceiling, preferring a word/hyphen boundary over a hard cut — and
/// never leave a dangling connector, which is how you end up shipping junk tags
/// like "pet portrait from".
fn fit_tag(tag: &str) -> String {
    if tag.chars().count() <= TAG_LEN_MAX {
        return tag.to_string();
    }
    // Try progressively shorter hyphen segments ("barista-inspired-decor").
    let parts: Vec<&str> = tag.split('-').collect();
    if parts.len() > 1 {
        for take in (1..parts.len()).rev() {
            let candidate = parts[..take].join("-");
            if !candidate.is_empty() && candidate.chars().count() <= TAG_LEN_MAX {
                return candidate;
            }
        }
    }
    // Then word boundaries.
    let mut words: Vec<&str> = Vec::new();
    for word in tag.split(' ') {
        let mut candidate = words.clone();
        candidate.push(word);
        if candidate.join(" ").chars().count() > TAG_LEN_MAX {
            break;
        }
        words.push(word);
    }
    while words.len() > 1 && is_trailing_filler(words[words.len() - 1]) {
        words.pop();
    }
    if !words.is_empty() {
        return words.join(" ");
    }
    tag.chars().take(TAG_LEN_MAX).collect()
}

/// Words that carry no search intent on their own, so a tag should never end on
/// one after a clip.
fn is_trailing_filler(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "of"
            | "to"
            | "in"
            | "on"
            | "at"
            | "by"
            | "for"
            | "from"
            | "with"
            | "your"
            | "my"
            | "our"
            | "is"
    )
}

/// Etsy wants 13 tags; anything short wastes free traffic. Top up from a
/// category pool so we always ship exactly 13.
fn pad_tags(tags: &mut Vec<String>, category: &str, warnings: &mut Vec<String>) {
    if tags.len() >= TAGS_MAX {
        return;
    }
    let pool: &[&str] = match category {
        "clothing" => &[
            "graphic tee",
            "unisex t shirt",
            "film lover gift",
            "movie shirt",
            "retro tee",
            "funny t shirt",
            "gift for him",
            "gift for her",
            "cotton tee",
            "cult classic",
            "minimalist shirt",
            "birthday gift",
            "vintage style tee",
        ],
        "jewelry" => &[
            "jewelry",
            "gift for her",
            "handmade jewelry",
            "necklace",
            "dainty jewelry",
            "birthday gift",
            "personalized",
            "everyday jewelry",
            "minimal jewelry",
            "layering necklace",
            "gift for mom",
            "sterling silver",
            "gold filled",
        ],
        "art & collectibles" => &[
            "wall art",
            "art print",
            "home decor",
            "custom art",
            "original painting",
            "gallery wall",
            "fine art",
            "modern art",
            "gift for him",
            "housewarming",
            "unique art",
            "made to order",
            "art poster",
        ],
        _ => &[
            "home decor",
            "wall art",
            "gift for her",
            "housewarming",
            "cozy home",
            "cottagecore",
            "interior design",
            "unique gift",
            "handmade decor",
            "boho decor",
            "minimalist",
            "living room art",
            "gift for him",
        ],
    };

    let before = tags.len();
    for candidate in pool {
        if tags.len() >= TAGS_MAX {
            break;
        }
        if let Some(t) = sanitize_tag(candidate) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
    }
    if tags.len() > before {
        warnings.push(format!(
            "padded {} -> {} tags from the '{}' pool",
            before,
            tags.len(),
            category
        ));
    }
}

// ------------------------------------------------------------ template path

/// Offline fallback. Deterministic, and still produces a compliant listing with
/// all 13 tags — so `generate` is genuinely useful with no LLM running.
pub fn generate_with_template(product: &Product) -> GenerationOutcome {
    let mut warnings = vec!["generated from template (no LLM used)".to_string()];

    let headline = product
        .name
        .split(',')
        .next()
        .unwrap_or(&product.name)
        .trim()
        .to_string();

    let title = clamp_title(&format!(
        "{} | Handmade {} Gift, Made to Order",
        headline, product.category
    ));

    let description = format!(
        "{} — made by hand in small batches, one at a time.\n\n\
         Each piece is finished to order, so yours arrives clean, consistent, and ready to \
         give. A natural fit for {} styling and an easy gift for housewarmings, birthdays, \
         and holidays. Message us for custom sizing, colors, or a made-to-match set.",
        headline,
        product.category.replace("&", "and")
    );

    let mut tags: Vec<String> = Vec::new();
    if let Some(first) = sanitize_tag(
        headline
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
            .as_str(),
    ) {
        tags.push(first);
    }
    for w in headline.split_whitespace() {
        if tags.len() >= 6 {
            break;
        }
        if let Some(t) = sanitize_tag(w) {
            if t.chars().count() >= 3 && !tags.contains(&t) {
                tags.push(t);
            }
        }
    }
    for base in [
        "handmade",
        "made to order",
        "gift for her",
        "unique gift",
        "home decor",
    ] {
        if tags.len() >= TAGS_MAX {
            break;
        }
        if let Some(t) = sanitize_tag(base) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
    }
    pad_tags(&mut tags, &product.category, &mut warnings);
    tags.truncate(TAGS_MAX);

    GenerationOutcome {
        listing: Listing {
            id: 0,
            product_id: product.id,
            title,
            description,
            tags,
            price: product.market_price,
            status: "active".to_string(),
            created_at: Utc::now().to_rfc3339(),
            source: "template".to_string(),
            model: None,
            unit_cost: 0.0,
        },
        warnings,
    }
}

/// Try the local model, fall back to the template. Never fails, always reports
/// which path was taken.
pub async fn generate_listing(product: &Product, config: &Config) -> GenerationOutcome {
    match generate_with_llm(product, config).await {
        Ok(outcome) => outcome,
        Err(reason) => {
            let mut outcome = generate_with_template(product);
            outcome
                .warnings
                .insert(0, format!("LLM unavailable, used template: {reason}"));
            outcome
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product() -> Product {
        Product {
            id: 1,
            name: "Espresso Martini Scented Candle, Foodie Home Decor".to_string(),
            category: "home & living".to_string(),
            market_price: 17.0,
            demand_score: 0.9,
            competition_score: 0.6,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            search_term: Some("espresso martini candle".to_string()),
        }
    }

    #[test]
    fn tags_never_exceed_marketplace_limits() {
        // The real phi3:mini output that broke the first version: 40 tags, some
        // 22 characters long.
        let raw: Vec<String> = vec![
            "soy wax",
            "barista-inspired-decor",
            "eco-friendly-candle",
            "espresso",
            "SOY WAX",
            "",
            "###",
            "a".repeat(60).as_str(),
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let (tags, _) = normalize_tags(&raw, "home & living");
        for t in &tags {
            assert!(t.chars().count() <= TAG_LEN_MAX, "tag too long: {t}");
        }
        let mut padded = tags.clone();
        let mut w = vec![];
        pad_tags(&mut padded, "home & living", &mut w);
        assert_eq!(padded.len(), TAGS_MAX);
        let unique: std::collections::HashSet<_> = padded.iter().collect();
        assert_eq!(unique.len(), padded.len(), "duplicate tags emitted");
    }

    #[test]
    fn title_is_clamped_on_a_word_boundary() {
        let long = "word ".repeat(60);
        let t = clamp_title(&long);
        assert!(t.chars().count() <= TITLE_MAX);
        assert!(!t.ends_with(' '));
    }

    #[test]
    fn price_mentions_are_stripped_from_copy() {
        let d = "Lovely candle. Priced at $17.00 for you. Ships fast.";
        let out = strip_price_mentions(d);
        assert!(!out.contains("$17"), "{}", out);
        assert!(out.contains("Ships fast"));
    }

    #[test]
    fn clipped_tags_never_end_on_a_filler_word() {
        // Observed in real phi3:mini output before the fix.
        assert_eq!(fit_tag("pet portrait from photo"), "pet portrait");
        assert_eq!(fit_tag("gift for your cat lover"), "gift for your cat");
        // Hyphen segments still win when they fit.
        assert_eq!(fit_tag("barista-inspired-decor"), "barista-inspired");
    }

    #[test]
    fn reordered_duplicate_tags_are_collapsed() {
        let raw: Vec<String> = vec![
            "custom pet portrait",
            "pet portrait custom",
            "custom pet portrait", // exact dupe
            "pet portrait",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let (tags, warnings) = normalize_tags(&raw, "art & collectibles");
        assert_eq!(tags.len(), 2, "got {tags:?}");
        assert!(tags.contains(&"custom pet portrait".to_string()));
        assert!(warnings.iter().any(|w| w.contains("reordered")));
    }

    #[test]
    fn title_prices_are_stripped() {
        // Real phi3:mini output: a hardcoded price in the title, stale the
        // moment the pricing pass ran.
        assert_eq!(
            remove_price_tokens("Epic Film Inspired T-Shirt - $27.00"),
            "Epic Film Inspired T-Shirt"
        );
        assert_eq!(
            remove_price_tokens("Cotton Tee | $1,299.99 | Free Ship"),
            "Cotton Tee | Free Ship"
        );
        assert_eq!(
            remove_price_tokens("Retro Graphic Tee - Vintage Style Tee"),
            "Retro Graphic Tee - Vintage Style Tee"
        );
    }

    #[test]
    fn garment_swap_variants_collapse_but_real_variants_survive() {
        let raw: Vec<String> = vec![
            "retro graphic tee",
            "retro graphic shirt",
            "retro graphic blouse",
            "retro graphic top",
            "retro graphic dress",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let (tags, warnings) = normalize_tags(&raw, "clothing");
        assert_eq!(tags.len(), 1, "got {tags:?}");
        assert!(warnings.iter().any(|w| w.contains("garment-swap")));

        // Non-garment differences are genuinely different tags.
        let distinct: Vec<String> = vec!["gift for her", "gift for him", "gift for mom"]
            .into_iter()
            .map(String::from)
            .collect();
        let (kept, _) = normalize_tags(&distinct, "clothing");
        assert_eq!(kept.len(), 3, "gift-for tags must all survive: {kept:?}");
    }

    #[test]
    fn t_shirt_is_treated_as_one_word() {
        // Clipping at the hyphen produced the real tag "retro graphic t".
        assert_eq!(
            sanitize_tag("Retro Graphic t-shirt").unwrap(),
            "retro graphic tshirt"
        );
        assert_eq!(sanitize_tag("T-Shirts").unwrap(), "tshirts");
        // And the swap-variant rule then catches it against the tee spelling.
        let raw: Vec<String> = vec!["retro graphic tee".into(), "retro graphic tshirt".into()];
        let (tags, _) = normalize_tags(&raw, "clothing");
        assert_eq!(tags.len(), 1, "got {tags:?}");
    }

    #[test]
    fn template_output_is_compliant() {
        let o = generate_with_template(&product());
        assert_eq!(o.listing.tags.len(), TAGS_MAX);
        assert!(o.listing.title.chars().count() <= TITLE_MAX);
        assert_eq!(o.listing.source, "template");
    }

    #[test]
    fn extracts_json_from_fenced_output() {
        let fenced = "```json\n{\"title\":\"t\",\"description\":\"d\",\"tags\":[\"a\"]}\n```";
        let got = extract_json_object(fenced).unwrap();
        assert!(got.starts_with('{'));
    }
}
