use std::time::Duration;

#[derive(Debug, Clone)]
pub struct WebhookCfg {
    pub url: String,
    pub hmac_key: Option<String>,
    pub timeout: Duration,
    pub events: Vec<String>,
}

impl WebhookCfg {
    pub fn allows(&self, ev: &str) -> bool {
        if self.events.is_empty() {
            return true;
        }
        self.events.iter().any(|e| e == ev)
    }
}
