#!/bin/sh
# PushMessageFixture.java 를 원본 JAR(Jackson 3 + 원본 PushMessage)에 돌려 SQS 본문 해석 기대값(expected_push_message.tsv)을 출력한다.
# 사용: run_push_message.sh [backend-jar] [jdk-bin-dir] > expected_push_message.tsv
set -eu
JAR=${1:-/tmp/chalkak-lanes/oracle/app.jar}
JDK=${2:-/opt/jdk/temurin-25/bin}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
unzip -q -o "$JAR" 'BOOT-INF/lib/*' 'BOOT-INF/classes/*' -d "$WORK"
CP="$WORK/BOOT-INF/classes:$WORK/BOOT-INF/lib/*"
mkdir -p "$WORK/out"
JAVA_TOOL_OPTIONS= "$JDK/javac" -cp "$CP" -d "$WORK/out" "$HERE/PushMessageFixture.java"
JAVA_TOOL_OPTIONS="-Dstdout.encoding=UTF-8 -Dfile.encoding=UTF-8" "$JDK/java" -cp "$WORK/out:$CP" PushMessageFixture
rm -rf "$WORK"
