# ---- Build stage ----
ARG RUST_VERSION=1.84
FROM rust:${RUST_VERSION}-slim-bookworm AS builder
WORKDIR /app

# Cache deps
# COPY Cargo.toml Cargo.lock ./
# RUN mkdir -p src && echo "fn main() {}" > src/main.rs
# RUN cargo build --release
# RUN rm -rf src

# Build real app
COPY . .
RUN cargo build --release

# ---- Runtime stage ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/rss-bot /app/rss-bot

#ENV STATE_FILE=/app/state.json \
#    DEDUP_LIMIT=200 \
#    POLL_EVERY_MINUTES=5
ENTRYPOINT ["/app/rss-bot"]
