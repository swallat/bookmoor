# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
ARG BOOKMOOR_COMMIT=
ENV BOOKMOOR_COMMIT=$BOOKMOOR_COMMIT
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p bookmoor-server \
 && cp target/release/bookmoor-server /bookmoor-server

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin bookmoor \
 && mkdir /data && chown bookmoor /data
COPY --from=build /bookmoor-server /usr/local/bin/bookmoor-server
USER bookmoor
WORKDIR /data
ENV BOOKMOOR_STATE=/data/state.json
VOLUME /data
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s CMD \
  bash -c 'exec 3<>/dev/tcp/127.0.0.1/8787 && printf "GET /health HTTP/1.0\r\n\r\n" >&3 && head -n1 <&3 | grep -q " 200"'
# The server refuses a non-loopback address without TLS in front, so run this
# image behind a reverse proxy that terminates TLS (see README).
ENTRYPOINT ["bookmoor-server"]
CMD ["--bind", "0.0.0.0:8787", "--insecure-http"]
