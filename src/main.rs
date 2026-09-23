use clap::{Parser, Subcommand};
use ecommerce_optimizer::{
    listing, pricing, scanner, Config, GenerateReport, Marketplace, OptimizeReport, Optimizer,
    ScanReport,
};
use serde_json::json;
use std::io::Write;

#[derive(Parser, Debug)]
#[command(
    name = "ecommerce-optimizer",
    version,
    about = "Local-first e-commerce listing optimization bot",
    long_about = "Scans market opportunities, writes optimized listings with a local LLM, \
                  and prices them with fee-aware margin targeting. Runs entirely offline."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to a JSON config file. Created with `config --write`.
    #[arg(long, default_value = "config.json", global = true)]
    config: String,

    /// SQLite database path.
    #[arg(long, global = true)]
    db: Option<String>,

    /// Marketplace: etsy | ebay | amazon.
    #[arg(long, global = true)]
    marketplace: Option<String>,

    /// Comma-separated categories to scan.
    #[arg(long, global = true)]
    categories: Option<String>,

    /// Target net margin on cost, e.g. 0.30 for 30%.
    #[arg(long, global = true)]
    margin: Option<f64>,

    /// Marketplace API key. Etsy requires the "keystring:shared_secret" form.
    #[arg(long, global = true)]
    api_key: Option<String>,

    /// Ollama model used for listing copy.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Ollama base URL.
    #[arg(long, global = true)]
    ollama_url: Option<String>,

    /// How many listings to generate in parallel.
    #[arg(long, global = true)]
    concurrency: Option<usize>,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Find product opportunities and store them.
    Scan,
    /// Write listings for products that do not have one yet.
    Generate {
        /// Regenerate copy for every product, including ones that already have a listing.
        #[arg(long)]
        force: bool,
        /// Only process the first N products, highest demand first.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Re-price active listings against competitors and the margin floor.
    Optimize {
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run scan, generate, then optimize.
    All {
        #[arg(long)]
        dry_run: bool,
    },
    /// Show database contents and the last few pricing decisions.
    Status,
    /// Run the full cycle on an interval until interrupted.
    Watch {
        /// Seconds between cycles.
        #[arg(long)]
        interval: Option<u64>,
        /// Stop after N cycles (default: run forever).
        #[arg(long)]
        iterations: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Check the local environment: Ollama, model, API key, database.
    Doctor,
    /// Write a default config.json to disk.
    Config {
        #[arg(long)]
        write: bool,
    },
}

fn resolve_config(args: &Args) -> Result<(Config, bool), String> {
    let (mut cfg, from_file) = Config::load(&args.config)?;
    if let Some(v) = &args.db {
        cfg.db_path = v.clone();
    }
    if let Some(v) = &args.marketplace {
        cfg.marketplace = Marketplace::parse(v);
    }
    if let Some(v) = &args.categories {
        cfg.categories = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(v) = args.margin {
        if !(0.0..0.99).contains(&v) {
            return Err(format!("--margin {v} is out of range (expected 0.0..0.99)"));
        }
        cfg.target_margin = v;
    }
    if let Some(v) = &args.api_key {
        cfg.api_keys.marketplace = v.clone();
    }
    if let Some(v) = &args.model {
        cfg.api_keys.llm = Some(v.clone());
    }
    if let Some(v) = &args.ollama_url {
        cfg.ollama_url = v.clone();
    }
    if let Some(v) = args.concurrency {
        cfg.concurrency = v.clamp(1, 16);
    }
    Ok((cfg, from_file))
}

fn money(v: f64) -> String {
    format!("${v:.2}")
}

fn print_scan(r: &ScanReport, json_out: bool) {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "command": "scan",
                "source": r.source,
                "found": r.found,
                "stored": r.written,
                "notes": r.notes,
            }))
            .unwrap()
        );
        return;
    }
    println!("Scan complete.");
    println!("  source : {}", r.source);
    println!("  found  : {} opportunities", r.found);
    println!("  stored : {} (upserted, no duplicates)", r.written);
    for n in &r.notes {
        println!("  note   : {n}");
    }
}

fn print_generate(r: &GenerateReport, json_out: bool) {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "command": "generate",
                "attempted": r.attempted,
                "from_llm": r.from_llm,
                "from_template": r.from_template,
                "warnings": r.warnings,
            }))
            .unwrap()
        );
        return;
    }
    println!("Generation complete.");
    println!("  attempted     : {}", r.attempted);
    println!("  from local LLM: {}", r.from_llm);
    println!("  from template : {}", r.from_template);
    for w in r.warnings.iter().take(40) {
        println!("  warn: {w}");
    }
    if r.warnings.len() > 40 {
        println!("  ... {} more warnings", r.warnings.len() - 40);
    }
}

fn print_optimize(r: &OptimizeReport, json_out: bool) {
    if json_out {
        let rows: Vec<_> = r
            .rows
            .iter()
            .map(|x| {
                json!({
                    "listing_id": x.listing_id,
                    "product": x.product,
                    "old_price": x.old_price,
                    "new_price": x.new_price,
                    "margin": x.margin,
                    "method": x.method,
                    "source": x.source,
                    "note": x.note,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "command": "optimize",
                "dry_run": r.dry_run,
                "considered": r.considered,
                "updated": r.updated,
                "margin_met": r.margin_met,
                "margin_unmet": r.margin_unmet,
                "rows": rows,
            }))
            .unwrap()
        );
        return;
    }

    println!(
        "Pricing complete{}.",
        if r.dry_run {
            " (dry run — nothing written)"
        } else {
            ""
        }
    );
    println!("  considered    : {}", r.considered);
    println!("  updated       : {}", r.updated);
    println!("  margin met    : {}", r.margin_met);
    println!("  margin unmet  : {}", r.margin_unmet);
    println!();
    println!(
        "  {:<44} {:>9} {:>9} {:>8} {:<10}",
        "product", "was", "now", "margin", "method"
    );
    for row in &r.rows {
        let name: String = row.product.chars().take(43).collect();
        println!(
            "  {:<44} {:>9} {:>9} {:>7.1}% {:<10}",
            name,
            money(row.old_price),
            money(row.new_price),
            row.margin * 100.0,
            row.method.chars().take(10).collect::<String>()
        );
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args = Args::parse();

    if matches!(args.command, Some(Commands::Config { write: true })) {
        Config::write_default(&args.config)?;
        println!("Wrote default config to {}", args.config);
        return Ok(());
    }

    let (cfg, from_file) = resolve_config(&args)?;

    // Fail on an impossible margin before touching anything.
    pricing::margin_floor(1.0, &cfg)?;

    let json_out = args.json;
    let optimizer = Optimizer::new(cfg).await?;

    match args.command {
        Some(Commands::Doctor) => {
            doctor(&optimizer.config().clone()).await;
        }

        Some(Commands::Status) => {
            let s = optimizer.status().await?;
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "products": s.products,
                        "listings": s.listings,
                        "products_without_listings": s.products_without_listings,
                        "listings_by_source": s.by_source,
                    }))
                    .unwrap()
                );
            } else {
                println!("Database status");
                println!("  products                : {}", s.products);
                println!("  listings                : {}", s.listings);
                println!(
                    "  products needing listing: {}",
                    s.products_without_listings
                );
                if s.by_source.is_empty() {
                    println!("  listing sources         : (none)");
                } else {
                    for (src, n) in &s.by_source {
                        println!("  listings via {src:<11}: {n}");
                    }
                }
                if !s.recent_decisions.is_empty() {
                    println!("\nRecent pricing decisions");
                    println!(
                        "  {:<8} {:>10} {:>10} {:>8} {:<10}",
                        "listing", "floor", "price", "margin", "source"
                    );
                    for d in &s.recent_decisions {
                        println!(
                            "  {:<8} {:>10} {:>10} {:>7.1}% {:<10}",
                            d.listing_id,
                            money(d.floor_price),
                            money(d.recommended_price),
                            d.margin_achieved * 100.0,
                            d.source
                        );
                    }
                }
            }
        }

        Some(Commands::Scan) => {
            let r = optimizer.scan().await?;
            print_scan(&r, json_out);
        }

        Some(Commands::Generate { force, limit }) => {
            let r = optimizer.generate(force, limit).await?;
            print_generate(&r, json_out);
        }

        Some(Commands::Optimize { dry_run }) => {
            let r = optimizer.optimize(dry_run).await?;
            print_optimize(&r, json_out);
        }

        Some(Commands::All { dry_run }) => {
            let (s, g, o) = optimizer.run_all(dry_run).await?;
            print_scan(&s, json_out);
            println!();
            print_generate(&g, json_out);
            println!();
            print_optimize(&o, json_out);
        }

        Some(Commands::Watch {
            interval,
            iterations,
            dry_run,
        }) => {
            let secs = interval.unwrap_or(optimizer.config().watch_interval_secs);
            println!(
                "Watching: full cycle every {}s{}. Ctrl-C to stop.",
                secs,
                match iterations {
                    Some(n) => format!(", {n} iteration(s)"),
                    None => String::new(),
                }
            );
            let mut cycle: u64 = 0;
            loop {
                cycle += 1;
                println!(
                    "\n=== cycle {} @ {} ===",
                    cycle,
                    chrono::Local::now().to_rfc3339()
                );
                match optimizer.run_all(dry_run).await {
                    Ok((s, g, o)) => {
                        print_scan(&s, json_out);
                        print_generate(&g, json_out);
                        print_optimize(&o, json_out);
                    }
                    Err(e) => eprintln!("cycle {cycle} failed, continuing: {e}"),
                }
                std::io::stdout().flush().ok();
                if let Some(n) = iterations {
                    if cycle >= n {
                        break;
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                    _ = tokio::signal::ctrl_c() => {
                        println!("\nInterrupted. Stopping after {cycle} cycle(s).");
                        break;
                    }
                }
            }
        }

        Some(Commands::Config { .. }) => {
            println!(
                "Config at {} does not exist. Run `config --write` to create it.",
                args.config
            );
            println!("Current effective settings:");
            let cfg = optimizer.config();
            println!("  marketplace    : {}", cfg.marketplace.as_str());
            println!("  database       : {}", cfg.db_path);
            println!("  categories     : {}", cfg.categories.join(", "));
            println!("  target margin  : {:.0}%", cfg.target_margin * 100.0);
            println!("  ollama url     : {}", cfg.ollama_url);
            println!("  llm model      : {}", cfg.llm_model());
        }

        None => {
            // Bare invocation keeps the original behaviour: run the cycle.
            let (s, g, o) = optimizer.run_all(false).await?;
            print_scan(&s, json_out);
            println!();
            print_generate(&g, json_out);
            println!();
            print_optimize(&o, json_out);
        }
    }

    if !json_out {
        println!("\nDone.");
        if !from_file {
            println!(
                "Tip: `config --write` to create {} and make settings permanent.",
                args.config
            );
        }
    }
    Ok(())
}

async fn doctor(cfg: &Config) {
    println!("Environment check");
    println!("  marketplace      : {}", cfg.marketplace.as_str());
    println!("  target margin    : {:.0}%", cfg.target_margin * 100.0);
    println!(
        "  cost ratio       : {:.0}% of competitor median",
        cfg.cost_ratio * 100.0
    );
    println!("  database         : {}", cfg.db_path);
    println!(
        "  database exists  : {}",
        if std::path::Path::new(&cfg.db_path).exists() {
            "yes"
        } else {
            "no (will be created)"
        }
    );
    println!("  ollama url       : {}", cfg.ollama_url);
    println!("  llm model        : {}", cfg.llm_model());
    println!(
        "  marketplace key  : {}",
        if cfg.has_marketplace_key() {
            "set"
        } else {
            "not set (seed provider + offline pricing)"
        }
    );

    if cfg.has_marketplace_key() {
        match scanner::verify_api_key(cfg).await {
            Ok(app_id) => println!("  key status       : LIVE (application_id {app_id})"),
            Err(e) => println!("  key status       : NOT USABLE — {e}"),
        }
    }

    let (rate, fixed) = cfg.marketplace.fee_model();
    match pricing::margin_floor(1.0, cfg) {
        Ok(floor) => println!(
            "  margin math      : OK (fees {:.1}% + {}, $1.00 cost needs {} to hit target)",
            rate * 100.0,
            money(fixed),
            money(floor)
        ),
        Err(e) => println!("  margin math      : BROKEN — {e}"),
    }

    match listing::ollama_models(cfg).await {
        Ok(models) => {
            println!("  ollama reachable : yes ({} model(s))", models.len());
            let want = cfg.llm_model();
            let exact = models.iter().any(|m| m == &want);
            let prefix = models
                .iter()
                .find(|m| m.split(':').next() == Some(want.split(':').next().unwrap_or(&want)));
            if exact {
                println!("  model available  : yes ({want})");
            } else if let Some(alt) = prefix {
                println!(
                    "  model available  : no — '{want}' not pulled, but '{alt}' is. Try --model {alt}"
                );
            } else {
                println!(
                    "  model available  : no — run `ollama pull {}` (available: {})",
                    want,
                    models.join(", ")
                );
            }
        }
        Err(e) => println!("  ollama reachable : NO — {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_surface_parses() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }
}
