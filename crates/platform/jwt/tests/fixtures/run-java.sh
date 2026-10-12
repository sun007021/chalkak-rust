#!/bin/sh
# 단일 파일 Java 프로그램을 원본 classpath로 실행한다.
#
# 사용: run-java.sh <Program.java> [인자...]
# 환경 변수:
#   CHALKAK_ORACLE_DIR  setup-oracle.sh가 만든 디렉터리 (기본: /tmp/chalkak-jwt-oracle)
#   JAVA                java 실행 파일 (기본: /opt/jdk/temurin-25/bin/java)
set -eu
DIR="${CHALKAK_ORACLE_DIR:-/tmp/chalkak-jwt-oracle}"
JAVA="${JAVA:-/opt/jdk/temurin-25/bin/java}"
PROG="$1"
shift
# 프록시용 JAVA_TOOL_OPTIONS가 stderr에 안내문을 찍지 않도록 비운다.
JAVA_TOOL_OPTIONS= exec "$JAVA" -Dfile.encoding=UTF-8 -Dstdout.encoding=UTF-8 -cp "$DIR/classes:$DIR/lib/*" "$PROG" "$@"
