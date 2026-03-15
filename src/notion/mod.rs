use log::{debug, error, warn};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;

const NOTION_API_BASE: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";

/// Message sent through the channel to the background worker.
struct SaveRequest {
    name: String,
    properties: HashMap<String, String>,
}

/// Handle for saving records to Notion. Calls are non-blocking —
/// requests are queued and processed by a background task.
#[derive(Clone)]
pub struct Notion {
    tx: mpsc::UnboundedSender<SaveRequest>,
    bot_name: String,
}

impl Notion {
    /// Queue a save (upsert) request. The `market` parameter is the market slug.
    /// The Notion "Name" (title) is set to `bot_name+market`. Returns immediately.
    pub fn save(&self, market: &str, properties: HashMap<&str, &str>) {
        let req = SaveRequest {
            name: format!("{}::{}", self.bot_name, market),
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

/// Spawn the Notion background worker. Returns a handle for queueing saves.
/// If secret or database_id is empty, returns a no-op handle that silently drops saves.
pub fn spawn(secret: Option<String>, database_id: &str, bot_name: &str) -> Notion {
    let (tx, rx) = mpsc::unbounded_channel::<SaveRequest>();
    let bot_name = bot_name.to_string();

    let secret = match secret {
        Some(s) if !s.is_empty() && !database_id.is_empty() => s,
        _ => {
            warn!("Notion disabled (missing secret or database_id)");
            return Notion { tx, bot_name };
        }
    };

    let database_id = database_id.to_string();

    tokio::spawn(async move {
        let worker = NotionWorker {
            client: Client::new(),
            secret,
            database_id,
        };
        worker.run(rx).await;
    });

    debug!("Notion background worker started");
    Notion { tx, bot_name }
}

struct NotionWorker {
    client: Client,
    secret: String,
    database_id: String,
}

impl NotionWorker {
    async fn run(self, mut rx: mpsc::UnboundedReceiver<SaveRequest>) {
        while let Some(req) = rx.recv().await {
            self.save(&req.name, &req.properties).await;
        }
    }

    async fn save(&self, name: &str, properties: &HashMap<String, String>) {
        let now = now_iso8601();
        let slug = name.split_once("::").map(|(_, s)| s).unwrap_or(name);
        let market_url = format!("https://polymarket.com/event/{}", slug);
        let mut props: HashMap<&str, &str> = properties
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        props.insert("Name", name);
        props.insert("updated", &now);

        let is_update = if let Some(id) = self.find_by_name(name).await {
            let notion_properties = build_properties(&props);
            let body = json!({ "properties": notion_properties });
            if self
                .request("PATCH", &format!("{}/pages/{}", NOTION_API_BASE, id), &body)
                .await
                .is_some()
            {
                debug!("Notion record updated: {} (name={})", id, name);
            } else {
                warn!("Notion failed to update record: {} (name={})", id, name);
            }
            true
        } else {
            false
        };

        if !is_update {
            props.insert("created", &now);
            props.insert("market_url", &market_url);
            let notion_properties = build_properties(&props);
            let body = json!({
                "parent": { "database_id": self.database_id },
                "properties": notion_properties,
            });
            match self
                .request("POST", &format!("{}/pages", NOTION_API_BASE), &body)
                .await
            {
                Some(resp) => {
                    let id = resp["id"].as_str().unwrap_or_default();
                    debug!("Notion record created: {} (name={})", id, name);
                }
                None => {
                    warn!("Notion failed to create record (name={})", name);
                }
            }
        }
    }

    async fn find_by_name(&self, name: &str) -> Option<String> {
        let body = json!({
            "filter": {
                "property": "Name",
                "title": { "equals": name }
            },
            "page_size": 1,
        });

        let url = format!("{}/databases/{}/query", NOTION_API_BASE, self.database_id);
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

fn now_iso8601() -> String {
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

    // Days since 1970-01-01 to Y-M-D
    let mut y = 1970i32;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining < days_in_year { break; }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    while m < 12 && remaining >= month_days[m] as i64 {
        remaining -= month_days[m] as i64;
        m += 1;
    }
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m + 1, remaining + 1, hours, minutes, seconds)
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
            _ => json!({
                "rich_text": [{ "text": { "content": value } }]
            }),
        };

        notion_props.insert(key.to_string(), prop);
    }

    Value::Object(notion_props)
}
