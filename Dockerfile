FROM docker.io/lukemathwalker/cargo-chef:0.1.78-rust-bookworm@sha256:14ac293f096e29d01296479bb2f81ec1fb0d6ff166dc2be399cf79cc79ec4906 AS chef
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY vendor/zstd-0.13.3 ./vendor/zstd-0.13.3
COPY vendor/zstd-safe-7.2.4 ./vendor/zstd-safe-7.2.4
COPY vendor/zstd-sys-2.0.16+zstd.1.5.7 ./vendor/zstd-sys-2.0.16+zstd.1.5.7
COPY packages/execpolicy-rs ./packages/execpolicy-rs
COPY packages/context-rs ./packages/context-rs
COPY packages/tui-rs ./packages/tui-rs
COPY packages/local-host-rs ./packages/local-host-rs
COPY packages/sandbox-rs ./packages/sandbox-rs
COPY packages/workspace-rs ./packages/workspace-rs
COPY packages/codex-rs ./packages/codex-rs
COPY packages/swarm-rs ./packages/swarm-rs
COPY packages/ui-rs ./packages/ui-rs
COPY packages/interaction-rs ./packages/interaction-rs
COPY packages/presentation-rs ./packages/presentation-rs
COPY packages/ui-preview-rs ./packages/ui-preview-rs
COPY packages/scenario-rs ./packages/scenario-rs
COPY packages/runtime-gateway-rs ./packages/runtime-gateway-rs
COPY packages/maestro-rs ./packages/maestro-rs
COPY packages/runtime-rs ./packages/runtime-rs
COPY packages/runtime-contracts-rs ./packages/runtime-contracts-rs
COPY packages/coding-acceptance-rs ./packages/coding-acceptance-rs
COPY packages/ambient-agent-rs ./packages/ambient-agent-rs
COPY packages/ai-rs ./packages/ai-rs
COPY packages/a2a-ledger-rs ./packages/a2a-ledger-rs
COPY packages/session-history-rs ./packages/session-history-rs
COPY packages/session-rs ./packages/session-rs
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS native
COPY --from=planner /app/recipe.json recipe.json
COPY vendor/zstd-0.13.3 ./vendor/zstd-0.13.3
COPY vendor/zstd-safe-7.2.4 ./vendor/zstd-safe-7.2.4
COPY vendor/zstd-sys-2.0.16+zstd.1.5.7 ./vendor/zstd-sys-2.0.16+zstd.1.5.7
RUN cargo chef cook --release --locked -p maestro --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY packages/execpolicy-rs ./packages/execpolicy-rs
COPY packages/context-rs ./packages/context-rs
COPY packages/tui-rs ./packages/tui-rs
COPY packages/local-host-rs ./packages/local-host-rs
COPY packages/sandbox-rs ./packages/sandbox-rs
COPY packages/workspace-rs ./packages/workspace-rs
COPY packages/codex-rs ./packages/codex-rs
COPY packages/swarm-rs ./packages/swarm-rs
COPY packages/ui-rs ./packages/ui-rs
COPY packages/interaction-rs ./packages/interaction-rs
COPY packages/presentation-rs ./packages/presentation-rs
COPY packages/ui-preview-rs ./packages/ui-preview-rs
COPY packages/scenario-rs ./packages/scenario-rs
COPY packages/runtime-gateway-rs ./packages/runtime-gateway-rs
COPY packages/maestro-rs ./packages/maestro-rs
COPY packages/runtime-rs ./packages/runtime-rs
COPY packages/runtime-contracts-rs ./packages/runtime-contracts-rs
COPY packages/coding-acceptance-rs ./packages/coding-acceptance-rs
COPY packages/ambient-agent-rs ./packages/ambient-agent-rs
COPY packages/ai-rs ./packages/ai-rs
COPY packages/a2a-ledger-rs ./packages/a2a-ledger-rs
COPY packages/session-history-rs ./packages/session-history-rs
COPY packages/session-rs ./packages/session-rs
COPY proto ./proto
COPY scripts/install.sh ./scripts/install.sh
COPY test/fixtures/codex/coding-tools-doctor-v1.json ./test/fixtures/codex/coding-tools-doctor-v1.json
RUN cargo build --release --locked -p maestro

FROM debian:bookworm-slim
# Seed CA trust from the bookworm build image before switching apt to HTTPS.
COPY --from=native /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=native /app/target/release/maestro /usr/local/bin/maestro
COPY skills ./skills
# The image binds to every interface, so the runtime gateway requires API-key
# auth. Supply a key at run time, for example:
#   docker run -p 3000:3000 -e MAESTRO_WEB_API_KEY="$(openssl rand -hex 32)" ghcr.io/dx-corp/maestro
# Do not add MAESTRO_WEB_REQUIRE_KEY=0 here: it is only honored for loopback
# binds and the server refuses to start when it is combined with 0.0.0.0.
ENV MAESTRO_CONTROL_HOST=0.0.0.0 PORT=3000
EXPOSE 3000
ENTRYPOINT ["maestro"]
CMD ["serve"]
