# 원본과 같은 PostgreSQL 18.4에 libfaketime만 더한다. 하네스가 DB 기본값(CURRENT_TIMESTAMP, uuidv7)의 시각도 고정할 수 있게 한다.
FROM postgres:18.4
RUN apt-get update \
 && apt-get install -y --no-install-recommends libfaketime \
 && mkdir -p /opt/faketime \
 && ln -s "$(dpkg -L libfaketime | grep '/libfaketime.so.1$')" /opt/faketime/libfaketime.so.1 \
 && rm -rf /var/lib/apt/lists/*
