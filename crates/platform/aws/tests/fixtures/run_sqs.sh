#!/bin/sh
# SqsFixture.java 를 원본 JAR의 AWS SDK v2(SqsClient 2.54.1)로 컴파일·실행해 기대값(expected_sqs.tsv)을 출력한다.
# 원본 `SqsPushConfiguration.notificationSqsClient`와 같은 설정(5초 제한, 재시도 없음)의 클라이언트를 로컬 HTTP 서버에 붙여
# 요청 바이트(헤더·본문)와 응답별 분류(성공/서비스 오류/클라이언트 오류)를 기록한다.
# 사용: run_sqs.sh [backend-jar] [jdk-bin-dir] > expected_sqs.tsv
set -eu
JAR=${1:-/tmp/chalkak-lanes/oracle/app.jar}
JDK=${2:-/opt/jdk/temurin-25/bin}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
unzip -q -o "$JAR" 'BOOT-INF/lib/*' -d "$WORK"
CP="$WORK/BOOT-INF/lib/*"
mkdir -p "$WORK/out"
JAVA_TOOL_OPTIONS= "$JDK/javac" -cp "$CP" -d "$WORK/out" "$HERE/SqsFixture.java"
JAVA_TOOL_OPTIONS=-Dstdout.encoding=UTF-8 "$JDK/java" -cp "$WORK/out:$CP" SqsFixture
rm -rf "$WORK"
