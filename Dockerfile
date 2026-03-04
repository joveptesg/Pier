# ── Stage 1: Build ──────────────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /app

# Install docker-compose CLI (needed at runtime for stack deployments)
# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/pier-core/Cargo.toml crates/pier-core/Cargo.toml
COPY crates/pier-agent/Cargo.toml crates/pier-agent/Cargo.toml

# Create dummy sources so cargo can fetch & compile dependencies
RUN mkdir -p crates/pier-core/src crates/pier-agent/src \
    && echo 'fn main() {}' > crates/pier-core/src/main.rs \
    && echo 'fn main() {}' > crates/pier-agent/src/main.rs \
    && cargo build --release --package pier-core \
    && rm -rf crates/pier-core/src crates/pier-agent/src

# Copy real sources + assets
COPY crates/ crates/
COPY templates/ templates/

# Touch main.rs to invalidate the cached dummy build
RUN touch crates/pier-core/src/main.rs \
    && cargo build --release --package pier-core

# ── Stage 2: Runtime ───────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gnupg \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" \
       > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
       docker-ce-cli \
       docker-compose-plugin \
       git \
       openssh-client \
    && apt-get purge -y gnupg curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user with docker group for socket access
RUN groupadd -r docker && groupadd -r pier && useradd -r -g pier -G docker pier

WORKDIR /app

# Copy the compiled binary
COPY --from=builder /app/target/release/pier /usr/local/bin/pier

# Create data directory
RUN mkdir -p /app/data && chown -R pier:pier /app

USER pier

ENV PIER_HOST=0.0.0.0
ENV PIER_PORT=8443
ENV PIER_DATA_DIR=/app/data
ENV PIER_LOG_LEVEL=info

EXPOSE 8443

VOLUME ["/app/data"]

CMD ["pier"]
