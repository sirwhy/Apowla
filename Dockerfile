# syntax=docker/dockerfile:1.7
# ---------- Build stage ----------
FROM rust:1.85-slim-bookworm AS builder

WORKDIR /build
ENV CARGO_TERM_COLOR=always

# System deps for ring/rustls and a C compiler for the sha2 ASM backend.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config build-essential ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main(){}' > src/main.rs \
 && cargo build --release \
 && rm -rf src target/release/deps/rpow_miner* target/release/rpow-miner*

# Build the actual binary.
COPY src ./src
RUN cargo build --release --locked \
 && strip target/release/rpow-miner

# ---------- Runtime stage ----------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin miner

COPY --from=builder /build/target/release/rpow-miner /usr/local/bin/rpow-miner

USER miner
ENV RUST_BACKTRACE=1 \
    RPOW_LOG=info

EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/rpow-miner"]
