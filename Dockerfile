# Multi-stage build with cargo-chef: the dependency layer is rebuilt only when
# Cargo.toml/Cargo.lock change, so code-only deploys compile just this crate.
FROM lukemathwalker/cargo-chef:0.1.78-rust-1.100.0-slim-trixie AS chef
WORKDIR /build
# The VPS also runs latency-sensitive bots: compile on one core.
ARG CARGO_BUILD_JOBS=1
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin digest

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app digest
WORKDIR /app
COPY --from=builder /build/target/release/digest /usr/local/bin/digest
COPY config.yaml /app/config.yaml
RUN mkdir -p /app/data /app/logs && chown -R digest /app
USER digest
ENTRYPOINT ["digest"]
CMD ["serve"]
