use std::collections::HashMap;

pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub query: HashMap<String, String>,
}

impl HttpRequest {
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            headers: HashMap::new(),
            query: HashMap::new(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&String> {
        self.headers.get(&name.to_lowercase())
    }

    pub fn path_segments(&self) -> Vec<&str> {
        self.path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect()
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    pub fn json(status: u16, body: &impl serde::Serialize) -> Result<Self, HttpError> {
        let json = serde_json::to_vec(body).map_err(|e| HttpError::Json(e.to_string()))?;
        let mut resp = Self::new(status);
        resp.headers
            .insert("content-type".to_string(), "application/json".to_string());
        resp.body = json;
        Ok(resp)
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        let mut resp = Self::new(status);
        resp.headers.insert(
            "content-type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        );
        resp.body = body.into().into_bytes();
        resp
    }

    pub fn redirect(status: u16, location: &str) -> Self {
        let mut resp = Self::new(status);
        resp.headers
            .insert("location".to_string(), location.to_string());
        resp
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .insert(name.into().to_lowercase(), value.into());
        self
    }

    pub fn with_cors(mut self) -> Self {
        self.headers
            .insert("access-control-allow-origin".to_string(), "*".to_string());
        self
    }
}

#[derive(Debug)]
pub enum HttpError {
    Json(String),
    Fetch(String),
    InvalidRequest(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(msg) => write!(f, "JSON error: {}", msg),
            Self::Fetch(msg) => write!(f, "Fetch error: {}", msg),
            Self::InvalidRequest(msg) => write!(f, "Invalid request: {}", msg),
        }
    }
}

impl std::error::Error for HttpError {}

#[cfg(feature = "server")]
pub async fn fetch_url(url: &str) -> Result<HttpResponse, HttpError> {
    use reqwest::Client;

    let client = Client::new();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| HttpError::Fetch(e.to_string()))?;

    let status = resp.status().as_u16();
    let headers: HashMap<String, String> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let body = resp
        .bytes()
        .await
        .map_err(|e| HttpError::Fetch(e.to_string()))?
        .to_vec();

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(not(feature = "server"))]
pub async fn fetch_url(_url: &str) -> Result<HttpResponse, HttpError> {
    Err(HttpError::Fetch(
        "fetch_url not available in this configuration".to_string(),
    ))
}
