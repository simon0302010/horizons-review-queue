# Horizons Dashboard — AGENTS.md

## Architecture

- Single-crate Rust binary (no workspace, no modules). All backend code in `src/main.rs` (~1750 lines).
- Frontend is plain HTML/CSS/JS in `static/` — no framework. Served as static files via the Axum router.
- State is stored in-memory with `RwLock` (sessions, priority reviews). Priority review records are also persisted to a JSON file (default `data/priority_review.json`, configurable via `PRIORITY_REVIEW_STORAGE_PATH`).
- No database. No tests. No CI.

## Build & Run

```sh
# build
cargo build

# run (requires env vars)
HCA_CLIENT_ID=... HCA_CLIENT_SECRET=... HCA_REDIRECT_URI=... HORIZONS_SESSION_ID=... cargo run

# docker-compose (reads .env)
docker compose up -d --build
```

## Key Conventions

1. **Enrichment in `handle_my_projects`**: the response from `/api/my/projects` is enriched server-side with a `priorityReviewRequested` boolean (line ~1027). Frontend reads this to decide which projects to show in the priority review dropdown.
2. **Slack interaction flow**: `POST /api/slack/interactions` parses a URL-encoded form body with a `payload` JSON field. The handler verifies the Slack signature (if `SLACK_SIGNING_SECRET` is set), then updates the in-memory record and edits the Slack message via `chat.update`.
3. **Priority review data model**: `HashMap<u64, PriorityReviewEntry>` keyed by project ID. Status is one of `Pending`, `Approved`, `Rejected`. A record is created on request, persists through decisions, and is only pruned once the project clears regular normal review (checked via `get_past_reviews()` on endpoint call).
4. **Re-request**: if status is `Rejected`, the project is NOT locked — the user can request again.

## Gotchas

- `PRIORITY_REVIEW_API_KEY` env var controls access to `/api/priority-review/approved`. Returns `501` if unset. Accepts key via `Authorization: Bearer <key>`, `Authorization: ApiKey <key>`, or `?key=<key>` query param.
- `handle_slack_interaction` silently does nothing if `pid == 0` (failed parse) or if no matching record exists in the map.
- The `data/priority_review.json` file is relative to the server's working directory. In Docker, the compose file sets it to `/data/priority_review.json` on a named volume.
- No `Secure` flag on session cookie by default — add it in `src/main.rs` if deploying behind HTTPS.
- `DEV=1` mode enables a user-override box without auth; `ADMIN_USERS` allows the same in production.
