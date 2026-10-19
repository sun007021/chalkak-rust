#!/bin/sh
# FcmFixture.java 를 원본 JAR(Firebase Admin 9.11.0 + 원본 FcmDevicePushSender)에 돌려 기대값(expected_fcm.tsv)을 출력한다.
# FCM 주소(https://fcm.googleapis.com)는 SDK 전송(NetHttpTransport)의 연결 팩토리로 로컬 서버에 돌리고, OAuth 토큰 주소는
# 서비스 계정 JSON 의 token_uri 로 돌린다. 원본 코드가 엔드포인트를 설정으로 받지 않아 JAR 안의 실제 운영 클래스를 그대로 쓰되
# 전송 계층만 바꾼다(원본 FcmDevicePushSenderTest 와 같은 방법).
# 사용: run_fcm.sh [backend-jar] [jdk-bin-dir] [private-key.pem] > expected_fcm.tsv
set -eu
JAR=${1:-/tmp/chalkak-lanes/oracle/app.jar}
JDK=${2:-/opt/jdk/temurin-25/bin}
HERE=$(cd "$(dirname "$0")" && pwd)
KEY=${3:-$HERE/fcm-test-key.pem}
WORK=$(mktemp -d)
unzip -q -o "$JAR" 'BOOT-INF/lib/*' 'BOOT-INF/classes/*' -d "$WORK"
CP="$WORK/BOOT-INF/classes:$WORK/BOOT-INF/lib/*"
mkdir -p "$WORK/out"
JAVA_TOOL_OPTIONS= "$JDK/javac" -cp "$CP" -d "$WORK/out" "$HERE/FcmFixture.java"
JAVA_TOOL_OPTIONS="-Dstdout.encoding=UTF-8 -Dfile.encoding=UTF-8" "$JDK/java" -cp "$WORK/out:$CP" FcmFixture "$KEY"
rm -rf "$WORK"
