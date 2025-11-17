# ---- Build stage ----
ARG RUST_VERSION=1.84
FROM rust:${RUST_VERSION}-alpine AS builder

# Install build tools and MUSL toolchain
RUN apk add --no-cache build-base musl-dev pkgconfig

# If you depend on OpenSSL, you can add it here, but for MUSL static builds
# prefer rustls in your Cargo features. Uncomment only if truly needed:
# RUN apk add --no-cache openssl-dev

# Prepare MUSL target
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app

# Copy the source and build
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

# Strip symbols to reduce binary size (best-effort)
RUN strip target/x86_64-unknown-linux-musl/release/rss-bot || true

# ---- Runtime stage ----
FROM alpine:3.20

# TLS certs for HTTPS. libc6-compat helps in some edge cases with deps.
RUN apk add --no-cache ca-certificates libc6-compat

WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/rss-bot /app/rss-bot

# Run as non-root
RUN adduser -D -H -u 10001 appuser
USER appuser

ENTRYPOINT ["/app/rss-bot"]
