use super::*;
use crate::model::config::Config;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};

struct LocalEndpoint(String);

impl KiroEndpoint for LocalEndpoint {
    fn name(&self) -> &'static str {
        "test"
    }
    fn api_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn mcp_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn decorate_api(
        &self,
        req: reqwest::RequestBuilder,
        ctx: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        req.header("x-test-account", ctx.credentials.id.unwrap().to_string())
    }
    fn decorate_mcp(
        &self,
        req: reqwest::RequestBuilder,
        ctx: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        self.decorate_api(req, ctx)
    }
    fn transform_api_body(&self, body: &str, _: &RequestContext<'_>) -> String {
        body.to_string()
    }
}

#[derive(Clone)]
struct MockState {
    calls: Arc<Mutex<Vec<String>>>,
    all_limited: bool,
}

async fn upstream(
    State(state): State<MockState>,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap, &'static str) {
    let id = headers["x-test-account"].to_str().unwrap().to_string();
    state.calls.lock().push(id.clone());
    if state.all_limited || id == "1" {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amzn-kiro-ratelimit-retry-after",
            "30000".parse().unwrap(),
        );
        (StatusCode::TOO_MANY_REQUESTS, headers, "limited")
    } else {
        (StatusCode::OK, HeaderMap::new(), "ok")
    }
}

#[tokio::test]
async fn rate_limit_failover_covers_api_stream_and_mcp() {
    for kind in ["api", "stream", "mcp"] {
        for all_limited in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let app = Router::new()
                .route("/", post(upstream))
                .with_state(MockState {
                    calls: calls.clone(),
                    all_limited,
                });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let credentials = (1..=2)
                .map(|id| KiroCredentials {
                    id: Some(id),
                    kiro_api_key: Some(format!("ksk_test_{id}")),
                    priority: id as u32,
                    ..KiroCredentials::default()
                })
                .collect();
            let manager = Arc::new(
                MultiTokenManager::new(Config::default(), credentials, None, None, false).unwrap(),
            );
            let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
            endpoints.insert("test".into(), Arc::new(LocalEndpoint(url)));
            let provider =
                KiroProvider::with_proxy(manager.clone(), None, endpoints, "test".into());
            let result = match kind {
                "api" => provider.call_api("{}").await.map(|call| call.response),
                "stream" => provider.call_api_stream("{}").await.map(|call| call.response),
                _ => provider.call_mcp("{}").await,
            };
            assert_eq!(
                *calls.lock(),
                vec!["1", "2"],
                "{kind}: must not retry cooling accounts"
            );
            if all_limited {
                assert!(result.err().unwrap().is::<CredentialsCoolingDown>());
                assert_eq!(manager.available_count(), 0);
            } else {
                assert_eq!(result.unwrap().text().await.unwrap(), "ok");
                assert_eq!(manager.available_count(), 1);
            }
            assert!(
                manager
                    .snapshot()
                    .entries
                    .iter()
                    .all(|e| !e.disabled && e.failure_count == 0)
            );
            server.abort();
        }
    }
}

#[test]
fn retry_after_headers_use_correct_units_and_precedence() {
    let mut headers = HeaderMap::new();
    assert_eq!(
        KiroProvider::rate_limit_delay(&headers),
        Duration::from_secs(60)
    );
    headers.insert("retry-after", "120".parse().unwrap());
    assert_eq!(
        KiroProvider::rate_limit_delay(&headers),
        Duration::from_secs(120)
    );
    headers.insert("x-amzn-kiro-ratelimit-retry-after", "1500".parse().unwrap());
    assert_eq!(
        KiroProvider::rate_limit_delay(&headers),
        Duration::from_millis(1500)
    );
    headers.insert("x-amzn-kiro-ratelimit-retry-after", "bad".parse().unwrap());
    assert_eq!(
        KiroProvider::rate_limit_delay(&headers),
        Duration::from_secs(120)
    );
    headers.insert("retry-after", "0".parse().unwrap());
    assert_eq!(
        KiroProvider::rate_limit_delay(&headers),
        Duration::from_secs(1)
    );
    headers.insert("retry-after", u64::MAX.to_string().parse().unwrap());
    assert_eq!(
        KiroProvider::rate_limit_delay(&headers),
        Duration::from_secs(86_400)
    );
    let future = chrono::Utc::now() + chrono::Duration::seconds(120);
    headers.insert("retry-after", future.to_rfc2822().parse().unwrap());
    let delay = KiroProvider::rate_limit_delay(&headers);
    assert!(delay > Duration::from_secs(118) && delay <= Duration::from_secs(120));
}
