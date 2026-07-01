# ── Build stage ──
FROM rust:1-bookworm AS builder

WORKDIR /app

# Cache dependencies: build with a dummy main first, then the real sources.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
COPY static ./static
# Touch so cargo picks up the real main.rs over the cached dummy build.
RUN touch src/main.rs && cargo build --release

# ── Runtime stage ──
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user.
# Create /data owned by app so the named volume mounted there (see
# docker-compose.yml) inherits writable ownership on first creation.
RUN useradd --create-home --uid 10001 app \
    && mkdir /data \
    && chown app:app /data
USER app
WORKDIR /home/app

COPY --from=builder /app/target/release/horizons_dashboard /usr/local/bin/horizons_dashboard

ENV PORT=3001
EXPOSE 3001

CMD ["horizons_dashboard"]
