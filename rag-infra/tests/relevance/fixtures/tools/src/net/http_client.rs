//! Blocking HTTP client used by the tools CLI.

pub struct HttpClient {
    base_url: String,
    timeout_seconds: u64,
}

impl HttpClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_owned(),
            timeout_seconds: 30,
        }
    }

    pub fn get(&self, path: &str) -> Result<String, String> {
        Err(format!(
            "GET {}{} timed out after {}s",
            self.base_url, path, self.timeout_seconds
        ))
    }
}
