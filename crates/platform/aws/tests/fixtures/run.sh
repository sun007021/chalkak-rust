#!/bin/sh
# PresignFixture.java 를 원본 JAR의 AWS SDK v2 로 컴파일·실행해 기대값을 출력한다.
# 사용: run.sh [backend-jar] [jdk-bin-dir]
# 자격증명(AWS_ACCESS_KEY_ID 등)과 AWS_ENDPOINT_URL_S3 는 호출 측 환경변수로 준다.
set -eu
JAR=${1:-/home/claude/2026-Chalkak/backend/build/libs/backend-0.0.1-SNAPSHOT.jar}
JDK=${2:-/opt/jdk/temurin-25/bin}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
unzip -q -o "$JAR" 'BOOT-INF/lib/*' -d "$WORK"
CP="$WORK/BOOT-INF/lib/*"
mkdir -p "$WORK/out"
JAVA_TOOL_OPTIONS= "$JDK/javac" -cp "$CP" -d "$WORK/out" "$HERE/PresignFixture.java"
JAVA_TOOL_OPTIONS=-Dstdout.encoding=UTF-8 "$JDK/java" -cp "$WORK/out:$CP" PresignFixture
rm -rf "$WORK"
