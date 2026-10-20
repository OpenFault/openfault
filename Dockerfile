# NOTE: mimalloc is used because musl malloc is slow
# Builds natively per platform (buildx + QEMU for arm64); no cross toolchain needed.
FROM rust:1.98-bookworm AS chef
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add $(uname -m)-unknown-linux-musl \
    && cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --target $(uname -m)-unknown-linux-musl --recipe-path recipe.json
COPY . .
RUN cargo build --release --target $(uname -m)-unknown-linux-musl \
    && cp target/$(uname -m)-unknown-linux-musl/release/openfault /openfault

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /openfault /openfault
VOLUME /data
ENV OPENFAULT_DATA=/data OPENFAULT_PORT=8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/openfault"]
