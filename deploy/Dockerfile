FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p clipsyncd

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /var/lib/clipsync clipsync
COPY --from=build /src/target/release/clipsyncd /usr/local/bin/clipsyncd
COPY deploy/config/relay.toml /etc/clipsync/config.toml
RUN mkdir -p /var/lib/clipsync \
    && chown -R clipsync:clipsync /etc/clipsync /var/lib/clipsync
USER clipsync
EXPOSE 8765
ENTRYPOINT ["clipsyncd"]
CMD ["run", "--no-tray", "--config", "/etc/clipsync/config.toml"]
