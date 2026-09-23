use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct Product {
    pub id: i64,
    pub name: String,
    pub category: String,
    /// Median competitor selling price observed in the market.
    pub market_price: f64,
    pub demand_score: f64,
    pub competition_score: f64,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct Listing {
    pub id: i64,
    pub product_id: i64,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub price: f64,
    pub status: String,
    pub created_at: String,
    /// "llm" or "template"
    pub source: String,
    /// Model name when source == "llm".
    pub model: Option<String>,
    /// Our own unit cost basis used for margin math.
    pub unit_cost: f64,
}

/// Audit row for every pricing decision. Makes cost-optimization idempotent and
/// explainable after the fact.
#[derive(Debug, Clone)]
pub struct PricingDecision {
    pub listing_id: i64,
    pub reference_price: f64,
    pub min_price: f64,
    pub max_price: f64,
    pub floor_price: f64,
    pub recommended_price: f64,
    pub margin_achieved: f64,
    pub method: String,
    pub source: String,
    pub notes: String,
    pub created_at: String,
}

/// Competitor price observation, from the marketplace API or the offline
/// estimator.
#[derive(Debug, Clone)]
pub struct CompetitorPrice {
    pub product_id: i64,
    pub reference_price: f64,
    pub min_price: f64,
    pub max_price: f64,
    pub sample_size: i64,
    pub source: String,
    pub created_at: String,
}

pub struct Database {
    conn: Connection,
}

type DbResult<T> = Result<T, rusqlite::Error>;

impl Database {
    pub fn new(path: &str) -> DbResult<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Database { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> DbResult<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS products (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                category TEXT NOT NULL,
                market_price REAL NOT NULL,
                demand_score REAL NOT NULL DEFAULT 0.0,
                competition_score REAL NOT NULL DEFAULT 0.0,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS listings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                product_id INTEGER NOT NULL REFERENCES products(id),
                title TEXT NOT NULL,
                description TEXT NOT NULL,
                tags TEXT NOT NULL,
                price REAL NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS competitor_prices (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                product_id INTEGER NOT NULL REFERENCES products(id),
                reference_price REAL NOT NULL,
                min_price REAL NOT NULL,
                max_price REAL NOT NULL,
                sample_size INTEGER NOT NULL DEFAULT 0,
                source TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pricing_decisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                listing_id INTEGER NOT NULL REFERENCES listings(id),
                reference_price REAL NOT NULL,
                min_price REAL NOT NULL,
                max_price REAL NOT NULL,
                floor_price REAL NOT NULL,
                recommended_price REAL NOT NULL,
                margin_achieved REAL NOT NULL,
                method TEXT NOT NULL,
                source TEXT NOT NULL,
                notes TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );
            ",
        )?;

        // Additive migrations for databases created by the first version.
        self.ensure_column("listings", "source", "TEXT NOT NULL DEFAULT 'template'")?;
        self.ensure_column("listings", "model", "TEXT")?;
        self.ensure_column("listings", "unit_cost", "REAL NOT NULL DEFAULT 0.0")?;
        self.ensure_column("listings", "updated_at", "TEXT")?;
        self.ensure_column("products", "unit_cost", "REAL")?;
        self.ensure_column("products", "source", "TEXT NOT NULL DEFAULT 'seed'")?;

        // Collapse duplicates left by the pre-idempotent scanner, then make the
        // natural keys unique so re-running any command is a no-op.
        self.conn.execute_batch(
            "
            DELETE FROM products WHERE id NOT IN (
                SELECT MIN(id) FROM products GROUP BY name
            );
            DELETE FROM listings WHERE id NOT IN (
                SELECT MAX(id) FROM listings GROUP BY product_id
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_products_name ON products(name);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_listings_product ON listings(product_id);
            CREATE INDEX IF NOT EXISTS idx_listings_status ON listings(status);
            ",
        )?;

        Ok(())
    }

    fn ensure_column(&self, table: &str, column: &str, decl: &str) -> DbResult<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map(params![], |row| row.get::<_, String>(1))?
            .collect::<DbResult<Vec<String>>>()?;
        drop(stmt);
        if !existing.iter().any(|c| c == column) {
            self.conn
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
        Ok(())
    }

    // ---------- products ----------

    /// Idempotent: re-scanning the same product updates its scores in place.
    pub fn upsert_product(&self, product: &Product) -> DbResult<i64> {
        self.conn.query_row(
            "INSERT INTO products (name, category, market_price, demand_score, competition_score, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET
                category = excluded.category,
                market_price = excluded.market_price,
                demand_score = excluded.demand_score,
                competition_score = excluded.competition_score
             RETURNING id",
            params![
                product.name,
                product.category,
                product.market_price,
                product.demand_score,
                product.competition_score,
                product.created_at
            ],
            |row| row.get(0),
        )
    }

    pub fn all_products(&self) -> DbResult<Vec<Product>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, category, market_price, demand_score, competition_score, created_at
             FROM products ORDER BY demand_score DESC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![], |row| {
                Ok(Product {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    market_price: row.get(3)?,
                    demand_score: row.get(4)?,
                    competition_score: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<DbResult<Vec<Product>>>()?;
        Ok(rows)
    }

    /// Products with no listing yet. Ordered by demand so the best
    /// opportunities are generated first when `--limit` is used.
    pub fn products_needing_listings(&self) -> DbResult<Vec<Product>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name, p.category, p.market_price, p.demand_score, p.competition_score, p.created_at
             FROM products p
             LEFT JOIN listings l ON l.product_id = p.id
             WHERE l.id IS NULL
             ORDER BY p.demand_score DESC, p.id ASC",
        )?;
        let rows = stmt
            .query_map(params![], |row| {
                Ok(Product {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    category: row.get(2)?,
                    market_price: row.get(3)?,
                    demand_score: row.get(4)?,
                    competition_score: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<DbResult<Vec<Product>>>()?;
        Ok(rows)
    }

    pub fn product_by_id(&self, id: i64) -> DbResult<Option<Product>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, category, market_price, demand_score, competition_score, created_at
             FROM products WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(Product {
                id: row.get(0)?,
                name: row.get(1)?,
                category: row.get(2)?,
                market_price: row.get(3)?,
                demand_score: row.get(4)?,
                competition_score: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn count_products(&self) -> DbResult<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM products", params![], |r| r.get(0))
    }

    // ---------- listings ----------

    /// Idempotent upsert keyed on product_id. Regenerating copy never creates a
    /// duplicate listing, and never clobbers the price set by the pricing pass.
    pub fn upsert_listing(&self, listing: &Listing) -> DbResult<i64> {
        let tags_json = serde_json::to_string(&listing.tags).unwrap_or_else(|_| "[]".to_string());
        self.conn.query_row(
            "INSERT INTO listings
                (product_id, title, description, tags, price, status, created_at, source, model, unit_cost, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(product_id) DO UPDATE SET
                title = excluded.title,
                description = excluded.description,
                tags = excluded.tags,
                status = excluded.status,
                source = excluded.source,
                model = excluded.model,
                unit_cost = excluded.unit_cost,
                updated_at = excluded.updated_at
             RETURNING id",
            params![
                listing.product_id,
                listing.title,
                listing.description,
                tags_json,
                listing.price,
                listing.status,
                listing.created_at,
                listing.source,
                listing.model,
                listing.unit_cost,
                listing.created_at
            ],
            |row| row.get(0),
        )
    }

    fn row_to_listing(row: &rusqlite::Row) -> rusqlite::Result<Listing> {
        let tags_json: String = row.get("tags")?;
        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        Ok(Listing {
            id: row.get("id")?,
            product_id: row.get("product_id")?,
            title: row.get("title")?,
            description: row.get("description")?,
            tags,
            price: row.get("price")?,
            status: row.get("status")?,
            created_at: row.get("created_at")?,
            source: row.get("source")?,
            model: row.get("model")?,
            unit_cost: row.get("unit_cost")?,
        })
    }

    const LISTING_COLS: &'static str = "id, product_id, title, description, tags, price, status, \
                                        created_at, source, model, unit_cost";

    pub fn active_listings(&self) -> DbResult<Vec<Listing>> {
        let sql = format!(
            "SELECT {} FROM listings WHERE status = 'active' ORDER BY id ASC",
            Self::LISTING_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![], Self::row_to_listing)?
            .collect::<DbResult<Vec<Listing>>>()?;
        Ok(rows)
    }

    pub fn all_listings(&self) -> DbResult<Vec<Listing>> {
        let sql = format!(
            "SELECT {} FROM listings ORDER BY id ASC",
            Self::LISTING_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![], Self::row_to_listing)?
            .collect::<DbResult<Vec<Listing>>>()?;
        Ok(rows)
    }

    pub fn listing_for_product(&self, product_id: i64) -> DbResult<Option<Listing>> {
        let sql = format!(
            "SELECT {} FROM listings WHERE product_id = ?1",
            Self::LISTING_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![product_id], Self::row_to_listing)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn update_listing_price(&self, listing_id: i64, price: f64) -> DbResult<()> {
        self.conn.execute(
            "UPDATE listings SET price = ?1, updated_at = ?2 WHERE id = ?3",
            params![price, chrono::Utc::now().to_rfc3339(), listing_id],
        )?;
        Ok(())
    }

    /// Used by `generate --force` to re-run the LLM over existing listings.
    pub fn clear_listing(&self, product_id: i64) -> DbResult<()> {
        self.conn.execute(
            "DELETE FROM listings WHERE product_id = ?1",
            params![product_id],
        )?;
        Ok(())
    }

    pub fn count_listings(&self) -> DbResult<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM listings", params![], |r| r.get(0))
    }

    pub fn count_listings_by_source(&self) -> DbResult<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, COUNT(*) FROM listings GROUP BY source ORDER BY source")?;
        let rows = stmt
            .query_map(params![], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<DbResult<Vec<(String, i64)>>>()?;
        Ok(rows)
    }

    // ---------- competitor prices ----------

    pub fn record_competitor_price(&self, cp: &CompetitorPrice) -> DbResult<()> {
        self.conn.execute(
            "INSERT INTO competitor_prices
                (product_id, reference_price, min_price, max_price, sample_size, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                cp.product_id,
                cp.reference_price,
                cp.min_price,
                cp.max_price,
                cp.sample_size,
                cp.source,
                cp.created_at
            ],
        )?;
        Ok(())
    }

    pub fn latest_competitor_price(&self, product_id: i64) -> DbResult<Option<CompetitorPrice>> {
        let mut stmt = self.conn.prepare(
            "SELECT product_id, reference_price, min_price, max_price, sample_size, source, created_at
             FROM competitor_prices WHERE product_id = ?1 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![product_id], |row| {
            Ok(CompetitorPrice {
                product_id: row.get(0)?,
                reference_price: row.get(1)?,
                min_price: row.get(2)?,
                max_price: row.get(3)?,
                sample_size: row.get(4)?,
                source: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    // ---------- pricing decisions ----------

    pub fn record_decision(&self, d: &PricingDecision) -> DbResult<()> {
        self.conn.execute(
            "INSERT INTO pricing_decisions
                (listing_id, reference_price, min_price, max_price, floor_price,
                 recommended_price, margin_achieved, method, source, notes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                d.listing_id,
                d.reference_price,
                d.min_price,
                d.max_price,
                d.floor_price,
                d.recommended_price,
                d.margin_achieved,
                d.method,
                d.source,
                d.notes,
                d.created_at
            ],
        )?;
        Ok(())
    }

    pub fn recent_decisions(&self, limit: usize) -> DbResult<Vec<PricingDecision>> {
        let mut stmt = self.conn.prepare(
            "SELECT listing_id, reference_price, min_price, max_price, floor_price,
                    recommended_price, margin_achieved, method, source, notes, created_at
             FROM pricing_decisions ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(PricingDecision {
                    listing_id: row.get(0)?,
                    reference_price: row.get(1)?,
                    min_price: row.get(2)?,
                    max_price: row.get(3)?,
                    floor_price: row.get(4)?,
                    recommended_price: row.get(5)?,
                    margin_achieved: row.get(6)?,
                    method: row.get(7)?,
                    source: row.get(8)?,
                    notes: row.get(9)?,
                    created_at: row.get(10)?,
                })
            })?
            .collect::<DbResult<Vec<PricingDecision>>>()?;
        Ok(rows)
    }
}
