use std::collections::HashMap;
use std::sync::Arc;
use std::future::Future;
use std::time::{Duration, Instant};

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json},
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
) -> impl IntoResponse {
    let sid = match get_session_id(&headers) {
        Some(s) => s,
        None => {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "no session"})))
                .into_response();
        }
    };

    let session = {
        let mut sessions = state.sessions.write().await;
        let entry = sessions.get(&sid).cloned();
        if let Some(ref s) = entry {
            if s.created_at.elapsed().as_secs() > SESSION_TTL_SECS {
                sessions.remove(&sid);
                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "session expired"})))
                    .into_response();
            }
        }
        entry
    };
    let session = match session {
        Some(s) => s,
        None => {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid session"})))
                .into_response();
        }
    };

    let slack_id = match session.slack_id {
        Some(ref id) => id.clone(),
        None => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "no slack_id"})))
                .into_response();
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

async fn handle_dashboard() -> Html<&'static str> {
    Html(HTML)
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
        .route("/api/stats", get(handle_stats))
        .route("/api/auth/login", get(handle_auth_login))
        .route("/api/auth/callback", get(handle_auth_callback))
        .route("/api/auth/me", get(handle_auth_me))
        .route("/api/auth/logout", get(handle_auth_logout))
        .route("/api/my/projects", get(handle_my_projects))
        .route("/api/events", get(handle_events))
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3001".into());
    let addr = format!("0.0.0.0:{}", port);
    println!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

const HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Horizons — Dashboard</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: 'Bricolage Grotesque', 'DM Sans', system-ui, -apple-system, sans-serif;
    background: #f3e8d8;
    color: #000;
    min-height: 100vh;
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 32px 40px 0;
  }
  .head-left {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  /* ── Pipeline card ── */
  .page {
    padding: 20px 40px;
    max-width: 780px;
    margin: 0 auto;
  }
  .card {
    background: #f3e8d8;
    border: 4px solid #000;
    border-radius: 20px;
    box-shadow: 4px 4px 0 0 #000;
    padding: 28px 32px;
  }
  .card-title {
    font-size: 20px;
    font-weight: 700;
    margin-bottom: 20px;
    font-family: 'Bricolage Grotesque', sans-serif;
  }

  .total-row {
    display: flex;
    align-items: baseline;
    gap: 8px;
    margin-bottom: 16px;
    padding-bottom: 16px;
    border-bottom: 2px solid #000;
  }
  .total-row .num {
    font-size: 44px;
    font-weight: 700;
    line-height: 1;
    font-family: 'DM Sans', sans-serif;
  }
  .total-row .desc {
    font-size: 16px;
    font-weight: 600;
    color: #444;
  }

  .bar-wrap {
    display: flex;
    height: 52px;
    border-radius: 10px;
    overflow: hidden;
    border: 2px solid #000;
    margin-bottom: 12px;
  }
  .bar-seg {
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 16px;
    font-weight: 700;
    color: #fff;
    transition: width 0.5s ease;
    text-shadow: 0 1px 3px rgba(0,0,0,0.5);
    position: relative;
  }
  .bar-seg:not(:last-child)::after {
    content: '';
    position: absolute;
    right: 0;
    top: 0;
    bottom: 0;
    width: 2px;
    background: rgba(0,0,0,0.2);
  }
  .bar-seg span { position: relative; z-index: 1; }

  .legend {
    display: flex;
    gap: 28px;
    flex-wrap: wrap;
  }
  .legend-item {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    font-weight: 600;
  }
  .dot {
    width: 12px;
    height: 12px;
    border-radius: 4px;
    border: 1.5px solid rgba(0,0,0,0.3);
    flex-shrink: 0;
  }

  .skel-bar {
    height: 52px;
    background: #ddd;
    border-radius: 10px;
    border: 2px solid #000;
    animation: pulse 1.5s infinite;
  }
  .skel-total {
    height: 44px;
    width: 100px;
    background: #ddd;
    border-radius: 8px;
    animation: pulse 1.5s infinite;
  }
  @keyframes pulse {
    0%, 100% { opacity: 0.4; }
    50% { opacity: 0.8; }
  }

  /* ── Login button / user info ── */
  .auth-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .hca-btn {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 20px;
    border: 3px solid #000;
    border-radius: 12px;
    background: #ec3750;
    color: #fff;
    font-family: 'Bricolage Grotesque', sans-serif;
    font-size: 15px;
    font-weight: 700;
    cursor: pointer;
    transition: transform 0.15s ease;
    box-shadow: 3px 3px 0 0 #000;
    text-decoration: none;
  }
  .hca-btn:hover { transform: scale(1.03); }
  .hca-btn:active { transform: scale(0.97); box-shadow: 1px 1px 0 0 #000; }

  .user-badge {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 16px;
    border: 3px solid #000;
    border-radius: 12px;
    background: #f3e8d8;
    box-shadow: 3px 3px 0 0 #000;
    font-weight: 600;
    font-size: 14px;
  }
  .logout-btn {
    background: none;
    border: 2px solid #000;
    border-radius: 8px;
    padding: 6px 12px;
    font-family: 'Bricolage Grotesque', sans-serif;
    font-weight: 600;
    font-size: 12px;
    cursor: pointer;
    transition: transform 0.15s ease;
  }
  .logout-btn:hover { transform: scale(1.05); }

  /* ── Projects island ── */
  .island {
    margin-top: 20px;
    background: #f3e8d8;
    border: 4px solid #000;
    border-radius: 20px;
    box-shadow: 4px 4px 0 0 #000;
    padding: 0;
    max-height: 0;
    overflow: hidden;
    transition: max-height 0.4s ease, padding 0.3s ease;
  }
  .island.open {
    max-height: 2000px;
    padding: 24px 28px;
    overflow-y: auto;
  }
  .island-title {
    font-size: 18px;
    font-weight: 700;
    font-family: 'Bricolage Grotesque', sans-serif;
    margin-bottom: 16px;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .project-item {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    padding: 12px 16px;
    border: 2px solid #000;
    border-radius: 12px;
    margin-bottom: 8px;
    background: #fff8ee;
    gap: 12px;
  }
  .project-item:last-child { margin-bottom: 0; }
  .project-info { min-width: 0; flex: 1; }
  .project-title {
    font-weight: 700;
    font-size: 14px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .project-meta {
    font-size: 11px;
    color: #666;
    font-weight: 500;
    margin-top: 2px;
  }
  .project-status {
    flex-shrink: 0;
    display: flex;
    flex-direction: row;
    align-items: center;
    justify-content: flex-end;
    gap: 4px;
  }
  .project-status-feedback {
    width: 100%;
    text-align: right;
  }
  .badge {
    display: inline-block;
    padding: 3px 10px;
    border-radius: 6px;
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    border: 2px solid #000;
  }
  .badge-pending    { background: #facc15; color: #000; }
  .badge-approved   { background: #ffa936; color: #000; }
  .badge-rejected   { background: #e05632; color: #fff; }
  .badge-in-review  { background: #ab47bc; color: #fff; }
  .badge-fraud      { background: #ef5350; color: #fff; }
  .badge-finalized  { background: #42a5f5; color: #fff; }
  .badge-unsubmitted { background: #d1d5db; color: #000; }
  /* Status colors for Fraud/Review badges */
  .badge-status-approved { background: #22c55e; color: #fff; }
  .badge-status-pending  { background: #facc15; color: #000; }
  .badge-status-rejected { background: #e05632; color: #fff; }
  .badge-queue-pos {
    background: #000;
    color: #f3e8d8;
    font-variant-numeric: tabular-nums;
    letter-spacing: 0.03em;
  }
  .badge-claimed {
    background: #f3e8d8;
    color: #000;
    border-style: dashed;
  }
  .badge-claimed::before {
    content: '●';
    font-size: 8px;
    margin-right: 5px;
    color: #f5a623;
    vertical-align: middle;
  }
  .feedback {
    font-size: 12px;
    color: #444;
    font-style: italic;
    max-width: 200px;
    text-align: right;
  }
  .island-empty {
    padding: 24px;
    text-align: center;
    color: #666;
    font-weight: 600;
  }
  .island-loading {
    padding: 24px;
    text-align: center;
    color: #666;
  }
  .island-error {
    padding: 16px;
    text-align: center;
    color: #c62828;
    font-weight: 600;
  }

  /* ── Event blobs ── */
  .events-card {
    margin-top: 20px;
  }
  .events-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
    gap: 12px;
  }
  .event-blob {
    display: flex;
    flex-direction: column;
    align-items: center;
    min-width: 120px;
    padding: 16px 20px 14px;
    border: 3px solid #000;
    border-radius: 20px;
    background: #fff8ee;
    box-shadow: 3px 3px 0 0 #000;
    gap: 2px;
  }
  .event-blob-name {
    font-family: 'Bricolage Grotesque', sans-serif;
    font-weight: 700;
    font-size: 13px;
    text-align: center;
    line-height: 1.2;
    margin-bottom: 4px;
  }
  .event-stat {
    display: flex;
    flex-direction: column;
    align-items: center;
    line-height: 1;
  }
  .event-stat + .event-stat {
    margin-top: 4px;
  }
  .event-stat-num {
    font-size: 22px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
  }
  .event-stat-num-sm {
    font-size: 16px;
    color: #444;
  }
  .event-stat-label {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    color: #666;
    letter-spacing: 0.05em;
    margin-top: 1px;
  }
  .events-skel {
    height: 16px;
    background: #ddd;
    border-radius: 6px;
    animation: pulse 1.5s infinite;
    margin-bottom: 8px;
  }

  @media (max-width: 600px) {
    .head { padding: 20px 16px 0; flex-wrap: wrap; gap: 12px; }
    .page { padding: 16px; }
    .card { padding: 20px; }
    .total-row .num { font-size: 32px; }
    .bar-wrap { height: 42px; }
    .bar-seg { font-size: 13px; }
    .island.open { padding: 16px; }
    .project-item { flex-direction: column; align-items: flex-start; }
    .project-status { justify-content: flex-start; max-width: 100%; width: 100%; flex-wrap: wrap; }
    .project-status-feedback { text-align: left; }
    .feedback { max-width: none; text-align: left; }
  }
</style>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link href="https://fonts.googleapis.com/css2?family=Bricolage+Grotesque:wght@400;600;700&family=DM+Sans:wght@400;500;600;700&display=swap" rel="stylesheet">
</head>
<body>

<div class="head">
  <div class="head-left">
    <svg viewBox="0 0 460 56" width="460" height="90" fill="#000">
      <text font-family="'Bricolage Grotesque',sans-serif" font-weight="800" font-size="38" y="40">HORIZONS</text>
      <text font-family="'DM Sans',sans-serif" font-weight="500" font-size="18" y="40" x="195" fill="#888">dashboard</text>
    </svg>
  </div>
  <div class="auth-row" id="auth-row">
    <div id="user-area"></div>
    <a id="login-btn" class="hca-btn" href="#">
      <svg viewBox="0 0 20 20" width="18" height="18" fill="none">
        <path d="M3 2h14a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1zm2 3v10h3V5H5zm5 0v10h3V5h-3z" fill="#fff"/>
        <rect x="2" y="2" width="16" height="16" rx="2" stroke="#fff" stroke-width="1.5" fill="none"/>
        <path d="M5 5h3v3H5V5zm5 0h3v3h-3V5z" fill="#fff"/>
      </svg>
      Sign in with Hack Club
    </a>
  </div>
</div>

<div class="page">
  <div class="card">
    <div class="card-title">Review Pipeline</div>

    <div id="skel">
      <div class="total-row">
        <div class="skel-total"></div>
        <div class="desc">pending</div>
      </div>
      <div class="skel-bar"></div>
    </div>

    <div id="chart" style="display:none">
      <div class="total-row">
        <span class="num" id="total-num"></span>
        <span class="desc">projects pending</span>
      </div>
      <div class="bar-wrap">
        <div class="bar-seg" id="seg-jf" style="background:#f5a623;width:0%"><span></span></div>
        <div class="bar-seg" id="seg-b"  style="background:#ab47bc;width:0%"><span></span></div>
        <div class="bar-seg" id="seg-jr" style="background:#42a5f5;width:0%"><span></span></div>
      </div>
      <div class="legend">
        <div class="legend-item"><span class="dot" style="background:#f5a623"></span> Just fraud review</div>
        <div class="legend-item"><span class="dot" style="background:#ab47bc"></span> Both</div>
        <div class="legend-item"><span class="dot" style="background:#42a5f5"></span> Just regular review</div>
      </div>
    </div>
  </div>

  <!-- Projects island -->
  <div class="island open" id="island">
    <div class="island-title">
      My Submitted Projects
    </div>
    <div id="projects-content">
      <div class="island-empty">Log in to see your projects review status.</div>
    </div>
  </div>

  <!-- Events card -->
  <div class="card events-card">
    <div class="card-title">Event Approved Hours</div>
    <div id="events-skel">
      <div class="events-skel"></div>
      <div class="events-skel" style="width:80%"></div>
      <div class="events-skel" style="width:60%"></div>
    </div>
    <div id="events-content" style="display:none"></div>
  </div>
</div>

<script>
// ── Auth state ──
let currentUser = null;

async function checkAuth() {
  try {
    const r = await fetch('/api/auth/me');
    if (r.ok) {
      currentUser = await r.json();
      renderUser();
      loadMyProjects();
    } else {
      currentUser = null;
      renderUser();
    }
  } catch { currentUser = null; renderUser(); }
}

function renderUser() {
  const area = document.getElementById('user-area');
  const btn = document.getElementById('login-btn');
  if (currentUser) {
    btn.style.display = 'none';
    area.innerHTML = `
      <div class="user-badge">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>
        ${escHtml(currentUser.display_name || currentUser.sub)}
        <button class="logout-btn" onclick="logout()">Logout</button>
      </div>`;
  } else {
    btn.style.display = '';
    area.innerHTML = '';
  }
}

async function logout() {
  await fetch('/api/auth/logout');
  currentUser = null;
  renderUser();
  document.getElementById('projects-content').innerHTML = '<div class="island-empty">Log in to see your projects review status.</div>';
}

// ── Login button ──
document.getElementById('login-btn').addEventListener('click', async (e) => {
  e.preventDefault();
  try {
    const r = await fetch('/api/auth/login');
    const d = await r.json();
    if (d.url) window.location.href = d.url;
  } catch (err) { console.error('Login failed', err); }
});

// ── My projects ──
async function loadMyProjects() {
  const island = document.getElementById('island');
  const content = document.getElementById('projects-content');

  if (!currentUser) {
    content.innerHTML = '<div class="island-empty">Log in to see your projects review status.</div>';
    return;
  }

  island.classList.add('open');
  content.innerHTML = '<div class="island-loading">Loading your projects...</div>';

  try {
    const r = await fetch('/api/my/projects');
    if (!r.ok) throw new Error(await r.text());
    const projects = await r.json();

    if (!projects.length) {
      content.innerHTML = '<div class="island-empty">No projects found</div>';
      return;
    }

      content.innerHTML = projects.map(p => {
        const isQueue = p.source === 'queue';
        const isPending = p.status === 'pending';
        const meta = p.projectType ? p.projectType.replace(/_/g, ' ') : '';

        let mainBadges;
        if (isPending) {
          // 3 permanent badges if in review (overall status is pending)
          const fraudStatus = getFraudStatus(p);
          const reviewStatus = getReviewStatus(p);
          
          const fraudClass = getBadgeColorClass(fraudStatus);
          const reviewClass = getBadgeColorClass(reviewStatus);
          
          const queuePos = (p.queuePosition != null && p.queuePosition > 0)
            ? `<span class="badge badge-queue-pos">#${p.queuePosition} in queue</span>` : '';
          const claimed = (isQueue && p.claimed)
            ? `<span class="badge badge-claimed">Being reviewed</span>` : '';
            
          mainBadges = `
            <span class="badge ${fraudClass}">Fraud ${fraudStatus}</span>
            <span class="badge ${reviewClass}">Review ${reviewStatus}</span>
            ${queuePos}
            ${claimed}`;
        } else {
          // Fully approved or rejected projects
          const badgeClass  = statusBadgeClass(p.status);
          const badgeLabel  = statusLabel(p.status, p.reviewStage);
          mainBadges = `<span class="badge ${badgeClass}">${badgeLabel}</span>`;
        }

        return `<div class="project-item">
          <div class="project-info">
            <div class="project-title">${escHtml(p.projectTitle || '(untitled)')}</div>
            <div class="project-meta">${escHtml(meta)}</div>
          </div>
          <div class="project-status">
            ${mainBadges}
          </div>
        </div>`;
      }).join('');
  } catch (e) {
    content.innerHTML = `<div class="island-error">Failed to load: ${escHtml(e.message)}</div>`;
  }
}

function getFraudStatus(p) {
  if (p.source === 'fraud_rejected' || p.reviewStage === 'Fraud Rejected') {
    return 'Rejected';
  }
  if (p.source === 'queue') {
    if (p.joeFraudPassed === true) return 'Approved';
    return 'Pending';
  }
  if (p.source === 'past') {
    if (p.approvalStatus === 'approved' || p.approvalStatus === 'finalized') {
      return 'Approved';
    }
    if (p.approvalStatus === 'rejected') {
      // If reviewer approved it but overall it is rejected, fraud rejected it
      if (p.reviewPassed === true) return 'Rejected';
      // If reviewer rejected it, fraud probably approved/passed it
      return 'Approved';
    }
    if (p.approvalStatus === 'pending') {
      return 'Pending';
    }
  }
  return 'Pending';
}

function getReviewStatus(p) {
  if (p.status === 'approved' || p.status === 'finalized') {
    return 'Approved';
  }
  if (p.source === 'queue') {
    return 'Pending';
  }
  if (p.source === 'past') {
    if (p.reviewPassed === true) return 'Approved';
    if (p.reviewPassed === false) return 'Rejected';
    if (p.reviewPassed === null || p.reviewPassed === undefined) {
      if (p.approvalStatus === 'approved') return 'Approved';
      if (p.approvalStatus === 'rejected') return 'Rejected';
    }
  }
  return 'Pending';
}

function getBadgeColorClass(status) {
  switch (status) {
    case 'Approved': return 'badge-status-approved';
    case 'Rejected': return 'badge-status-rejected';
    case 'Pending':  return 'badge-status-pending';
    default:         return 'badge-unsubmitted';
  }
}

function statusBadgeClass(status) {
  switch (status) {
    case 'pending':       return 'badge-pending';
    case 'approved':      return 'badge-approved';
    case 'rejected':      return 'badge-rejected';
    case 'in_review':     return 'badge-in-review';
    case 'finalized':     return 'badge-finalized';
    case 'unsubmitted':   return 'badge-unsubmitted';
    default:              return 'badge-unsubmitted';
  }
}

function statusLabel(status, reviewStage) {
  if (reviewStage === 'Fraud Rejected') return 'Fraud Rejected';
  if (reviewStage === 'Not Started')    return 'In Queue';
  if (reviewStage === 'Fraud Review')   return 'Fraud Review';
  if (reviewStage === 'Normal Review')  return 'In Review';
  switch (status) {
    case 'pending':     return 'Pending';
    case 'approved':    return 'Approved';
    case 'rejected':    return 'Rejected';
    case 'in_review':   return 'In Review';
    case 'finalized':   return 'Finalized';
    case 'unsubmitted': return 'Unsubmitted';
    default:            return status ? status.replace(/_/g, ' ') : 'Unknown';
  }
}

function escHtml(s) {
  const d = document.createElement('div');
  d.textContent = s || '';
  return d.innerHTML;
}

// ── Event blobs ──
async function loadEvents() {
  const skel = document.getElementById('events-skel');
  const cont = document.getElementById('events-content');
  try {
    const r = await fetch('/api/events');
    if (!r.ok) throw new Error(await r.text());
    const data = await r.json();
    if (data.error) throw new Error(data.error);
    if (!data.events || !data.events.length) {
      skel.style.display = 'none';
      cont.style.display = '';
      cont.innerHTML = '<div class="island-empty">No approved projects found.</div>';
      return;
    }
    skel.style.display = 'none';
    cont.style.display = '';
    cont.innerHTML = '<div class="events-grid">' +
      data.events.map(e => `<div class="event-blob">
        <div class="event-blob-name">${escHtml(e.title)}</div>
        <div class="event-stat">
          <div class="event-stat-num">${e.approvedProjects}</div>
          <div class="event-stat-label">projects</div>
        </div>
        <div class="event-stat">
          <div class="event-stat-num event-stat-num-sm">${Math.round(e.approvedHours)}</div>
          <div class="event-stat-label">hours</div>
        </div>
      </div>`).join('') +
      '</div>';
  } catch (e) {
    console.error('Failed to load events:', e);
    skel.style.display = '';
    cont.style.display = 'none';
  }
}

// ── Pipeline chart ──
async function loadStats() {
  const skel = document.getElementById('skel');
  const chart = document.getElementById('chart');
  try {
    const r = await fetch('/api/stats');
    if (!r.ok) throw new Error(await r.text());
    const d = await r.json();
    if (d.error) throw new Error(d.error);

    const total = d.total_pending;
    const jf = d.just_fraud_review_pending;
    const b = Math.max(0, d.fraud_review_pending - d.just_fraud_review_pending);
    const jr = d.just_normal_review_pending;

    skel.style.display = 'none';
    chart.style.display = '';

    document.getElementById('total-num').textContent = total;

    const pct = v => total > 0 ? (v / total) * 100 : 0;
    const segs = [
      { id: 'seg-jf', v: jf },
      { id: 'seg-b',  v: b },
      { id: 'seg-jr', v: jr },
    ];
    for (const s of segs) {
      const el = document.getElementById(s.id);
      el.style.width = pct(s.v) + '%';
      el.querySelector('span').textContent = s.v > 0 ? s.v : '';
    }
  } catch (e) {
    console.error('Failed to load stats:', e);
    skel.style.display = '';
    chart.style.display = 'none';
  }
}

loadStats();
setInterval(loadStats, 30000);
loadEvents();
setInterval(loadEvents, 30000);
checkAuth();
</script>
</body>
</html>"##;
