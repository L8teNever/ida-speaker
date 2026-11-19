# syntax=docker/dockerfile:1
# ida-speaker: ein Image mit allem, was der Lautsprecher braucht (Programm, Sprachmodell, Cloudflare WARP).
# Gebaut wird nativ auf arm64 (Raspberry Pi 4/5, 64-Bit-OS) und amd64 (zum Testen am PC).

ARG VOSK_VERSION=0.3.45
ARG VOSK_MODEL=vosk-model-small-de-0.15

FROM rust:1-trixie AS build
ARG VOSK_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
      clang libclang-dev libasound2-dev pkg-config meson ninja-build cmake git unzip curl \
    && rm -rf /var/lib/apt/lists/*
# libvosk (Wake-Word) als fertige Bibliothek von den Vosk-Releases
RUN set -eux; case "$(dpkg --print-architecture)" in amd64) a=x86_64;; arm64) a=aarch64;; *) echo "nicht unterstützt"; exit 1;; esac; \
    curl -fsSL -o /tmp/vosk.zip "https://github.com/alphacep/vosk-api/releases/download/v${VOSK_VERSION}/vosk-linux-${a}-${VOSK_VERSION}.zip"; \
    unzip -j /tmp/vosk.zip '*/libvosk.so' -d /usr/local/lib; ldconfig; rm /tmp/vosk.zip
ENV RUSTFLAGS="-L /usr/local/lib"
WORKDIR /src
# Erst nur die Abhängigkeiten bauen (inkl. WebRTC-Echo-Filter) – das bleibt im Cache, solange Cargo.lock gleich ist
COPY Cargo.toml Cargo.lock ./
RUN mkdir src web && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src src
COPY web web
RUN touch src/main.rs && cargo build --release --locked && cp target/release/ida-speaker /usr/local/bin/

FROM debian:trixie-slim AS model
ARG VOSK_MODEL
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL -o /tmp/m.zip "https://alphacephei.com/vosk/models/${VOSK_MODEL}.zip" \
    && unzip -q /tmp/m.zip -d /tmp && mv "/tmp/${VOSK_MODEL}" /opt/vosk-model && rm /tmp/m.zip

FROM debian:trixie-slim
RUN set -eux; apt-get update; apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg alsa-utils libstdc++6 tini dbus iproute2 network-manager; \
    curl -fsSL https://pkg.cloudflareclient.com/pubkey.gpg | gpg --dearmor -o /usr/share/keyrings/cloudflare-warp-archive-keyring.gpg; \
    echo "deb [signed-by=/usr/share/keyrings/cloudflare-warp-archive-keyring.gpg] https://pkg.cloudflareclient.com/ trixie main" \
      > /etc/apt/sources.list.d/cloudflare-client.list; \
    apt-get update; apt-get install -y --no-install-recommends cloudflare-warp; \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/lib/libvosk.so /usr/local/lib/
COPY --from=build /usr/local/bin/ida-speaker /usr/local/bin/
COPY --from=model /opt/vosk-model /opt/vosk-model
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
# Selbsttest: alles da, keine fehlenden Bibliotheken
RUN ldconfig && chmod +x /usr/local/bin/entrypoint.sh \
    && command -v warp-svc && command -v warp-cli && command -v tini && command -v nmcli && command -v dbus-send && test -d /opt/vosk-model/am \
    && ! ldd /usr/local/bin/ida-speaker /usr/local/lib/libvosk.so | grep "not found"
ENV IDA_SPEAKER_DATA=/data VOSK_MODEL=/opt/vosk-model PORT=8080 RUST_LOG=info
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD curl -fsS http://127.0.0.1:8080/health || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]
