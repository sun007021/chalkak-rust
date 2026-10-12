# chalkak-server(Rust) 이미지. 빌드 컨텍스트는 저장소 루트.
FROM rust:1.97-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p chalkak-server \
 && cp target/release/chalkak-server /chalkak-server

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends libfaketime ca-certificates \
 && mkdir -p /opt/faketime \
 && ln -s "$(dpkg -L libfaketime | grep '/libfaketime.so.1$')" /opt/faketime/libfaketime.so.1 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /chalkak-server /app/chalkak-server
COPY migrations /app/migrations
WORKDIR /app
EXPOSE 8080
ENTRYPOINT ["/app/chalkak-server"]
