# Horizons Dashboard

A Rust web dashboard for reviewing Hack Club Horizons submissions. Built with Axum, it shows pipeline statistics, per-user projects across all review stages, and event-level approved hours.

![Screenshot](screenshot.png)

## Features

- **Priority review requests** -- logged-in users can request priority review for their projects that are in regular ("Normal") review via a modal; the request (project, reason, requester) is posted to a configured Slack channel. Only one request per project: once requested, the project stays locked (whether the reviewer approves or rejects) until it clears regular review. Requires `SLACK_BOT_TOKEN` and `PRIORITY_REVIEW_CHANNEL_ID` env vars.
- **Pipeline overview** -- pending review counts for fraud and normal review, displayed as a stacked bar chart
- **Your projects** -- gathers all of a user's projects from the review queue, past reviews, and fraud-rejected submissions into one sorted list, deduplicated by project so each appears once at its latest status
- **Reviewer feedback & timeline** -- shows the latest reviewer feedback per project plus an expandable history of every submission, review, and resubmission
- **Event breakdown** -- shows approved projects and total approved hours grouped by event (Nexus, Arcana, Europa, etc.)
- **Hack Club Auth (HCA) login** -- OIDC-based login with PKCE, server-side nonce validation, and no persistent secrets beyond env vars
- **Rate limiting** -- 10 requests per minute per IP on auth endpoints

## Prerequisites

- Rust (2024 edition)
- A Horizons reviewer account with Slack ID linked
- A Hack Club Auth (HCA) OAuth application with `openid slack_id` scope

## Environment Variables

| Variable | Description |
|---|---|
| `HCA_CLIENT_ID` | OAuth client ID from your HCA application |
| `HCA_CLIENT_SECRET` | OAuth client secret |
| `HCA_REDIRECT_URI` | Must match the redirect URI registered in your HCA app (e.g. `http://localhost:3001/api/auth/callback`) |
| `HORIZONS_SESSION_ID` | Horizons API session cookie value -- paste the full `connect.sid` cookie from your browser after logging into the Horizons review dashboard |
| `PORT` | Server port (default: 3001) |
| `DEV` | Set to `1`/`true` to enable the dev user-override box (top right) for previewing any user's projects by Slack ID, with no login required. Leave unset in production. |
| `ADMIN_USERS` | Comma-separated Slack IDs (e.g. `U0123,U0344`) allowed to use the user-override box while logged in — works in production. |
| `SLACK_BOT_TOKEN` | Slack Bot User OAuth Token with `chat:write` and `chat:write.public` scopes. Required for priority review requests. |
| `SLACK_SIGNING_SECRET` | Slack Signing Secret from your app's Basic Info page. Validates interactive button clicks. Recommended if using Approve/Reject buttons. |
| `PRIORITY_REVIEW_CHANNEL_ID` | Slack channel ID (e.g. `C0123456789`) where priority review requests are posted. Required for priority review requests. |
| `PRIORITY_REVIEW_API_KEY` | API key for `GET /api/priority-review/approved`. Send as `Authorization: Bearer <key>` or `?key=<key>`. |
| `PRIORITY_REVIEW_STORAGE_PATH` | Path to JSON file for persisting priority review records (default: `data/priority_review.json`; the compose file sets this to `/data/priority_review.json` on a persistent volume). |

## Running

```sh
export HCA_CLIENT_ID=...
export HCA_CLIENT_SECRET=...
export HCA_REDIRECT_URI=...
export HORIZONS_SESSION_ID=...

cargo run
```

Then open `http://localhost:3001`.

## Deployment

The app is a single Axum binary. Deploy it behind a reverse proxy (Caddy, nginx) if you want HTTPS. The `SameSite=Lax` cookie works over HTTP for local dev -- add `Secure` to the cookie header in `src/main.rs` if deploying behind HTTPS.

Make sure the `HCA_REDIRECT_URI` points to the public URL of your deployed instance.

### Docker Compose / Coolify

Deploy with `docker-compose.yml`. It builds the image, runs the container, and mounts a named volume at `/data` so priority review records persist across rebuilds/redeploys (the compose file sets `PRIORITY_REVIEW_STORAGE_PATH=/data/priority_review.json`).

```sh
docker compose up -d --build
```

The service only uses `expose` (not `ports`) — Coolify injects its own proxy container and routes to port `3001` over the internal network, so no host port mapping is needed. Set the environment variables above in the Coolify UI (Coolify generates the `.env` that the service reads); for a plain local run, provide a `.env` file alongside the compose file.

## Slack Bot Setup

To enable priority review requests, create a Slack app using `slack-manifest.yml`:

1. Go to [api.slack.com/apps](https://api.slack.com/apps) → Create New App → From manifest → paste `slack-manifest.yml`
2. After creating the app, set the **Interactivity Request URL** to `https://your-domain.com/api/slack/interactions` in the app settings (under Interactivity & Shortcuts). This lets the Approve/Reject buttons work.
3. Install the app to your workspace and copy the **Bot User OAuth Token** (`xoxb-...`) as `SLACK_BOT_TOKEN`
4. Copy the **Signing Secret** from Basic Information → App Credentials as `SLACK_SIGNING_SECRET`
5. Invite the bot to your target channel, or use `chat:write.public` to post without joining
6. Set `PRIORITY_REVIEW_CHANNEL_ID` to the channel ID (right-click channel → Copy link → extract `C...`)

If `SLACK_BOT_TOKEN` or `PRIORITY_REVIEW_CHANNEL_ID` is unset, the priority review feature is disabled and the button stays hidden.
