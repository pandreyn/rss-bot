# ---- Build stage ----
ARG RUST_VERSION=1.84
FROM rust:${RUST_VERSION}-alpine AS builder

RUN apk add --no-cache build-base musl-dev pkgconfig
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app

# Cache dependencies
COPY Cargo.toml Cargo.lock ./

# Create a minimal target so Cargo doesn't complain during `cargo fetch`
# This preserves dependency caching without requiring your full source yet.
RUN mkdir -p src && \
    printf 'fn main() {}\n' > src/main.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo fetch

# Now bring in the real source and build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    #cargo build --release --target-dir /app/target 
    cargo build --release \
    && strip target/release/rss-bot \
    && mkdir -p /app/out && cp -a /app/target/release/rss-bot /app/out/rss-bot

# ---- Runtime stage ----
FROM alpine:3.20

# TLS certs for HTTPS. libc6-compat helps in some edge cases with deps.
#RUN apk add --no-cache ca-certificates libc6-compat

WORKDIR /app
COPY --from=builder /app/out/rss-bot /app/rss-bot

# Run as non-root
RUN adduser -D -H -u 10001 appuser
USER appuser

ENTRYPOINT ["/app/rss-bot"]
