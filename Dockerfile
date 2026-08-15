FROM rust:1.95.0-bookworm

ARG ESPUP_VERSION=0.17.1
ARG ESPFLASH_VERSION=4.5.0
ARG ESP_TOOLCHAIN_VERSION=1.95.0.0

RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates \
        libudev-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install --locked "espup@${ESPUP_VERSION}" \
    && cargo install --locked "espflash@${ESPFLASH_VERSION}" \
    && mkdir -p /opt/esp \
    && espup install \
        --targets esp32 \
        --toolchain-version "${ESP_TOOLCHAIN_VERSION}" \
        --export-file /opt/esp/export-esp.sh

COPY docker/entrypoint.sh /usr/local/bin/esp-entrypoint
RUN chmod 0755 /usr/local/bin/esp-entrypoint

WORKDIR /workspace
ENTRYPOINT ["esp-entrypoint"]
CMD ["cargo", "build", "--release"]
