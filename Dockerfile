# syntax=docker/dockerfile:1

FROM rust:1.99.1-bookworm AS build
WORKDIR /src
# Dependency layer, rebuilt only when the manifests change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked && rm -rf src
COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked \
    && mkdir -p /out/data/images && cp target/release/lidarr-meta-proxy /out/

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/lidarr-meta-proxy /usr/local/bin/lidarr-meta-proxy
COPY --from=build --chown=65532:65532 /out/data /data
ENV BIND_ADDR=0.0.0.0:5001 \
    DATABASE_PATH=/data/meta.db \
    IMAGE_DIR=/data/images \
    RUST_LOG=info
USER 65532:65532
VOLUME ["/data"]
EXPOSE 5001
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/lidarr-meta-proxy", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/lidarr-meta-proxy"]
