# Cross-compiles both arches on the amd64 builder (no QEMU): one BUILDPLATFORM stage
# runs cargo-zigbuild for x86_64 + aarch64 musl; zig's cc builds bundled SQLite and mimalloc.
# cargo-chef caches deps; the final stage just picks target/<triple>/release/openfault by TARGETARCH.
# mimalloc is used because musl malloc is slow.
ARG TARGETARCH

FROM --platform=$BUILDPLATFORM rust:1.98-bookworm AS chef
ARG ZIG=0.15.2
ARG ZIG_SHA256=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239
RUN curl -fsSL -o /tmp/zig.tar.xz https://ziglang.org/download/${ZIG}/zig-x86_64-linux-${ZIG}.tar.xz \
    && echo "${ZIG_SHA256}  /tmp/zig.tar.xz" | sha256sum -c - \
    && mkdir /opt/zig && tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1 && rm /tmp/zig.tar.xz \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    && cargo install --locked cargo-chef@0.1.78 cargo-zigbuild@0.23.4
ENV PATH=/opt/zig:$PATH
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG TARGETS="--target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl"
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --zigbuild $TARGETS --recipe-path recipe.json
COPY . .
RUN cargo zigbuild --release --locked $TARGETS

FROM scratch AS bin-amd64
COPY --from=build /app/target/x86_64-unknown-linux-musl/release/openfault /openfault
FROM scratch AS bin-arm64
COPY --from=build /app/target/aarch64-unknown-linux-musl/release/openfault /openfault
FROM bin-${TARGETARCH} AS bin

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=bin /openfault /openfault
VOLUME /data
ENV OPENFAULT_DATA=/data OPENFAULT_PORT=8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/openfault"]
