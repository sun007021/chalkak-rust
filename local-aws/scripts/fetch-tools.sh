#!/usr/bin/env bash
# 이미지로 받을 수 없는 도구(imds-mock, toxiproxy)를 GitHub 릴리스에서 .state/bin 에 받는다.
# public.ecr.aws·ghcr.io 가 막힌 환경에서도 GitHub 릴리스는 받을 수 있어서 바이너리를 debian 이미지에 마운트해 쓴다.
set -euo pipefail
BIN="$(cd "$(dirname "$0")/.." && pwd)/.state/bin"
IMDS_VERSION="${IMDS_VERSION:-v1.13.0}"
TOXIPROXY_VERSION="${TOXIPROXY_VERSION:-v2.12.0}"
mkdir -p "$BIN"
# 컨테이너는 호스트 CPU와 같은 아키텍처로 돈다(Apple Silicon 의 Docker 는 arm64 Linux). 맞는 바이너리를 받는다.
case "$(uname -m)" in
  arm64|aarch64) ARCH=arm64 ;;
  *) ARCH=amd64 ;;
esac

fetch() { # <url> <대상>
  [[ -x "$2" ]] && { echo "있음: $2"; return; }
  curl -fsSL --retry 3 -o "$2.tmp" "$1"
  chmod +x "$2.tmp"; mv "$2.tmp" "$2"; echo "받음: $2"
}

fetch "https://github.com/aws/amazon-ec2-metadata-mock/releases/download/${IMDS_VERSION}/ec2-metadata-mock-linux-${ARCH}" \
  "$BIN/ec2-metadata-mock"
fetch "https://github.com/Shopify/toxiproxy/releases/download/${TOXIPROXY_VERSION}/toxiproxy-server-linux-${ARCH}" \
  "$BIN/toxiproxy-server"
