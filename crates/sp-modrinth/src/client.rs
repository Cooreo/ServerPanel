//! The Modrinth API v2 client.
//!
//! Every read is cacheable and every write-free request is idempotent, so all of
//! them go through `sp_net` with a TTL cache keyed by the final URL: the panel polls
//! "available updates" from several screens, and un-cached polling is how a personal
//! panel gets itself rate-limited into an hour of dead buttons.

use crate::{
    upstream_err, ModrinthVersion, Notice, Project, ProjectType, SearchHit, Selection,
};
use serde_json::json;
use sp_core::CoreError;
use sp_net::cache::TtlCache;
use sp_net::{HttpClient, HttpClientBuilder};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_BASE: &str = "https://api.modrinth.com/v2";

/// One page of search results.
#[derive(Debug, Clone)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total: u64,
}

/// Query builder for `/search`. Facets are Modrinth's JSON-encoded array of arrays,
/// which is awkward enough to be worth encapsulating exactly once.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: Option<String>,
    pub project_type: Option<ProjectType>,
    pub categories: Vec<String>,
    pub loaders: Vec<String>,
    pub game_versions: Vec<String>,
    pub licenses: Vec<String>,
    pub index: SearchIndex,
    pub limit: Option<u8>,
    pub offset: Option<u32>,
    /// Hide client-only projects outright (`["server_side:required","server_side:optional"]`).
    pub server_side_only: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchIndex {
    #[default]
    Relevance,
    Downloads,
    Follows,
    Newest,
}

impl SearchIndex {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Downloads => "downloads",
            Self::Follows => "follows",
            Self::Newest => "newest",
        }
    }
}

impl SearchQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: Some(query.into()),
            ..Default::default()
        }
    }

    pub fn typed(mut self, t: ProjectType) -> Self {
        self.project_type = Some(t);
        self
    }

    pub fn for_mc(mut self, mc: impl Into<String>) -> Self {
        self.game_versions.push(mc.into());
        self
    }

    pub fn loader(mut self, l: impl Into<String>) -> Self {
        self.loaders.push(l.into());
        self
    }

    pub fn category(mut self, c: impl Into<String>) -> Self {
        self.categories.push(c.into());
        self
    }

    /// Build the facet JSON, e.g. `[["project_type:mod"],["versions:1.21.4"]]`.
    ///
    /// Each key is its own array (an OR within a facet), and facets are ANDed —
    /// getting this backwards returns an empty result set that looks like "no mods
    /// exist", which is the worst possible failure mode for a search box.
    pub fn facets_json(&self) -> String {
        let mut groups: Vec<Vec<String>> = Vec::new();
        if let Some(t) = self.project_type {
            let name = match t {
                ProjectType::Mod => "mod",
                ProjectType::Plugin => "plugin",
                ProjectType::ResourcePack => "resourcepack",
                ProjectType::Shader => "shader",
                ProjectType::Modpack => "modpack",
                ProjectType::Datapack => "datapack",
                ProjectType::Unknown => "mod",
            };
            groups.push(vec![format!("project_type:{name}")]);
        }
        if !self.categories.is_empty() {
            groups.push(self.categories.iter().map(|c| format!("categories:{c}")).collect());
        }
        if !self.loaders.is_empty() {
            groups.push(self.loaders.iter().map(|l| format!("categories:{l}")).collect());
        }
        if !self.game_versions.is_empty() {
            groups.push(self.game_versions.iter().map(|v| format!("versions:{v}")).collect());
        }
        if !self.licenses.is_empty() {
            groups.push(self.licenses.iter().map(|l| format!("licenses:{l}")).collect());
        }
        if self.server_side_only {
            groups.push(
                ["required", "optional"]
                    .iter()
                    .map(|s| format!("server_side:{s}"))
                    .collect(),
            );
        }
        let inner = groups
            .into_iter()
            .map(|g| {
                let items = g
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect::<Vec<_>>();
                serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("[{inner}]")
    }

    pub fn to_query_string(&self) -> String {
        let mut qs: Vec<String> = Vec::new();
        if let Some(q) = &self.query {
            qs.push(format!("query={}", urlencode(q)));
        }
        qs.push(format!("facets={}", urlencode(&self.facets_json())));
        qs.push(format!("index={}", self.index.as_str()));
        qs.push(format!("limit={}", self.limit.unwrap_or(20).min(100)));
        qs.push(format!("offset={}", self.offset.unwrap_or(0)));
        qs.join("&")
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ModrinthClient {
    http: HttpClient,
    cache: Arc<TtlCache>,
    ttl: Duration,
}

/// Raw response envelope for `/search`.
#[derive(Debug, serde::Deserialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
    #[serde(default)]
    total: Option<u64>,
}

impl ModrinthClient {
    pub fn new(cfg: &ModrinthConfig) -> Result<Self, CoreError> {
        Ok(Self {
            http: cfg.client()?,
            cache: Arc::new(TtlCache::with_capacity(512, cfg.ttl)),
            ttl: cfg.ttl,
        })
    }

    /// For tests: point at a loopback server.
    pub fn with_base(base: &str, ttl: Duration) -> Result<Self, CoreError> {
        Ok(Self {
            http: HttpClientBuilder::new()
                .service("modrinth")
                .base_url(base)
                .user_agent(concat!("ServerPanel/", env!("CARGO_PKG_VERSION")))
                .build_client()?,
            cache: Arc::new(TtlCache::with_capacity(64, ttl)),
            ttl,
        })
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    pub fn cache_stats(&self) -> sp_net::cache::CacheStats {
        self.cache.stats()
    }

    /// Cached GET returning text; the cache key is the full URL.
    async fn get_cached(&self, path: &str) -> Result<String, CoreError> {
        if let Some(hit) = self.cache.get(path) {
            tracing::trace!(path, "modrinth cache hit");
            return Ok(hit);
        }
        let body = self.http.get_text(path).await?;
        self.cache.insert(path.to_string(), body.clone());
        Ok(body)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, CoreError> {
        let body = self.get_cached(path).await?;
        serde_json::from_str(&body).map_err(|e| upstream_err(format!("unexpected shape at {path}: {e}"), 200))
    }

    pub async fn search(&self, q: &SearchQuery) -> Result<SearchPage, CoreError> {
        let path = format!("/search?{}", q.to_query_string());
        let resp: SearchResponse = self.get_json(&path).await?;
        Ok(SearchPage {
            hits: resp.hits,
            total: resp.total.unwrap_or(resp.hits.len() as u64),
        })
    }

    pub async fn project(&self, slug_or_id: &str) -> Result<Project, CoreError> {
        self.get_json(&format!("/project/{}", urlencode(slug_or_id))).await
    }

    /// Versions for a project, narrowed server-side when possible (a project with
    /// 400 versions is not something to filter in the browser).
    pub async fn versions_of(
        &self,
        slug_or_id: &str,
        sel: &Selection,
    ) -> Result<Vec<ModrinthVersion>, CoreError> {
        let mut path = format!("/project/{}/version", urlencode(slug_or_id));
        let mut filters: Vec<String> = Vec::new();
        if let Some(mc) = &sel.mc_version {
            filters.push(format!("game_versions={}", urlencode(&json!([mc]).to_string())));
        }
        if !sel.loaders.is_empty() {
            filters.push(format!("loaders={}", urlencode(&json!(sel.loaders).to_string())));
        }
        if !filters.is_empty() {
            path.push('?');
            path.push_str(&filters.join("&"));
        }
        self.get_json(&path).await
    }

    pub async fn version(&self, id: &str) -> Result<ModrinthVersion, CoreError> {
        self.get_json(&format!("/version/{}", urlencode(id))).await
    }

    /// Resolve many file hashes at once for update checks.
    ///
    /// Modrinth answers with a map-ish array; if the payload shape changes upstream
    /// this returns `Err` and the caller falls back to `versions_of` per project
    /// rather than reporting "no updates".
    pub async fn versions_for_hashes(
        &self,
        sha1_hex: &[String],
    ) -> Result<Vec<ModrinthVersion>, CoreError> {
        if sha1_hex.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!(sha1_hex);
        let text = self
            .http
            .post("/version_files", &body)
            .send()
            .await
            .map_err(|e| CoreError::UpstreamUnavailable {
                service: "modrinth",
                source: e.into(),
            })?;
        let status = text.status();
        let text = text.text().await.unwrap_or_default();
        if status.as_u16() == 404 {
            // The documented behaviour for "none of these hashes are known".
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(upstream_err(text.chars().take(300).collect::<String>(), status.as_u16()));
        }
        serde_json::from_str(&text).map_err(|e| {
            upstream_err(
                format!("unexpected /version_files shape: {e}"),
                status.as_u16(),
            )
        })
    }

    pub async fn notices(&self) -> Vec<Notice> {
        match self.get_json("/notices").await {
            Ok(n) => n,
            Err(e) => {
                // Notices are a courtesy banner; never fail a screen over them.
                tracing::debug!(error = %e, "could not load Modrinth notices");
                Vec::new()
            }
        }
    }

    pub async fn tags(&self, kind: TagKind) -> Result<Vec<Tag>, CoreError> {
        self.get_json(&format!("/tag/{}", kind.as_str())).await
    }

    /// Drop cached entries (the UI's refresh button).
    pub fn invalidate(&self, prefix: &str) {
        self.cache.invalidate_prefix(prefix);
    }

    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    Category,
    License,
    Loader,
    VersionType,
    ProjectType,
    Side,
}

impl TagKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Category => "category",
            Self::License => "license",
            Self::Loader => "loader",
            Self::VersionType => "version_type",
            Self::ProjectType => "project_type",
            Self::Side => "side",
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Tag {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModrinthConfig {
    pub base_url: String,
    pub ttl: Duration,
    pub user_agent: String,
}

impl Default for ModrinthConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE.to_string(),
            // 10 minutes: long enough that a busy UI does not hammer the API, short
            // enough that a mod author's re-upload shows up within a coffee break.
            ttl: Duration::from_secs(600),
            user_agent: concat!("ServerPanel/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

impl ModrinthConfig {
    pub fn client(&self) -> Result<HttpClient, CoreError> {
        HttpClientBuilder::new()
            .service("modrinth")
            .user_agent(self.user_agent.clone())
            .base_url(self.base_url.clone())
            .timeout(Duration::from_secs(20))
            // Modrinth answers 429 with a Retry-After; sp_net honours it, and one
            // blind retry is enough for a transient blip without risking a ban.
            .retry(sp_net::Retry::from_config(2, 1000))
            .build_client()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_json_is_an_array_of_arrays_with_the_right_grouping() {
        let q = SearchQuery::new("sodium")
            .typed(ProjectType::Mod)
            .for_mc("1.21.4")
            .loader("fabric")
            .category("technology");
        let facets = q.facets_json();
        let parsed: serde_json::Value = serde_json::from_str(&facets).unwrap();
        let groups = parsed.as_array().unwrap();
        assert_eq!(groups.len(), 4, "type, categories, loaders, versions");
        assert!(groups.iter().all(|g| g.is_array()), "each facet group must be an array");
        let flat: Vec<String> = groups
            .iter()
            .flat_map(|g| g.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()))
            .collect();
        assert!(flat.contains(&"project_type:mod".to_string()));
        assert!(flat.contains(&"versions:1.21.4".to_string()));
        assert!(flat.contains(&"categories:fabric".to_string()), "loaders are categories in this API");
        assert!(flat.contains(&"categories:technology".to_string()));
        // Empty query still produces valid JSON (the API rejects malformed facets).
        let empty = SearchQuery::default();
        assert_eq!(empty.facets_json(), "[]");
    }

    #[test]
    fn server_side_filter_hides_client_only_projects() {
        let q = SearchQuery {
            server_side_only: true,
            ..Default::default()
        };
        let f: serde_json::Value = serde_json::from_str(&q.facets_json()).unwrap();
        let group = f.as_array().unwrap().last().unwrap().as_array().unwrap().clone();
        let vals: Vec<&str> = group.iter().map(|s| s.as_str().unwrap()).collect();
        assert_eq!(vals, vec!["server_side:required", "server_side:optional"]);
    }

    #[test]
    fn query_string_is_encoded_and_capped() {
        let q = SearchQuery {
            query: Some("a b&c".into()),
            limit: Some(500),
            ..Default::default()
        };
        let qs = q.to_query_string();
        assert!(qs.contains("query=a%20b%26c"), "{qs}");
        assert!(qs.ends_with("limit=100"), "limit must clamp to what the API allows");
        assert!(qs.contains("facets=%5B%5D"), "the facet json must be url-encoded");
        assert!(qs.contains("index=relevance"));
        assert!(qs.contains("offset=0"));
        assert!(!qs.contains(' '), "raw spaces break a query string");
    }

    #[test]
    fn tag_kinds_and_indexes() {
        assert_eq!(TagKind::VersionType.as_str(), "version_type");
        assert_eq!(SearchIndex::Downloads.as_str(), "downloads");
        assert_eq!(SearchIndex::default(), SearchIndex::Relevance);
    }

    #[tokio::test]
    async fn search_and_project_parse_real_shapes() {
        let body = r#"{
          "hits":[{"project_id":"A1","project_type":"mod","slug":"sodium","title":"Sodium",
                   "description":"Renderer","categories":["fabric","performance"],
                   "downloads":20,"latest_version":"V1","date_modified":"2024-12-01T00:00:00Z"}],
          "total":1
        }"#;
        let routes = std::collections::BTreeMap::from([
            ("/search".to_string(), body.to_string()),
            (
                "/project/sodium".to_string(),
                r#"{"id":"A1","slug":"sodium","project_type":"mod","title":"Sodium","server_side":"optional","client_side":"required"}"#.to_string(),
            ),
            (
                "/project/A1/version".to_string(),
                format!(
                    "[{}]",
                    r#"{"id":"V1","project_id":"A1","name":"Sodium 0.6.0","version_number":"0.6.0",
                        "game_versions":["1.21.4"],"loaders":["fabric"],"version_type":"release",
                        "required_envs":{"client":"required","server":"optional"},
                        "files":[{"url":"https://cdn/x.jar","filename":"sodium-fabric-0.6.0.jar","primary":true,"size":10,"hashes":{"sha1":"ab"}}]}"#
                ),
            ),
            ("/notices".to_string(), r#"[{"id":"n1","msg":"read-only","severity":"info"}]"#.to_string()),
            ("/tag/loader".to_string(), r#"[{"id":"fabric","name":"Fabric"}]"#.to_string()),
        ]);
        let mock = MockServer::start(routes).await;
        let client = ModrinthClient::with_base(&mock.base(), Duration::from_secs(60)).unwrap();

        let page = client.search(&SearchQuery::new("sodium")).await.unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.total, 1);
        assert_eq!(page.hits[0].slug, "sodium");

        let p = client.project("sodium").await.unwrap();
        assert_eq!(p.id, "A1");

        let sel = Selection {
            mc_version: Some("1.21.4".into()),
            loaders: vec!["fabric".into()],
            ..Default::default()
        };
        let versions = client.versions_of("A1", &sel).await.unwrap();
        assert_eq!(versions[0].install_file().unwrap().filename, "sodium-fabric-0.6.0.jar");

        assert_eq!(client.notices().await.len(), 1);
        assert_eq!(client.tags(TagKind::Loader).await.unwrap()[0].name, "Fabric");

        // The second identical search must be served from cache, not from upstream.
        let before = mock.hits();
        let _ = client.search(&SearchQuery::new("sodium")).await.unwrap();
        assert_eq!(mock.hits(), before, "a cached search must not hit the network");
        assert!(client.cache_stats().hits >= 1);

        // Invalidating forces a refetch.
        client.invalidate("/search");
        let _ = client.search(&SearchQuery::new("sodium")).await.unwrap();
        assert!(mock.hits() > before);
    }

    #[tokio::test]
    async fn upstream_errors_are_named_and_shaped() {
        let routes = std::collections::BTreeMap::from([
            // 404 on a project must read as "not found", not "network down".
            ("/project/nope".to_string(), "".to_string()),
        ]);
        let mock = MockServer::start(routes).await;
        let client = ModrinthClient::with_base(&mock.base(), Duration::from_secs(1)).unwrap();
        let err = client.project("nope").await.unwrap_err();
        assert!(
            matches!(err, CoreError::Upstream { service, status, .. }
                if service == "modrinth" && status == 404),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn malformed_json_is_an_upstream_shape_error_not_a_panic() {
        let routes = std::collections::BTreeMap::from([("/notices".to_string(), "not json".to_string())]);
        let mock = MockServer::start(routes).await;
        let client = ModrinthClient::with_base(&mock.base(), Duration::from_secs(1)).unwrap();
        // notices swallow errors by design...
        assert!(client.notices().await.is_empty());
        // ...but a project does not.
        let err = client.project("x").await.unwrap_err();
        assert_eq!(err.code(), "upstream");
        assert!(err.to_string().contains("unexpected shape"), "{err}");
    }

    /// Minimal loopback origin: enough to exercise real HTTP, retries and parsing.
    struct MockServer {
        port: u16,
        hits: Arc<std::sync::atomic::AtomicUsize>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl MockServer {
        fn hits(&self) -> usize {
            self.hits.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn base(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
        async fn start(routes: std::collections::BTreeMap<String, String>) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let hits_bg = hits.clone();
            let routes = Arc::new(routes);
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else { return };
                    let routes = routes.clone();
                    let hits = hits_bg.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 8192];
                        let mut text = String::new();
                        while !text.contains("\r\n\r\n") {
                            match sock.read(&mut buf).await {
                                Ok(0) => return,
                                Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
                                Err(_) => return,
                            }
                        }
                        let path = text
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .split('?')
                            .next()
                            .unwrap_or("/")
                            .to_string();
                        hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let (status, body) = match routes.get(&path) {
                            Some(b) => ("200 OK", b.clone()),
                            None => ("404 Not Found", "{\"error\":\"no route\"}".to_string()),
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            });
            Self { port, hits, handle }
        }
    }
}
