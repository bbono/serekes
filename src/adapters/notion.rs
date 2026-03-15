use crate::ports::PersistencePort;
use log::{debug, error, warn};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub(crate) struct NotionRecord {
    pub slug: String,
    pub cost: f64,
    pub shares_up: f64,
    pub shares_down: f64,
}

const NOTION_API_BASE: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";

// ---------------------------------------------------------------------------
// NotionApi — shared HTTP client for Notion operations
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct NotionApi {
    client: Client,
    secret: String,
    database_id: String,
    bot_name: String,
}

impl NotionApi {
    pub fn new(secret: String, database_id: String, bot_name: String) -> Self {
        Self {
            client: Client::new(),
            secret,
            database_id,
            bot_name,
        }
    }

    /// Upsert a record by slug. Sets `updated` on every call, `created`, `bot`, and `market_url` only on insert.
    pub async fn save(&self, slug: &str, properties: &HashMap<String, String>) {
        let now = now_iso8601();
        let market_url = format!("https://polymarket.com/event/{}", slug);
        let mut props: HashMap<&str, &str> = properties
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        props.insert("Name", slug);
        props.insert("updated", &now);

        if let Some(id) = self.find_by_name(slug).await {
            let body = json!({ "properties": build_properties(&props) });
            if self
                .request("PATCH", &format!("{}/pages/{}", NOTION_API_BASE, id), &body)
                .await
                .is_some()
            {
                debug!("Notion record updated: {} (slug={})", id, slug);
            } else {
                warn!(
                    "Notion failed to update record: {} (slug={})",
                    id, slug
                );
            }
            return;
        }

        props.insert("created", &now);
        props.insert("bot", &self.bot_name);
        props.insert("market_url", &market_url);
        let body = json!({
            "parent": { "database_id": self.database_id },
            "properties": build_properties(&props),
        });
        match self
            .request("POST", &format!("{}/pages", NOTION_API_BASE), &body)
            .await
        {
            Some(resp) => {
                let id = resp["id"].as_str().unwrap_or_default();
                debug!("Notion record created: {} (slug={})", id, slug);
            }
            None => {
                warn!("Notion failed to create record (slug={})", slug);
            }
        }
    }

    /// Find own records by status. Filters by bot column automatically.
    pub async fn find_by_status(&self, status: &str) -> Option<Vec<NotionRecord>> {
        let body = json!({
            "filter": {
                "and": [
                    { "property": "status", "rich_text": { "equals": status } },
                    { "property": "bot", "rich_text": { "equals": self.bot_name } }
                ]
            },
        });

        let url = format!(
            "{}/databases/{}/query",
            NOTION_API_BASE, self.database_id
        );
        let resp = self.request("POST", &url, &body).await?;
        let results = resp["results"].as_array()?;

        let mut records = Vec::new();
        for page in results {
            let slug = page
                .pointer("/properties/Name/title/0/text/content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if slug.is_empty() {
                continue;
            }
            let cost = read_number(page, "cost").unwrap_or(0.0);
            let shares_up = read_number(page, "shares_up").unwrap_or(0.0);
            let shares_down = read_number(page, "shares_down").unwrap_or(0.0);
            records.push(NotionRecord {
                slug,
                cost,
                shares_up,
                shares_down,
            });
        }

        Some(records)
    }

    async fn find_by_name(&self, name: &str) -> Option<String> {
        let body = json!({
            "filter": {
                "and": [
                    { "property": "Name", "title": { "equals": name } },
                    { "property": "bot", "rich_text": { "equals": self.bot_name } }
                ]
            },
            "page_size": 1,
        });

        let url = format!(
            "{}/databases/{}/query",
            NOTION_API_BASE, self.database_id
        );
        let resp = self.request("POST", &url, &body).await?;
        let results = resp["results"].as_array()?;
        let page = results.first()?;
        page["id"].as_str().map(|s| s.to_string())
    }

    async fn request(&self, method: &str, url: &str, body: &Value) -> Option<Value> {
        let builder = match method {
            "POST" => self.client.post(url),
            "PATCH" => self.client.patch(url),
            _ => return None,
        };

        match builder
            .header("Authorization", format!("Bearer {}", self.secret))
            .header("Notion-Version", NOTION_VERSION)
            .json(body)
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    resp.json::<Value>().await.ok()
                } else {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    error!("Notion API error {}: {}", status, text);
                    None
                }
            }
            Err(e) => {
                error!("Notion request failed: {}", e);
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NotionAdapter — implements PersistencePort via non-blocking mpsc
// ---------------------------------------------------------------------------

struct SaveRequest {
    name: String,
    properties: HashMap<String, String>,
}

#[derive(Clone)]
pub struct NotionAdapter {
    tx: mpsc::UnboundedSender<SaveRequest>,
}

impl NotionAdapter {
    /// Spawn the Notion background worker. Returns an adapter for queueing saves.
    /// If secret or database_id is empty, returns a no-op handle that silently drops saves.
    /// Also returns the NotionApi for the resolver (if enabled).
    pub fn spawn(
        secret: Option<String>,
        database_id: &str,
        bot_name: &str,
    ) -> (Self, Option<NotionApi>) {
        let (tx, rx) = mpsc::unbounded_channel::<SaveRequest>();
        let bot_name = bot_name.to_string();

        let secret = match secret {
            Some(s) if !s.is_empty() && !database_id.is_empty() => s,
            _ => {
                warn!("Notion disabled (missing secret or database_id)");
                return (Self { tx }, None);
            }
        };

        let api = NotionApi::new(secret, database_id.to_string(), bot_name.clone());
        let resolver_api = api.clone();

        let worker_api = api;
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(req) = rx.recv().await {
                worker_api.save(&req.name, &req.properties).await;
            }
        });

        debug!("Notion background worker started");
        (Self { tx }, Some(resolver_api))
    }
}

impl PersistencePort for NotionAdapter {
    fn save(&self, slug: &str, properties: HashMap<&str, &str>) {
        let req = SaveRequest {
            name: slug.to_string(),
            properties: properties
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        if self.tx.send(req).is_err() {
            warn!("Notion channel closed, save dropped");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;

    let mut y = 1970i32;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while m < 12 && remaining >= month_days[m] as i64 {
        remaining -= month_days[m] as i64;
        m += 1;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m + 1,
        remaining + 1,
        hours,
        minutes,
        seconds
    )
}

fn build_properties(properties: &HashMap<&str, &str>) -> Value {
    let mut notion_props = serde_json::Map::new();

    for (key, value) in properties {
        let prop = match *key {
            "Name" => json!({
                "title": [{ "text": { "content": value } }]
            }),
            "created" | "updated" => json!({
                "date": { "start": value }
            }),
            "market_url" => json!({
                "url": value
            }),
            "pnl" | "cost" | "shares_up" | "shares_down" | "trades" => json!({
                "number": value.parse::<f64>().unwrap_or(0.0)
            }),
            _ => json!({
                "rich_text": [{ "text": { "content": value } }]
            }),
        };

        notion_props.insert(key.to_string(), prop);
    }

    Value::Object(notion_props)
}

fn read_number(page: &Value, property: &str) -> Option<f64> {
    page.pointer(&format!("/properties/{}/number", property))
        .and_then(|v| v.as_f64())
}
