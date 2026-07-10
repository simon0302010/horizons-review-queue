# Horizons Dashboard

A Rust web dashboard for reviewing Hack Club Horizons submissions. Built with Axum, it shows pipeline statistics, per-user projects across all review stages, event-level approved hours, and supports priority review requests via Slack.

![Screenshot](screenshot.png)

## Prerequisites

- Rust (2024 edition)
- A Horizons reviewer account with Slack ID linked
- A Hack Club Auth (HCA) OAuth application with `openid slack_id` scope

---

## Endpoints

### Frontend

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | Dashboard HTML page |
| `GET` | `/style.css` | Stylesheet |
| `GET` | `/script.js` | Client-side JavaScript |

### Auth

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/auth/login` | Redirects to HCA login page. Initiates PKCE flow with a 128-char code verifier and stores the pending OAuth state server-side (expires after 10 minutes). |
| `GET` | `/api/auth/callback` | HCA OAuth callback. Exchanges the authorization code for tokens, verifies the PKCE nonce, resolves the user's Slack ID, creates a session cookie (`SameSite=Lax`, 24-hour expiry), and redirects to `/`. |
| `GET` | `/api/auth/me` | Returns the current session as JSON: `{ sub, slack_id, display_name }`, or `401` if not logged in. |
| `GET` | `/api/auth/logout` | Clears the session cookie. No response body. |

### Data

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/config` | Public configuration: `{ dev: bool, impersonate: bool, priority_review_enabled: bool }`. No auth required. |
| `GET` | `/api/stats` | Pipeline statistics (`PendingCounts`). Requires valid session. Returns: `{ total_pending, fraud_review_pending, normal_review_pending, just_fraud_review_pending, just_normal_review_pending }`. |
| `GET` | `/api/my/projects` | All projects belonging to the logged-in user across queue, past reviews, and fraud-rejected. Supports `?user=<slack_id>` (admin/DEV only). Each project includes: `projectId`, `projectTitle`, `projectType`, `source`, `status`, `reviewStage`, `queuePosition`, `claimed`, `priorityReviewRequested`, `timeline`, and `feedback`. Requires valid session. |
| `GET` | `/api/events` | Event-level approved hours breakdown. Requires valid session. |

### Priority Review

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/priority-review` | Submit a priority review request. Requires valid session. Body: `{ project_id: u64, reason: string }`. Creates a `Pending` record and posts a Slack message with Approve/Reject buttons. Returns `409` if a non-rejected record already exists for the project. |
| `GET` | `/api/priority-review/approved` | Returns all priority review records with status `Approved`. Protected by `PRIORITY_REVIEW_API_KEY` (send as `Authorization: Bearer <key>` or `?key=<key>`). Returns `501 Not Implemented` if API key is unset. Each entry: `{ project_id, project_title, reason, slack_id, status, decided_by, decided_at }`. |

### Slack Integration

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/slack/interactions` | Handles Slack button clicks (Approve/Reject). Updates the priority review record status and edits the Slack message to reflect the decision. Must be registered as the Interactivity Request URL in your Slack app settings. |

### Dev / Admin

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/dev/users` | Returns a list of all known users (Slack ID + display name). Available in `DEV` mode without auth, or to `ADMIN_USERS` when logged in. Used by the user-override autocomplete. |

---

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `HCA_CLIENT_ID` | Yes | — | OAuth client ID from your HCA application |
| `HCA_CLIENT_SECRET` | Yes | — | OAuth client secret |
| `HCA_REDIRECT_URI` | Yes | — | Must match the redirect URI registered in your HCA app (e.g. `http://localhost:3001/api/auth/callback`) |
| `HORIZONS_SESSION_ID` | Yes | — | Horizons API session cookie value (`connect.sid`) from your browser after logging into the Horizons review dashboard |
| `PORT` | No | `3001` | Server port |
| `DEV` | No | — | Set to `1` or `true` to enable the dev user-override box (top right) for previewing any user's projects by Slack ID, with no login required. Leave unset in production. |
| `ADMIN_USERS` | No | — | Comma-separated Slack IDs (e.g. `U0123,U0344`) allowed to use the user-override box while logged in. Works in production. |
| `SLACK_BOT_TOKEN` | No* | — | Slack Bot User OAuth Token with `chat:write` and `chat:write.public` scopes. Required for priority review. |
| `SLACK_SIGNING_SECRET` | No | — | Slack Signing Secret from your app's Basic Info page. Validates interactive button clicks. Recommended if using Approve/Reject buttons. |
| `PRIORITY_REVIEW_CHANNEL_ID` | No* | — | Slack channel ID (e.g. `C0123456789`) where priority review requests are posted. Required for priority review. |
| `PRIORITY_REVIEW_API_KEY` | No* | — | API key for `GET /api/priority-review/approved`. Send as `Authorization: Bearer <key>` or `?key=<key>`. The endpoint returns `501 Not Implemented` if unset. |
| `PRIORITY_REVIEW_STORAGE_PATH` | No | `data/priority_review.json` | Path to JSON file for persisting priority review records. The compose file sets this to `/data/priority_review.json` on a persistent Docker volume. |

*Required only if using the priority review feature.

---

## Running

```sh
export HCA_CLIENT_ID=...
export HCA_CLIENT_SECRET=...
export HCA_REDIRECT_URI=...
export HORIZONS_SESSION_ID=...

cargo run
```

Then open `http://localhost:3001`.

---

## Deployment

The app is a single Axum binary. Deploy it behind a reverse proxy (Caddy, nginx) if you want HTTPS. The `SameSite=Lax` cookie works over HTTP for local dev — add `Secure` to the cookie header in `src/main.rs` if deploying behind HTTPS.

Make sure the `HCA_REDIRECT_URI` points to the public URL of your deployed instance.

### Docker Compose / Coolify

Deploy with `docker-compose.yml`. It builds the image, runs the container, and mounts a named volume at `/data` so priority review records persist across rebuilds/redeploys (the compose file sets `PRIORITY_REVIEW_STORAGE_PATH=/data/priority_review.json`).

```sh
docker compose up -d --build
```

The service only uses `expose` (not `ports`) — Coolify injects its own proxy container and routes to port `3001` over the internal network, so no host port mapping is needed. Set the environment variables above in the Coolify UI (Coolify generates the `.env` that the service reads); for a plain local run, provide a `.env` file alongside the compose file.

---

## Slack Bot Setup

To enable priority review requests, create a Slack app using `slack-manifest.yml`:

1. Go to [api.slack.com/apps](https://api.slack.com/apps) → Create New App → From manifest → paste `slack-manifest.yml`
2. After creating the app, set the **Interactivity Request URL** to `https://your-domain.com/api/slack/interactions` in the app settings (under Interactivity & Shortcuts). This lets the Approve/Reject buttons work.
3. Install the app to your workspace and copy the **Bot User OAuth Token** (`xoxb-...`) as `SLACK_BOT_TOKEN`
4. Copy the **Signing Secret** from Basic Information → App Credentials as `SLACK_SIGNING_SECRET`
5. Invite the bot to your target channel, or use `chat:write.public` to post without joining
6. Set `PRIORITY_REVIEW_CHANNEL_ID` to the channel ID (right-click channel → Copy link → extract `C...`)

If `SLACK_BOT_TOKEN` or `PRIORITY_REVIEW_CHANNEL_ID` is unset, the priority review feature is disabled and the button stays disabled.

---

## Priority Review Behaviour

- Any queue project can be requested for priority review, regardless of review stage (fraud or normal).
- A project can only be requested once — a record is created and persists through Approve/Reject until the project clears normal review.
- If rejected, the project can be re-requested.
- Records are persisted to `data/priority_review.json` (configurable) so they survive server restarts.

## AI Usage

This is vibecoded
