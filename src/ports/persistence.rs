use std::collections::HashMap;

/// Port for persisting market records.
pub trait PersistencePort: Send + Sync {
    /// Queue a save (upsert) request. Returns immediately (non-blocking).
    fn save(&self, slug: &str, properties: HashMap<&str, &str>);
}
