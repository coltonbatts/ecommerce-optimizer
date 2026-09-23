use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

pub mod config;
pub mod database;
pub mod listing;
pub mod pricing;
pub mod scanner;

pub use config::{ApiKeys, Config, Marketplace};
pub use database::{CompetitorPrice, Database, Listing, PricingDecision, Product};
pub use listing::GenerationOutcome;
pub use scanner::ScanSource;

#[derive(Debug, Default)]
pub struct ScanReport {
    pub found: usize,
    pub written: usize,
    pub source: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Default)]
pub struct GenerateReport {
    pub attempted: usize,
    pub from_llm: usize,
    pub from_template: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct PriceRow {
    pub listing_id: i64,
    pub product: String,
    pub old_price: f64,
    pub new_price: f64,
    pub margin: f64,
    pub method: String,
    pub source: String,
    pub note: String,
}

#[derive(Debug, Default)]
pub struct OptimizeReport {
    pub considered: usize,
    pub updated: usize,
    pub margin_met: usize,
    pub margin_unmet: usize,
    pub dry_run: bool,
    pub rows: Vec<PriceRow>,
}

pub struct Optimizer {
    config: Arc<Config>,
    db: Arc<Mutex<Database>>,
    seeds_path: String,
}

impl Optimizer {
    pub async fn new(config: Config) -> Result<Self, String> {
        let db = Database::new(&config.db_path)
            .map_err(|e| format!("failed to open {}: {}", config.db_path, e))?;
        Ok(Optimizer {
            seeds_path: "data/seeds.json".to_string(),
            config: Arc::new(config),
            db: Arc::new(Mutex::new(db)),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    // ------------------------------------------------------------- scan

    pub async fn scan(&self) -> Result<ScanReport, String> {
        let outcome = scanner::scan_trending(&self.config, &self.seeds_path).await?;
        let found = outcome.products.len();

        let db = self.db.lock().await;
        let mut written = 0usize;
        for product in &outcome.products {
            match db.upsert_product(product) {
                Ok(_) => written += 1,
                Err(e) => {
                    eprintln!("  ! failed to store '{}': {}", product.name, e);
                }
            }
        }

        Ok(ScanReport {
            found,
            written,
            source: outcome.source.as_str().to_string(),
            notes: outcome.notes,
        })
    }

    // --------------------------------------------------------- generate

    pub async fn generate(
        &self,
        force: bool,
        limit: Option<usize>,
    ) -> Result<GenerateReport, String> {
        let candidates = {
            let db = self.db.lock().await;
            let all = if force {
                db.all_products()
            } else {
                db.products_needing_listings()
            }
            .map_err(|e| format!("query products: {e}"))?;
            match limit {
                Some(n) => all.into_iter().take(n).collect::<Vec<_>>(),
                None => all,
            }
        };

        let mut report = GenerateReport {
            attempted: candidates.len(),
            ..Default::default()
        };
        if candidates.is_empty() {
            return Ok(report);
        }

        let semaphore = Arc::new(Semaphore::new(self.config.concurrency.max(1)));
        let mut set: JoinSet<(Product, GenerationOutcome)> = JoinSet::new();

        for product in candidates {
            let cfg = self.config.clone();
            let sem = semaphore.clone();
            set.spawn(async move {
                let _permit = sem.acquire().await;
                let outcome = listing::generate_listing(&product, &cfg).await;
                (product, outcome)
            });
        }

        while let Some(joined) = set.join_next().await {
            let (product, outcome) = match joined {
                Ok(v) => v,
                Err(e) => {
                    report
                        .warnings
                        .push(format!("generation task panicked: {e}"));
                    continue;
                }
            };

            let mut listing = outcome.listing;
            let cfg = self.config.clone();
            let unit_cost = pricing::estimate_competitors(&product).reference * cfg.cost_ratio;
            listing.unit_cost = (unit_cost * 100.0).round() / 100.0;

            if listing.source == "llm" {
                report.from_llm += 1;
            } else {
                report.from_template += 1;
            }
            for w in &outcome.warnings {
                report.warnings.push(format!("{}: {}", product.name, w));
            }

            let db = self.db.lock().await;
            if let Err(e) = db.upsert_listing(&listing) {
                report
                    .warnings
                    .push(format!("{}: failed to store listing: {}", product.name, e));
            }
        }

        Ok(report)
    }

    // --------------------------------------------------------- optimize

    pub async fn optimize(&self, dry_run: bool) -> Result<OptimizeReport, String> {
        let listings = {
            let db = self.db.lock().await;
            db.active_listings()
                .map_err(|e| format!("query listings: {e}"))?
        };

        let mut report = OptimizeReport {
            considered: listings.len(),
            dry_run,
            ..Default::default()
        };

        for listing in listings {
            let product = {
                let db = self.db.lock().await;
                match db.product_by_id(listing.product_id) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        report.rows.push(PriceRow {
                            listing_id: listing.id,
                            product: format!("(missing product {})", listing.product_id),
                            old_price: listing.price,
                            new_price: listing.price,
                            margin: 0.0,
                            method: "skipped".into(),
                            source: "-".into(),
                            note: "product row no longer exists".into(),
                        });
                        continue;
                    }
                    Err(e) => {
                        report.warnings_push(format!("lookup product: {e}"));
                        continue;
                    }
                }
            };

            let stats = pricing::competitor_stats(&product, &self.config).await;
            let outcome = match pricing::optimize_price(&listing, &product, &stats, &self.config) {
                Ok(o) => o,
                Err(e) => {
                    report.rows.push(PriceRow {
                        listing_id: listing.id,
                        product: product.name.clone(),
                        old_price: listing.price,
                        new_price: listing.price,
                        margin: 0.0,
                        method: "error".into(),
                        source: stats.source.clone(),
                        note: e,
                    });
                    continue;
                }
            };

            if outcome.decision.method == "margin_floor_above_market" {
                report.margin_unmet += 1;
            } else {
                report.margin_met += 1;
            }

            if !dry_run {
                let db = self.db.lock().await;
                if let Err(e) =
                    db.update_listing_price(listing.id, outcome.decision.recommended_price)
                {
                    report.warnings_push(format!("update price: {e}"));
                    continue;
                }
                let _ = db.record_competitor_price(&outcome.stats.to_record(product.id));
                if let Err(e) = db.record_decision(&outcome.decision) {
                    report.warnings_push(format!("record decision: {e}"));
                }
            }

            report.updated += 1;
            report.rows.push(PriceRow {
                listing_id: listing.id,
                product: product.name.clone(),
                old_price: listing.price,
                new_price: outcome.decision.recommended_price,
                margin: outcome.decision.margin_achieved,
                method: outcome.decision.method.clone(),
                source: outcome.decision.source.clone(),
                note: outcome.decision.notes.clone(),
            });
        }

        Ok(report)
    }

    pub async fn run_all(
        &self,
        dry_run: bool,
    ) -> Result<(ScanReport, GenerateReport, OptimizeReport), String> {
        let scan = self.scan().await?;
        let generate = self.generate(false, None).await?;
        let optimize = self.optimize(dry_run).await?;
        Ok((scan, generate, optimize))
    }

    // ----------------------------------------------------------- status

    pub async fn status(&self) -> Result<StatusReport, String> {
        let db = self.db.lock().await;
        let products = db.count_products().map_err(|e| e.to_string())?;
        let listings = db.count_listings().map_err(|e| e.to_string())?;
        let by_source = db.count_listings_by_source().map_err(|e| e.to_string())?;
        let needs = db
            .products_needing_listings()
            .map_err(|e| e.to_string())?
            .len();
        let decisions = db.recent_decisions(5).map_err(|e| e.to_string())?;
        Ok(StatusReport {
            products,
            listings,
            products_without_listings: needs,
            by_source,
            recent_decisions: decisions,
        })
    }
}

#[derive(Debug)]
pub struct StatusReport {
    pub products: i64,
    pub listings: i64,
    pub products_without_listings: usize,
    pub by_source: Vec<(String, i64)>,
    pub recent_decisions: Vec<PricingDecision>,
}

impl OptimizeReport {
    fn warnings_push(&mut self, msg: String) {
        self.rows.push(PriceRow {
            listing_id: 0,
            product: "-".into(),
            old_price: 0.0,
            new_price: 0.0,
            margin: 0.0,
            method: "warning".into(),
            source: "-".into(),
            note: msg,
        });
    }
}
