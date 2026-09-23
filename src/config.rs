use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Marketplace {
    Etsy,
    #[serde(alias = "ebay")]
    EBay,
    Amazon,
}

impl Marketplace {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "ebay" => Marketplace::EBay,
            "amazon" => Marketplace::Amazon,
            _ => Marketplace::Etsy,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Marketplace::Etsy => "etsy",
            Marketplace::EBay => "ebay",
            Marketplace::Amazon => "amazon",
        }
    }

    /// Fee model per marketplace: (variable_rate, fixed_per_order).
    /// Etsy US: 6.5% transaction + 3% payment processing, $0.25 payment fixed,
    /// plus $0.20 listing fee amortized per unit.
    pub fn fee_model(&self) -> (f64, f64) {
        match self {
            Marketplace::Etsy => (0.095, 0.45),
            Marketplace::EBay => (0.13, 0.30),
            Marketplace::Amazon => (0.15, 0.00),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiKeys {
    pub marketplace: String,
    pub llm: Option<String>,
}

impl Default for ApiKeys {
    fn default() -> Self {
        ApiKeys {
            marketplace: String::new(),
            llm: Some("phi3:mini".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub db_path: String,
    pub marketplace: Marketplace,
    pub api_keys: ApiKeys,
    pub categories: Vec<String>,

    /// Target net margin on cost, as a fraction. 0.30 = 30%.
    pub target_margin: f64,

    /// Share of the competitor reference price assumed to be our unit cost.
    /// Used to derive a margin floor when no real cost data is on file.
    pub cost_ratio: f64,

    /// Ollama base URL. Local-first: this never leaves the machine.
    pub ollama_url: String,

    /// Per-request timeout for Ollama, seconds.
    pub llm_timeout_secs: u64,

    /// Per-request timeout for marketplace HTTP calls, seconds.
    pub http_timeout_secs: u64,

    /// How many listings to generate in parallel.
    pub concurrency: usize,

    /// Default interval for `watch`, in seconds.
    pub watch_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db_path: "data/optimizer.db".to_string(),
            marketplace: Marketplace::Etsy,
            api_keys: ApiKeys::default(),
            categories: vec![
                "home & living".to_string(),
                "jewelry".to_string(),
                "art & collectibles".to_string(),
            ],
            target_margin: 0.30,
            cost_ratio: 0.45,
            ollama_url: "http://localhost:11434".to_string(),
            llm_timeout_secs: 240,
            http_timeout_secs: 20,
            concurrency: 3,
            watch_interval_secs: 21_600, // 6 hours
        }
    }
}

impl Config {
    /// Load config from a JSON file, falling back to defaults for missing keys.
    /// A missing file is not an error — local-first tools must run with zero setup.
    pub fn load(path: &str) -> Result<(Self, bool), String> {
        if !Path::new(path).exists() {
            return Ok((Config::default(), false));
        }
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        let cfg: Config =
            serde_json::from_str(&raw).map_err(|e| format!("parse {path}: {e}"))?;
        Ok((cfg, true))
    }

    pub fn write_default(path: &str) -> Result<(), String> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {path}: {e}"))?;
        }
        let json = serde_json::to_string_pretty(&Config::default())
            .map_err(|e| format!("serialize config: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("write {path}: {e}"))
    }

    pub fn has_marketplace_key(&self) -> bool {
        !self.api_keys.marketplace.trim().is_empty()
    }

    pub fn llm_model(&self) -> String {
        self.api_keys
            .llm
            .clone()
            .unwrap_or_else(|| "phi3:mini".to_string())
    }
}
