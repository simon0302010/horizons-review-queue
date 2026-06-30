use std::collections::HashMap;
use std::sync::Arc;
use std::future::Future;
use std::time::{Duration, Instant};

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

const API_BASE: &str = "https://horizons.hackclub.com";
const HCA_AUTH_URL: &str = "https://auth.hackclub.com";
const CACHE_TTL_SECS: u64 = 60;
const SESSION_TTL_SECS: u64 = 86400 * 7; // 7 days

// ── Session ──

#[derive(Clone)]
struct UserSession {
    sub: String,
    slack_id: Option<String>,
    display_name: Option<String>,
    created_at: Instant,
}

// ── Horizons client ──

struct HorizonsClient {
    client: reqwest::Client,
    token: String,
    cached_stats: RwLock<Option<(Instant, serde_json::Value)>>,
    cached_queue: RwLock<Option<(Instant, serde_json::Value)>>,
    cached_past_reviews: RwLock<Option<(Instant, serde_json::Value)>>,
    cached_fraud_rejected: RwLock<Option<(Instant, serde_json::Value)>>,
    cached_user_projects: RwLock<HashMap<String, (Instant, Vec<serde_json::Value>)>>,
}

impl HorizonsClient {
    fn new() -> anyhow::Result<Self> {
        let token = std::env::var("HORIZONS_SESSION_ID")
            .map_err(|_| anyhow::anyhow!("HORIZONS_SESSION_ID env var not set"))?;
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to build client: {}", e))?,
            token,
            cached_stats: RwLock::new(None),
            cached_queue: RwLock::new(None),
            cached_past_reviews: RwLock::new(None),
            cached_fraud_rejected: RwLock::new(None),
            cached_user_projects: RwLock::new(HashMap::new()),
        })
    }

    fn cookie_val(&self) -> String {
        format!("sessionId={}", self.token)
    }

    async fn fetch_json(&self, path: &str) -> Result<serde_json::Value, anyhow::Error> {
        let url = format!("{}{}", API_BASE, path);
        let resp = self
            .client
            .get(&url)
            .header("Cookie", self.cookie_val())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            eprintln!("Horizons API error {} on {}: {}", status.as_u16(), path, body);
            anyhow::bail!("upstream request failed (status {})", status.as_u16());
        }
        Ok(resp.json().await?)
    }

    async fn get_or_cached<T, Fut>(
        cache: &RwLock<Option<(Instant, T)>>,
        fetch: impl FnOnce() -> Fut,
    ) -> Result<T, anyhow::Error>
    where
        T: Clone + Send + 'static,
        Fut: Future<Output = Result<T, anyhow::Error>> + Send,
    {
        {
            let guard = cache.read().await;
            if let Some((ts, data)) = guard.as_ref() {
                if ts.elapsed().as_secs() < CACHE_TTL_SECS {
                    return Ok(data.clone());
                }
            }
        }
        let data = fetch().await?;
        let mut guard = cache.write().await;
        *guard = Some((Instant::now(), data.clone()));
        Ok(data)
    }

    async fn get_stats(&self) -> Result<serde_json::Value, anyhow::Error> {
        Self::get_or_cached(&self.cached_stats, || self.fetch_json("/api/reviewer/stats")).await
    }

    async fn get_queue(&self) -> Result<serde_json::Value, anyhow::Error> {
        Self::get_or_cached(&self.cached_queue, || self.fetch_json("/api/reviewer/queue")).await
    }

    async fn get_past_reviews(&self) -> Result<serde_json::Value, anyhow::Error> {
        Self::get_or_cached(&self.cached_past_reviews, || {
            self.fetch_json("/api/reviewer/past-reviews")
        })
        .await
    }

    async fn get_fraud_rejected(&self) -> Result<serde_json::Value, anyhow::Error> {
        Self::get_or_cached(&self.cached_fraud_rejected, || {
            self.fetch_json("/api/reviewer/fraud-rejected")
        })
        .await
    }

    async fn compute_event_stats(&self) -> Result<serde_json::Value, anyhow::Error> {
        use std::collections::BTreeMap;

        let past_res = self.get_past_reviews().await;

        let past_reviews = match &past_res {
            Ok(p) => p["reviews"].as_array().cloned().unwrap_or_default(),
            Err(_) => vec![],
        };

        let mut by_project: BTreeMap<u64, (String, serde_json::Value)> = BTreeMap::new();

        for r in &past_reviews {
            let approved = r["reviewPassed"].as_bool().unwrap_or(false)
                && r["approvalStatus"].as_str().unwrap_or("") == "approved";
            if !approved {
                continue;
            }
            let pid = match r["projectId"].as_u64() {
                Some(id) => id,
                None => continue,
            };
            let reviewed_at = r["reviewedAt"].as_str().unwrap_or("");
            let should_replace = by_project
                .get(&pid)
                .map(|(ts, _)| reviewed_at > ts.as_str())
                .unwrap_or(true);
            if should_replace {
                by_project.insert(pid, (reviewed_at.to_string(), r.clone()));
            }
        }

        let mut by_event: BTreeMap<String, serde_json::Map<String, serde_json::Value>> = BTreeMap::new();

        for (_, (_, item)) in &by_project {
            let user = &item["user"];
            let slug = user["eventSlug"].as_str().unwrap_or("").to_string();
            let entry = by_event.entry(slug.clone()).or_insert_with(|| {
                let mut m = serde_json::Map::new();
                m.insert("slug".into(), serde_json::json!(slug));
                m.insert("title".into(), serde_json::json!("Other"));
                m.insert("approvedProjects".into(), serde_json::json!(0));
                m.insert("approvedHours".into(), serde_json::json!(0.0));
                m
            });

            if !slug.is_empty() {
                if let Some(t) = user["eventTitle"].as_str().filter(|t| !t.is_empty()) {
                    entry.insert("title".into(), serde_json::json!(t));
                }
            }

            let hours = item["approvedHours"].as_f64().unwrap_or(0.0);
            let prev_hours = entry["approvedHours"].as_f64().unwrap_or(0.0);
            entry.insert("approvedHours".into(), serde_json::json!(
                (prev_hours * 100.0 + hours * 100.0).round() / 100.0
            ));
            entry.insert("approvedProjects".into(), serde_json::json!(
                entry["approvedProjects"].as_i64().unwrap_or(0) + 1
            ));
        }

        let out: Vec<serde_json::Value> = by_event
            .into_values()
            .map(serde_json::Value::Object)
            .collect();

        Ok(serde_json::json!({ "events": out }))
    }

    /// Find all projects for a user across queue, past_reviews, and fraud_rejected
    async fn find_user_projects(
        &self,
        slack_id: &str,
    ) -> Result<Vec<serde_json::Value>, anyhow::Error> {
        // Check per-user cache
        {
            let guard = self.cached_user_projects.read().await;
            if let Some((ts, data)) = guard.get(slack_id) {
                if ts.elapsed().as_secs() < CACHE_TTL_SECS {
                    return Ok(data.clone());
                }
            }
        }

        let (q, pr, fr) = tokio::join!(
            self.get_queue(),
            self.get_past_reviews(),
            self.get_fraud_rejected(),
        );

        let mut results = Vec::new();

        // Queue items: project.user.slackUserId, project.joeFraudPassed
        // Track 1-based position among Normal Review items (joeFraudPassed == true).
        if let Ok(queue) = &q {
            let empty = vec![];
            let queue_arr = queue.as_array().unwrap_or(&empty);
            let mut normal_review_pos = 0;
            for item in queue_arr {
                let jfp = &item["project"]["joeFraudPassed"];
                let is_normal = !jfp.is_null() && jfp.as_bool().unwrap_or(false);
                if is_normal {
                    normal_review_pos += 1;
                    if item["project"]["user"]["slackUserId"].as_str() == Some(slack_id) {
                        results.push(self.normalize_queue_item(item, normal_review_pos));
                    }
                } else {
                    if item["project"]["user"]["slackUserId"].as_str() == Some(slack_id) {
                        results.push(self.normalize_queue_item(item, 0)); // 0 means not in normal review queue
                    }
                }
            }
        }

        // Past reviews: user.slackUserId, has approvalStatus
        if let Ok(past) = &pr {
            let empty = vec![];
            let reviews = past["reviews"].as_array().unwrap_or(&empty);
            for item in reviews {
                if item["user"]["slackUserId"].as_str() == Some(slack_id) {
                    results.push(self.normalize_review_item(item, "past"));
                }
            }
        }

        // Fraud rejected: user.slackUserId
        if let Ok(fraud) = &fr {
            let empty = vec![];
            for item in fraud.as_array().unwrap_or(&empty) {
                if item["user"]["slackUserId"].as_str() == Some(slack_id) {
                    results.push(self.normalize_review_item(item, "fraud_rejected"));
                }
            }
        }

        // Deduplicate by projectId, keeping the latest submission
        {
            let mut seen: HashMap<u64, usize> = HashMap::new();
            let mut deduped: Vec<serde_json::Value> = Vec::new();
            for item in &results {
                let pid = item["projectId"].as_u64();
                let Some(pid) = pid else {
                    deduped.push(item.clone());
                    continue;
                };
                let curr_ts = item["reviewedAt"].as_str()
                    .or_else(|| item["createdAt"].as_str())
                    .unwrap_or("");
                if let Some(&prev_idx) = seen.get(&pid) {
                    let prev_ts = deduped[prev_idx]["reviewedAt"].as_str()
                        .or_else(|| deduped[prev_idx]["createdAt"].as_str())
                        .unwrap_or("");
                    if curr_ts > prev_ts {
                        deduped[prev_idx] = item.clone();
                    }
                } else {
                    seen.insert(pid, deduped.len());
                    deduped.push(item.clone());
                }
            }
            results = deduped;
        }

        // Cache per user
        {
            let mut guard = self.cached_user_projects.write().await;
            guard.insert(slack_id.to_string(), (Instant::now(), results.clone()));
        }

        Ok(results)
    }

    /// Fetch display_name from flaron API
    async fn get_display_name(&self, slack_id: &str) -> Option<String> {
        let url = format!("https://flaron.halceon.dev/user/{}", slack_id);
        let resp = self.client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: serde_json::Value = resp.json().await.ok()?;
        Some(data["data"]["user"]["display_name"].as_str()?.to_string())
    }

    /// Gather all unique users from queue, past_reviews, and fraud_rejected
    async fn get_all_users(&self) -> Result<Vec<serde_json::Value>, anyhow::Error> {
        let (q, pr, fr) = tokio::join!(
            self.get_queue(),
            self.get_past_reviews(),
            self.get_fraud_rejected(),
        );

        let mut seen: Vec<String> = Vec::new();

        if let Ok(queue) = &q {
            let empty = vec![];
            for item in queue.as_array().unwrap_or(&empty) {
                if let Some(sid) = item["project"]["user"]["slackUserId"].as_str() {
                    if !seen.contains(&sid.to_string()) {
                        seen.push(sid.to_string());
                    }
                }
            }
        }
        if let Ok(past) = &pr {
            let empty = vec![];
            for item in past["reviews"].as_array().unwrap_or(&empty) {
                if let Some(sid) = item["user"]["slackUserId"].as_str() {
                    if !seen.contains(&sid.to_string()) {
                        seen.push(sid.to_string());
                    }
                }
            }
        }
        if let Ok(fraud) = &fr {
            let empty = vec![];
            for item in fraud.as_array().unwrap_or(&empty) {
                if let Some(sid) = item["user"]["slackUserId"].as_str() {
                    if !seen.contains(&sid.to_string()) {
                        seen.push(sid.to_string());
                    }
                }
            }
        }

        let mut users = Vec::with_capacity(seen.len());
        for sid in &seen {
            let display_name = self.get_display_name(sid).await;
            users.push(serde_json::json!({
                "slack_id": sid,
                "display_name": display_name.unwrap_or_else(|| sid.clone()),
            }));
        }

        users.sort_by(|a, b| {
            a["display_name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["display_name"].as_str().unwrap_or(""))
        });

        Ok(users)
    }

    fn normalize_queue_item(&self, item: &serde_json::Value, queue_pos: usize) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("projectId".into(), item["projectId"].clone());
        // Queue items nest title/type under item["project"]
        out.insert("projectTitle".into(), item["project"]["projectTitle"].clone());
        out.insert("projectType".into(), item["project"]["projectType"].clone());
        out.insert("createdAt".into(), item["createdAt"].clone());
        out.insert("submissionId".into(), item["submissionId"].clone());
        out.insert("source".into(), serde_json::json!("queue"));
        out.insert("status".into(), serde_json::json!("pending"));
        // Overall 1-based position in the queue (oldest = #1)
        out.insert("queuePosition".into(), serde_json::json!(queue_pos));

        let jfp = &item["project"]["joeFraudPassed"];
        let stage = if jfp.is_null() {
            "Not Started"
        } else if jfp.as_bool().unwrap_or(false) {
            "Normal Review"
        } else {
            "Fraud Review"
        };
        out.insert("reviewStage".into(), serde_json::json!(stage));

        // Claim: show whether someone has it open — NOT who.
        // A stale claim (heartbeat timed out) is treated as unclaimed.
        let claimed = !item["claim"].is_null()
            && !item["claim"]["isStale"].as_bool().unwrap_or(true);
        out.insert("claimed".into(), serde_json::json!(claimed));

        serde_json::Value::Object(out)
    }

    fn normalize_review_item(&self, item: &serde_json::Value, source: &str) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("projectId".into(), item["projectId"].clone());
        out.insert("projectTitle".into(), item["projectTitle"].clone());
        out.insert("projectType".into(), item["projectType"].clone());
        out.insert("createdAt".into(), item["createdAt"].clone());
        out.insert("submissionId".into(), item["submissionId"].clone());
        out.insert("source".into(), serde_json::json!(source));

        if source == "fraud_rejected" {
            out.insert("status".into(), serde_json::json!("rejected"));
            out.insert("reviewStage".into(), serde_json::json!("Fraud Rejected"));
        } else {
            let approval = item["approvalStatus"].as_str().unwrap_or("finalized");
            out.insert("status".into(), serde_json::json!(approval));
            out.insert("reviewStage".into(), serde_json::json!("Reviewed"));
            out.insert("reviewedAt".into(), item["reviewedAt"].clone());
            out.insert("reviewPassed".into(), item["reviewPassed"].clone());
            out.insert("approvalStatus".into(), item["approvalStatus"].clone());
        }
        serde_json::Value::Object(out)
    }
}

// ── App state ──

struct PendingState {
    email: Option<String>,
    referral_code: Option<String>,
    code_verifier: String,
    created: Instant,
}

struct AppState {
    client: HorizonsClient,
    hca_client_id: String,
    hca_client_secret: String,
    hca_redirect_uri: String,
    sessions: RwLock<HashMap<String, UserSession>>,
    pending_states: RwLock<HashMap<String, PendingState>>,
    rate_limiter: RwLock<HashMap<String, Vec<Instant>>>,
    debug: bool,
    cached_users: RwLock<Option<(Instant, Vec<serde_json::Value>)>>,
}

// ── Stats endpoint ──

#[derive(Serialize)]
struct PendingCounts {
    total_pending: i64,
    fraud_review_pending: i64,
    normal_review_pending: i64,
    just_fraud_review_pending: i64,
    just_normal_review_pending: i64,
}

fn compute_pending(matrix: &serde_json::Value) -> PendingCounts {
    let ra = &matrix["reviewApproved"];
    let rp = &matrix["reviewPending"];

    let ra_fraud_pending = ra["fraudPending"].as_i64().unwrap_or(0);
    let rp_fraud_passed = rp["fraudPassed"].as_i64().unwrap_or(0);
    let rp_fraud_pending = rp["fraudPending"].as_i64().unwrap_or(0);

    PendingCounts {
        total_pending: ra_fraud_pending + rp_fraud_passed + rp_fraud_pending,
        fraud_review_pending: ra_fraud_pending + rp_fraud_pending,
        normal_review_pending: rp_fraud_passed + rp_fraud_pending,
        just_fraud_review_pending: ra_fraud_pending,
        just_normal_review_pending: rp_fraud_passed,
    }
}

async fn handle_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.client.get_stats().await {
        Ok(stats) => {
            let funnel = &stats["reviewProjects"]["funnelMatrix"];
            let counts = compute_pending(funnel);
            Json(counts).into_response()
        }
        Err(e) => {
            let err = serde_json::json!({"error": e.to_string()});
            (StatusCode::BAD_GATEWAY, Json(err)).into_response()
        }
    }
}

// ── Auth helpers ──

fn generate_nonce() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.r#gen();
    hex::encode(bytes)
}

fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 32] = rng.r#gen();
    hex::encode(bytes)
}

fn generate_pkce_pair() -> (String, String) {
    use rand::distributions::{Alphanumeric, DistString};
    let mut rng = rand::thread_rng();
    let verifier: String = Alphanumeric.sample_string(&mut rng, 64);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64url_encode(&hash);
    (verifier, challenge)
}

fn base64url_encode(input: &[u8]) -> String {
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        out.push(table[((triple >> 18) & 0x3F) as usize] as char);
        out.push(table[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(table[((triple >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(table[(triple & 0x3F) as usize] as char);
        }
    }
    out
}

fn get_session_id(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for c in cookie.split(';') {
        let c = c.trim();
        if let Some(val) = c.strip_prefix("sessionId=") {
            return Some(val.to_string());
        }
    }
    None
}

fn client_ip(headers: &HeaderMap) -> String {
    if let Some(val) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next().map(|s| s.trim().to_string()))
    {
        return val;
    }
    if let Some(val) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        return val.to_string();
    }
    "unknown".into()
}

async fn check_rate_limit(
    state: &AppState,
    key: &str,
    max_requests: usize,
    window: Duration,
) -> bool {
    let mut limiter = state.rate_limiter.write().await;
    let now = Instant::now();
    let entries = limiter.entry(key.to_string()).or_default();
    entries.retain(|t| now.duration_since(*t) < window);
    if entries.len() >= max_requests {
        return false;
    }
    entries.push(now);
    true
}

// ── Auth endpoints ──

#[derive(Deserialize)]
struct LoginQuery {
    email: Option<String>,
    referral_code: Option<String>,
}

async fn handle_auth_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<LoginQuery>,
) -> impl IntoResponse {
    let ip = client_ip(&headers);
    if !check_rate_limit(&state, &format!("login:{}", ip), 10, Duration::from_secs(60)).await {
        return (StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({"error": "rate limit exceeded"})))
            .into_response();
    }

    let nonce = generate_nonce();
    let (code_verifier, code_challenge) = generate_pkce_pair();
    {
        let mut pending = state.pending_states.write().await;
        pending.insert(nonce.clone(), PendingState {
            email: q.email.clone(),
            referral_code: q.referral_code.clone(),
            code_verifier,
            created: Instant::now(),
        });
    }

    let mut params = Vec::new();
    params.push(("client_id", state.hca_client_id.as_str()));
    params.push(("redirect_uri", state.hca_redirect_uri.as_str()));
    params.push(("response_type", "code"));
    params.push(("scope", "openid slack_id"));
    params.push(("state", &nonce));
    params.push(("code_challenge", &code_challenge));
    params.push(("code_challenge_method", "S256"));

    if let Some(ref email) = q.email {
        params.push(("login_hint", email.as_str()));
    }

    let url = format!(
        "{}/oauth/authorize?{}",
        HCA_AUTH_URL,
        params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
            .collect::<Vec<_>>()
            .join("&")
    );

    Json(serde_json::json!({ "url": url })).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

async fn handle_auth_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> impl IntoResponse {
    let ip = client_ip(&headers);
    if !check_rate_limit(&state, &format!("callback:{}", ip), 10, Duration::from_secs(60)).await {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }

    // Look up pending state by nonce
    let pending = {
        let mut pending_states = state.pending_states.write().await;
        pending_states.remove(&q.state)
    };
    let pending = match pending {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "Invalid state").into_response(),
    };

    // Check expiry (10 min)
    if pending.created.elapsed().as_secs() > 600 {
        return (StatusCode::BAD_REQUEST, "State expired").into_response();
    }

    // Exchange code for tokens
    let token_resp = match state
        .client
        .client
        .post(format!("{}/oauth/token", HCA_AUTH_URL))
        .form(&[
            ("client_id", state.hca_client_id.as_str()),
            ("client_secret", state.hca_client_secret.as_str()),
            ("redirect_uri", state.hca_redirect_uri.as_str()),
            ("code", q.code.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", &pending.code_verifier),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("Token exchange failed: {}", e))
                .into_response();
        }
    };

    if !token_resp.status().is_success() {
        let body = token_resp.text().await.unwrap_or_default();
        eprintln!("Token exchange error: {}", body);
        return (StatusCode::BAD_GATEWAY, "Token exchange error").into_response();
    }

    let tokens: serde_json::Value = match token_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Token parse error: {}", e),
            )
                .into_response();
        }
    };

    let access_token = match tokens["access_token"].as_str() {
        Some(s) => s,
        None => {
            return (StatusCode::BAD_GATEWAY, "No access_token").into_response();
        }
    };

    // Fetch userinfo
    let userinfo_resp = match state
        .client
        .client
        .get(format!("{}/oauth/userinfo", HCA_AUTH_URL))
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("Userinfo failed: {}", e)).into_response();
        }
    };

    if !userinfo_resp.status().is_success() {
        return (StatusCode::BAD_GATEWAY, "Userinfo error").into_response();
    }

    let userinfo: serde_json::Value = match userinfo_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("Userinfo parse: {}", e)).into_response();
        }
    };

    let sub = userinfo["sub"].as_str().unwrap_or("").to_string();
    let slack_id = userinfo["slack_id"].as_str().map(|s| s.to_string());

    let display_name = if let Some(ref sid) = slack_id {
        state.client.get_display_name(sid).await
    } else {
        None
    };

    let session_id = generate_session_id();
    let session = UserSession {
        sub: sub.clone(),
        slack_id,
        display_name,
        created_at: Instant::now(),
    };

    {
        let mut sessions = state.sessions.write().await;
        sessions.insert(session_id.clone(), session);
    }

    // Set cookie + redirect
    let cookie = format!(
        "sessionId={}; HttpOnly; Path=/; Max-Age={}; SameSite=Lax",
        session_id, SESSION_TTL_SECS
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        cookie.parse().unwrap(),
    );
    headers.insert(
        header::LOCATION,
        "/".parse().unwrap(),
    );

    (StatusCode::FOUND, headers).into_response()
}

async fn handle_auth_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let sid = match get_session_id(&headers) {
        Some(s) => s,
        None => {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "no session"})))
                .into_response();
        }
    };

    let mut sessions = state.sessions.write().await;
    match sessions.get(&sid) {
        Some(session) => {
            if session.created_at.elapsed().as_secs() > SESSION_TTL_SECS {
                sessions.remove(&sid);
                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "session expired"})))
                    .into_response();
            }
            Json(serde_json::json!({
                "sub": session.sub,
                "slack_id": session.slack_id,
                "display_name": session.display_name,
                "debug": state.debug,
            }))
            .into_response()
        }
        None => (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid session"})))
            .into_response(),
    }
}

async fn handle_auth_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(sid) = get_session_id(&headers) {
        let mut sessions = state.sessions.write().await;
        sessions.remove(&sid);
    }

    let cookie = "sessionId=; HttpOnly; Path=/; Max-Age=0; SameSite=Lax";
    let mut h = HeaderMap::new();
    h.insert(header::SET_COOKIE, cookie.parse().unwrap());
    (h, Json(serde_json::json!({"ok": true})))
}

// ── Dashboard HTML ──

async fn handle_my_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let slack_id = if state.debug {
        if let Some(imp) = params.get("impersonate") {
            imp.clone()
        } else {
            let sid = match get_session_id(&headers) {
                Some(s) => s,
                None => {
                    return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "no session"})))
                        .into_response();
                }
            };
            let sessions = state.sessions.read().await;
            let session = match sessions.get(&sid) {
                Some(s) => s,
                None => {
                    return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid session"})))
                        .into_response();
                }
            };
            match session.slack_id.clone() {
                Some(id) => id,
                None => {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "no slack_id"})))
                        .into_response();
                }
            }
        }
    } else {
        let sid = match get_session_id(&headers) {
            Some(s) => s,
            None => {
                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "no session"})))
                    .into_response();
            }
        };
        let sessions = state.sessions.read().await;
        let session = match sessions.get(&sid) {
            Some(s) => s,
            None => {
                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid session"})))
                    .into_response();
            }
        };
        match session.slack_id.clone() {
            Some(id) => id,
            None => {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "no slack_id"})))
                    .into_response();
            }
        }
    };

    let projects = match state.client.find_user_projects(&slack_id).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let mut out: Vec<serde_json::Value> = projects
        .into_iter()
        .map(|p| {
            let mut m = p.as_object().cloned().unwrap_or_default();
            let created = m.get("createdAt")
                .and_then(|c| c.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            m.insert("sortKey".into(), serde_json::json!(created));
            serde_json::Value::Object(m)
        })
        .collect();

    out.sort_by(|a, b| {
        b["sortKey"]
            .as_str()
            .unwrap_or("")
            .cmp(a["sortKey"].as_str().unwrap_or(""))
    });

    for item in &mut out {
        if let Some(obj) = item.as_object_mut() {
            obj.remove("sortKey");
        }
    }

    Json(out).into_response()
}

async fn handle_debug_users(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if !state.debug {
        return StatusCode::NOT_FOUND.into_response();
    }
    {
        let guard = state.cached_users.read().await;
        if let Some((ts, data)) = &*guard {
            if ts.elapsed().as_secs() < CACHE_TTL_SECS {
                return Json(data).into_response();
            }
        }
    }
    match state.client.get_all_users().await {
        Ok(users) => {
            let mut guard = state.cached_users.write().await;
            guard.replace((Instant::now(), users.clone()));
            Json(users).into_response()
        }
        Err(e) => {
            (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response()
        }
    }
}

async fn handle_events(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.client.compute_event_stats().await {
        Ok(data) => Json(data).into_response(),
        Err(e) => {
            eprintln!("compute_event_stats error: {}", e);
            let err = serde_json::json!({"error": "failed to compute event stats"});
            (StatusCode::BAD_GATEWAY, Json(err)).into_response()
        }
    }
}

// ── Dashboard HTML ──

async fn handle_dashboard() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html"),
    )
}

async fn handle_style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/style.css"),
    )
}

async fn handle_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        include_str!("../static/script.js"),
    )
}

// ── URL encoding helper ──

fn urlencoding(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

// ── Main ──

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let debug = std::env::args().any(|a| a == "--debug");

    let hca_client_id =
        std::env::var("HCA_CLIENT_ID").map_err(|_| anyhow::anyhow!("HCA_CLIENT_ID not set"))?;
    let hca_client_secret = std::env::var("HCA_CLIENT_SECRET")
        .map_err(|_| anyhow::anyhow!("HCA_CLIENT_SECRET not set"))?;
    let hca_redirect_uri = std::env::var("HCA_REDIRECT_URI")
        .map_err(|_| anyhow::anyhow!("HCA_REDIRECT_URI not set"))?;

    let client = HorizonsClient::new()?;
    let state = Arc::new(AppState {
        client,
        hca_client_id,
        hca_client_secret,
        hca_redirect_uri,
        sessions: RwLock::new(HashMap::new()),
        pending_states: RwLock::new(HashMap::new()),
        rate_limiter: RwLock::new(HashMap::new()),
        debug,
        cached_users: RwLock::new(None),
    });

    // Periodic cleanup of expired pending states and sessions
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let mut pending = cleanup_state.pending_states.write().await;
            pending.retain(|_, ps| ps.created.elapsed().as_secs() < 600);
            drop(pending);

            let mut sessions = cleanup_state.sessions.write().await;
            sessions.retain(|_, s| s.created_at.elapsed().as_secs() < SESSION_TTL_SECS);

            let mut limiter = cleanup_state.rate_limiter.write().await;
            limiter.retain(|_, entries| {
                entries.retain(|t| t.elapsed().as_secs() < 60);
                !entries.is_empty()
            });
        }
    });

    let app = Router::new()
        .route("/", get(handle_dashboard))
        .route("/style.css", get(handle_style))
        .route("/script.js", get(handle_script))
        .route("/api/stats", get(handle_stats))
        .route("/api/auth/login", get(handle_auth_login))
        .route("/api/auth/callback", get(handle_auth_callback))
        .route("/api/auth/me", get(handle_auth_me))
        .route("/api/auth/logout", get(handle_auth_logout))
        .route("/api/my/projects", get(handle_my_projects))
        .route("/api/debug/users", get(handle_debug_users))
        .route("/api/events", get(handle_events))
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3001".into());
    let addr = format!("0.0.0.0:{}", port);
    println!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
