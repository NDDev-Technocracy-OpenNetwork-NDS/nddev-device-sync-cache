# Bootstrap image has the complete architecture index. The repository toolchain
# selects the current compiler independently of Docker tag publication timing.
FROM rust:1.99.1-trixie@sha256:8a4e90c7946b74626488b1446f712c0fe0ed1a596cbd3d871684bc9c5433ae07 AS build
WORKDIR /build
ARG TARGETARCH
# Native private builders may select an admitted loopback cache using host networking.
# Public hosted CI leaves this unset and retains its independent public source route.
ARG NDS_CACHE_PORT
COPY rust-toolchain.toml ./
RUN test "$(dpkg --print-architecture)" = "$TARGETARCH" \
    && if [ -n "$NDS_CACHE_PORT" ]; then \
         case "$NDS_CACHE_PORT" in *[!0-9]*) exit 2;; esac; \
         test "$NDS_CACHE_PORT" -ge 1 && test "$NDS_CACHE_PORT" -le 65535 \
         && test ! -e "$CARGO_HOME/config.toml" \
         && printf '[source.crates-io]\nreplace-with = "nds-cache"\n[source.nds-cache]\nregistry = "sparse+http://127.0.0.1:%s/crates-index/"\n' "$NDS_CACHE_PORT" > "$CARGO_HOME/config.toml" \
         && RUSTUP_DIST_SERVER="http://127.0.0.1:$NDS_CACHE_PORT/rustup" \
            RUSTUP_UPDATE_ROOT="http://127.0.0.1:$NDS_CACHE_PORT/rustup/rustup" rustup show active-toolchain; \
       else rustup show active-toolchain; fi \
    && rustc --version
ARG NDS_SOURCE_COMMIT
ENV NDS_SOURCE_COMMIT=${NDS_SOURCE_COMMIT}
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# The workspace also holds the device client, which Cargo loads to resolve the workspace.
COPY crates ./crates
COPY contracts ./contracts
RUN test -n "$NDS_SOURCE_COMMIT" \
    && cargo build --locked --release --bin nddev-device-sync-cache --jobs 4

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/nddev-device-sync-cache /usr/local/bin/nddev-device-sync-cache
USER 10001:10001
ENV NDS_CACHE_LISTEN=0.0.0.0:8740 \
    NDS_CACHE_ROOT=/var/lib/nds-cache
EXPOSE 8740
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/nddev-device-sync-cache", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/nddev-device-sync-cache"]
CMD ["serve"]
