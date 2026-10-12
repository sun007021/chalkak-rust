# 하네스 가짜 외부 서버(harness/fakes) 이미지. 빌드 컨텍스트는 저장소 루트.
# 네트워크가 막힌 환경에서는 이 파일 대신 harness/compose.fakes-local.yaml로 호스트 빌드 바이너리를 쓴다.
FROM rust:1.97-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p chalkak-fakes \
 && cp target/release/chalkak-fakes /chalkak-fakes

FROM debian:trixie-slim
COPY --from=build /chalkak-fakes /usr/local/bin/chalkak-fakes
EXPOSE 9000
ENTRYPOINT ["/usr/local/bin/chalkak-fakes"]
