# Build a small static-ish GNU binary on Debian. The kernel has no C
# dependencies beyond SQLite, which sqlx bundles.
FROM rust:1.89-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && printf 'fn main() {}\n' > src/main.rs && cargo build --release

COPY . .
RUN touch src/main.rs && cargo build --release

# Test stage is not part of the runtime image. It exists so CI and local
# verification can run unit tests with the same locked dependency graph.
FROM builder AS test
RUN cargo test --bin kiki-assistant-kernel-api

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home kernel \
    && mkdir -p /data \
    && chown -R kernel:kernel /data

COPY --from=builder /app/target/release/kiki-assistant-kernel-api /usr/local/bin/kernel

USER kernel
ENV KERNEL_BIND=0.0.0.0:8080
ENV KERNEL_DATABASE_URL=sqlite:///data/kernel.db?mode=rwc
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health/live || exit 1

ENTRYPOINT ["/usr/local/bin/kernel"]
