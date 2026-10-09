# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.97.1
FROM rust:${RUST_VERSION}-bookworm AS builder-base
WORKDIR /src
ARG CARGO_BUILD_JOBS=1
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

FROM builder-base AS builder-slim
RUN --mount=type=cache,id=crosslane-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=crosslane-cargo-target,target=/src/target,sharing=locked \
    cargo build --release --locked -p crosslane -p crosslaned --no-default-features \
    && install -D -m 0755 /src/target/release/crosslane /out/crosslane \
    && install -D -m 0755 /src/target/release/crosslaned /out/crosslaned

FROM builder-base AS builder-full
ARG CROSSLANED_FEATURES=crosslaned/sso
RUN --mount=type=cache,id=crosslane-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=crosslane-cargo-target,target=/src/target,sharing=locked \
    cargo build --release --locked -p crosslane -p crosslaned --no-default-features --features "${CROSSLANED_FEATURES}" \
    && install -D -m 0755 /src/target/release/crosslane /out/crosslane \
    && install -D -m 0755 /src/target/release/crosslaned /out/crosslaned \
    && touch /out/build-ready

FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates ncurses-term socat tini vim \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 crosslane \
    && useradd --system --uid 10001 --gid crosslane --home /var/lib/crosslane crosslane \
    && install -d -o crosslane -g crosslane -m 0700 /var/lib/crosslane
COPY deploy/entrypoint.sh /usr/local/bin/crosslane-entrypoint
ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    CROSSLANE_HOME=/var/lib/crosslane
EXPOSE 5580
USER crosslane:crosslane
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/crosslane-entrypoint"]
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD ["crosslane", "--socket", "/var/lib/crosslane/admin.sock", "health"]

FROM runtime-base AS runtime-slim
COPY --from=builder-slim /out/crosslane /usr/local/bin/crosslane
COPY --from=builder-slim /out/crosslaned /usr/local/bin/crosslaned

FROM runtime-base AS runtime-full
USER root
# This constant marker serializes the memory-heavy Rust build and Chromium
# installation. Its stable contents preserve the Chromium package layer cache
# when only application source code changes.
COPY --from=builder-full /out/build-ready /tmp/crosslane-build-ready
ENV CROSSLANE_CHROMIUM_REVISION=1566079
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl unzip \
        fonts-liberation \
        libasound2 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 \
        libcairo2 libcups2 libcurl4 libdbus-1-3 libexpat1 libgbm1 \
        libglib2.0-0 libgtk-3-0 libnspr4 libnss3 libpango-1.0-0 \
        libudev1 libvulkan1 libx11-6 libxcb1 libxcomposite1 \
        libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 \
        wget xdg-utils \
    && curl --fail --location --retry 3 \
        --output /tmp/chromium-linux.zip \
        "https://storage.googleapis.com/chromium-browser-snapshots/Linux_x64/${CROSSLANE_CHROMIUM_REVISION}/chrome-linux.zip" \
    && echo "eafeca49c07daab07d3b8749872811f889e10d44186ebd75771b2fde4e09bca6  /tmp/chromium-linux.zip" \
        | sha256sum --check --strict \
    && unzip -q /tmp/chromium-linux.zip -d /opt \
    && mv /opt/chrome-linux /opt/chromium \
    && find /opt/chromium -type f -name '*.pak.info' -delete \
    && find /opt/chromium/locales -type f ! -name 'en-US.pak' -delete \
    && ln -s /opt/chromium/chrome /usr/local/bin/chromium \
    && chromium --version \
    && rm -rf \
        /var/lib/apt/lists/* \
        /tmp/chromium-linux.zip \
        /tmp/crosslane-build-ready
COPY --from=builder-full /out/crosslane /usr/local/bin/crosslane
COPY --from=builder-full /out/crosslaned /usr/local/bin/crosslaned
ENV CROSSLANE_CHROMIUM_NO_SANDBOX=1
USER crosslane:crosslane

FROM runtime-full AS final
