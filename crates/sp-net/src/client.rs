//! [`HttpClient`]: user agent, timeouts, and retry/backoff policy.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, USER_AGENT};
use sp_core::{user_agent, CoreError};
use std::time::Duration;

/// Retry policy for idempotent GETs. POSTs are never retried here (a duplicate
/// Modrinth download is harmless, a duplicate GitHub event is not).
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: u32,
    pub base: Duration,
    /// Exponential factor, clamped so `attempts` cannot overflow into a sleep of
    /// hours: total worst-case wait is `base * (2^n - 1)`.
    pub factor: f64,
}

impl Default for Retry {
    fn default() -> Self {
        Self { attempts: 4, base: Duration::from_millis(500), factor: 2.0 }
    }
}

impl Retry {
    pub const fn none() -> Self {
        Self { attempts: 1, base: Duration::from_millis(0), factor: 1.0 }
    }
    pub const fn from_config(attempts: u32, base_ms: u64) -> Self {
        Self { attempts: attempts.max(1), base: Duration::from_millis(base_ms), factor: 2.0 }
    }
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exp = 2u32.pow(attempt.min(6));
        self.base * exp
    }
    /// Statuses worth retrying: rate limits and transient 5xx.
    pub fn is_retryable_status(status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status.is_server_error()
    }
}

#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    retry: Retry,
    service_hint: &'static str,
    /// Optional prefix so callers can say `"/v2/projects/paper"`.
    /// Two things need it: mirrors/self-hosted caches, and tests that point a
    /// provider at a loopback mock instead of the real API.
    base_url: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct HttpClientBuilder {
    ua: Option<String>,
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    retry: Option<Retry>,
    service: Option<&'static str>,
    base_url: Option<String>,
    proxy: Option<String>,
    default_headers: Vec<(String, String)>,
}

impl HttpClientBuilder {
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.ua = Some(ua.into());
        self
    }
    /// Total transfer timeout, per attempt (not per retry sequence).
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = Some(t);
        self
    }
    pub fn retry(mut self, r: Retry) -> Self {
        self.retry = Some(r);
        self
    }
    /// Label used in error messages (`"modrinth"`, `"mojang"`, …).
    pub fn service(mut self, s: &'static str) -> Self {
        self.service = Some(s);
        self
    }
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.default_headers.push((name.into(), value.into()));
        self
    }
    /// Prefix for relative request paths, e.g. `https://mirror.example/paper-mirror`.
    pub fn base_url(mut self, base: impl Into<String>) -> Self {
        let b = base.into();
        if !b.is_empty() {
            self.base_url = Some(b);
        }
        self
    }

    /// `http://host:port` or `socks5://host:port`.
    pub fn proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = Some(url.into());
        self
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Finalise the builder. Kept separate from `new()` so `build_client()` reads
    /// as the fallible step in calling code.
    pub fn build_client(&self) -> Result<HttpClient, CoreError> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json, */*"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(self.ua.as_deref().unwrap_or(&user_agent()))
                .map_err(|e| CoreError::other(format!("invalid user agent: {e}")))?,
        );
        for (k, v) in &self.default_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| CoreError::other(format!("invalid header name {k}: {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| CoreError::other(format!("invalid header value for {k}: {e}")))?;
            headers.insert(name, value);
        }
        let mut b = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(self.connect_timeout.unwrap_or(Duration::from_secs(20)))
            .tcp_nodelay(true)
            // Redirects are how Mojang's launcher services work (they hand out a
            // signed blob URL), so follow them — but cap it.
            .redirect(reqwest::redirect::Policy::limited(10))
            .use_rustls_tls();
        if let Some(t) = self.timeout {
            b = b.timeout(t);
        } else {
            b = b.timeout(Duration::from_secs(120));
        }
        if let Some(p) = &self.proxy {
            let proxy = reqwest::Proxy::all(p).map_err(|e| CoreError::other(format!("bad proxy: {e}")))?;
            b = b.proxy(proxy);
        }
        let inner = b.build().map_err(|e| CoreError::other(format!("http client: {e}")))?;
        Ok(HttpClient {
            inner,
            retry: self.retry.unwrap_or_default(),
            service_hint: self.service.unwrap_or("upstream"),
            base_url: self.base_url.clone(),
        })
    }
}

impl HttpClient {
    /// Production client with the default retry policy.
    pub fn new() -> Self {
        HttpClientBuilder::default().build_client().expect("default headers are valid")
    }

    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.inner
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn service(&self) -> &'static str {
        self.service_hint
    }

    /// Join `url` onto the configured base when it is relative.
    pub fn resolve(&self, url: &str) -> String {
        match &self.base_url {
            Some(base)
                if !url.starts_with("http://") && !url.starts_with("https://") =>
            {
                format!("{}/{}", base.trim_end_matches('/'), url.trim_start_matches('/'))
            }
            _ => url.to_string(),
        }
    }

    pub fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.inner.get(self.resolve(url))
    }

    pub fn post(&self, url: &str) -> reqwest::RequestBuilder {
        self.inner.post(self.resolve(url))
    }

    pub fn head(&self, url: &str) -> reqwest::RequestBuilder {
        self.inner.head(self.resolve(url))
    }

    /// `GET` → `T`, retried. Errors distinguish "not found" from "unreachable" so
    /// the UI can say "no builds published for that version" instead of "network".
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, CoreError> {
        let text = self.get_text(url).await?;
        serde_json::from_str(&text).map_err(|e| CoreError::Upstream {
            service: self.service_hint,
            status: 200,
            body: format!("unexpected response shape: {e}"),
        })
    }

    pub async fn get_text(&self, url: &str) -> Result<String, CoreError> {
        self.send_retrying(self.get(url)).await?.text().await.map_err(|e| {
            CoreError::UpstreamUnavailable {
                service: self.service_hint,
                source: e.into(),
            }
        })
    }

    /// Like [`HttpClient::get_text`] but also returns response headers, so
    /// callers can read `X-Ratelimit-*` / `ETag` / `Cache-Control`.
    pub async fn get_text_with_headers(&self, url: &str) -> Result<(String, HeaderMap), CoreError> {
        let resp = self.send_retrying(self.get(url)).await?;
        let headers = resp.headers().clone();
        let body = resp.text().await.map_err(|e| CoreError::UpstreamUnavailable {
            service: self.service_hint,
            source: e.into(),
        })?;
        Ok((body, headers))
    }

    pub async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, CoreError> {
        let resp = self
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| CoreError::UpstreamUnavailable {
                service: self.service_hint,
                source: e.into(),
            })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(CoreError::Upstream {
                service: self.service_hint,
                status: status.as_u16(),
                body: text.chars().take(400).collect(),
            });
        }
        resp.json().await.map_err(|e| CoreError::Upstream {
            service: self.service_hint,
            status: 200,
            body: format!("unexpected response shape: {e}"),
        })
    }

    /// Send with retries applied to the *request*, returning a successful response.
    pub async fn send_retrying(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::Response, CoreError> {
        let mut attempt = 0u32;
        loop {
            // `try_clone` lets us resend the same request after a failure.
            let Some(b) = builder.try_clone() else {
                return Err(CoreError::other("request was not retryable (body already consumed)"));
            };
            match b.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }
                    if Retry::is_retryable_status(status) && attempt + 1 < self.retry.attempts {
                        let delay = retry_after(resp.headers()).unwrap_or_else(|| self.retry.delay_for(attempt));
                        attempt += 1;
                        tracing::warn!(%status, attempt, ?delay, "retrying request");
                        drop(resp);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    let body = resp.text().await.unwrap_or_default();
                    return Err(if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        CoreError::RateLimited {
                            service: self.service_hint,
                            retry_after_secs: retry_after_from_text(&body)
                                .or_else(|| Some(self.retry.delay_for(attempt).as_secs()))
                                .unwrap_or(60),
                        }
                    } else {
                        CoreError::Upstream {
                            service: self.service_hint,
                            status: status.as_u16(),
                            body: body.chars().take(400).collect(),
                        }
                    });
                }
                Err(e) => {
                    if attempt + 1 >= self.retry.attempts || !e.is_connect() && !e.is_request() && !e.is_timeout() && !e.is_body() {
                        return Err(CoreError::UpstreamUnavailable {
                            service: self.service_hint,
                            source: e.into(),
                        });
                    }
                    let delay = self.retry.delay_for(attempt);
                    attempt += 1;
                    tracing::warn!(error = %e, attempt, ?delay, "network error, retrying");
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// `Retry-After` in seconds, if present and parseable.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Some services (Modrinth included) put the number in the body instead.
fn retry_after_from_text(body: &str) -> Option<u64> {
    let digits: String = body
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u64>().ok().filter(|v| (1..3600).contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delays_grow_and_clamp() {
        let r = Retry::default();
        assert_eq!(r.delay_for(0), Duration::from_millis(500));
        assert_eq!(r.delay_for(1), Duration::from_millis(1000));
        assert_eq!(r.delay_for(2), Duration::from_millis(2000));
        assert_eq!(r.delay_for(32), Duration::from_millis(500) * 64, "clamped at 2^6");
        assert_eq!(Retry::none().attempts, 1);
    }

    #[test]
    fn retryable_status_set() {
        assert!(Retry::is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(Retry::is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(Retry::is_retryable_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!Retry::is_retryable_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!Retry::is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
    }

    #[test]
    fn retry_after_parsing() {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("12"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(12)));
        assert_eq!(retry_after(&HeaderMap::new()), None);
        h.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2025"));
        assert_eq!(retry_after(&h), None, "HTTP-dates are not parsed, we fall back to backoff");
        assert_eq!(retry_after_from_text("Please wait 45 seconds"), Some(45));
        assert_eq!(retry_after_from_text("nope"), None);
        assert_eq!(retry_after_from_text("9999999"), None, "absurd values rejected");
    }

    #[test]
    fn user_agent_is_descriptive() {
        let ua = user_agent();
        assert!(ua.starts_with("ServerPanel/"), "{ua}");
        assert!(ua.contains("https://"), "{ua}");
    }

    #[test]
    fn builder_sets_headers() {
        let c = HttpClientBuilder::new()
            .service("example")
            .user_agent("TestAgent/1.0")
            .header("x-test", "1")
            .retry(Retry::none())
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(1))
            .build_client()
            .unwrap();
        let headers = c.raw().headers();
        assert_eq!(headers.get(USER_AGENT).unwrap().to_str().unwrap(), "TestAgent/1.0");
        assert_eq!(headers.get("x-test").unwrap().to_str().unwrap(), "1");
        // relative URL resolution proves the client is usable without a base
        assert!(c.get("https://example.com").build().is_ok());
    }

    #[test]
    fn proxy_is_validated() {
        assert!(HttpClientBuilder::new().proxy("not a url").build_client().is_err());
        assert!(HttpClientBuilder::new().proxy("http://127.0.0.1:8080").build_client().is_ok());
    }
}
