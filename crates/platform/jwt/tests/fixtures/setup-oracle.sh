#!/bin/sh
# 원본 실행 JAR에서 클래스와 라이브러리를 풀어 교차 검증용 classpath를 만든다.
#
# 사용: setup-oracle.sh [출력 디렉터리]   (기본: /tmp/chalkak-jwt-oracle)
# 환경 변수:
#   ORACLE_JAR  원본 실행 JAR (기본: /home/claude/2026-Chalkak/backend/build/libs/backend-0.0.1-SNAPSHOT.jar)
# 출력 디렉터리 아래에 classes/(원본 클래스)와 lib/(nimbus-jose-jwt, spring-security 등)가 생긴다.
# 다른 스크립트는 환경 변수 CHALKAK_ORACLE_DIR로 이 디렉터리를 받는다.
set -eu
OUT="${1:-/tmp/chalkak-jwt-oracle}"
JAR="${ORACLE_JAR:-/home/claude/2026-Chalkak/backend/build/libs/backend-0.0.1-SNAPSHOT.jar}"
mkdir -p "$OUT/classes" "$OUT/lib"
unzip -q -o "$JAR" 'BOOT-INF/classes/*' -d "$OUT/tmp"
unzip -q -o "$JAR" 'BOOT-INF/lib/*' -d "$OUT/tmp"
cp -R "$OUT/tmp/BOOT-INF/classes/." "$OUT/classes/"
cp "$OUT/tmp/BOOT-INF/lib/"*.jar "$OUT/lib/"
rm -rf "$OUT/tmp"
echo "$OUT"
