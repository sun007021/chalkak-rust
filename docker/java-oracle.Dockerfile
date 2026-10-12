# 원본 Spring 백엔드(기준 커밋)를 빌드해 oracle로 띄운다. 기준 커밋은 harness/BASELINE과 같아야 한다.
ARG BASELINE

FROM eclipse-temurin:25-jdk AS build
ARG BASELINE
RUN apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git init -q . \
 && git remote add origin https://github.com/woowacourse-teams/2026-Chalkak.git \
 && git fetch -q --depth 1 origin "${BASELINE}" \
 && git checkout -q FETCH_HEAD
WORKDIR /src/backend
RUN --mount=type=cache,target=/root/.gradle \
    ./gradlew bootJar -x test --no-daemon -q \
 && cp build/libs/backend-0.0.1-SNAPSHOT.jar /app.jar

FROM eclipse-temurin:25-jre
RUN apt-get update \
 && apt-get install -y --no-install-recommends libfaketime \
 && mkdir -p /opt/faketime \
 && ln -s "$(dpkg -L libfaketime | grep '/libfaketime.so.1$')" /opt/faketime/libfaketime.so.1 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /app.jar /app/app.jar
EXPOSE 8080
ENTRYPOINT ["java", "-XX:MaxRAMPercentage=70", "-jar", "/app/app.jar"]
