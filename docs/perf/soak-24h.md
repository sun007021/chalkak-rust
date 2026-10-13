# 24시간 장시간 시험 절차 (배포 환경용)

메모리 증가, DB 커넥션 누수, 파일 디스크립터(FD)·스레드 누수를 확인한다. 이 저장소의 개발 환경(공유 4코어 컨테이너)에서는
30~60분 길이만 돌린다([report-2026-10-13.md](report-2026-10-13.md)). 24시간 시험은 private AWS 배포 환경에서 한다.

## 준비

1. 시험 대상: 배포 후보 바이너리를 **운영과 같은 인스턴스 유형, 같은 환경변수, 같은 DB 설정**으로 띄운다. 운영 DB가 아니라
   복제본 또는 전용 DB를 쓴다(쓰기 부하가 있다).
2. 부하 발생기: **다른 인스턴스**에서 돌린다(같은 머신이면 부하 발생기가 CPU를 나눠 쓴다). 같은 VPC의 같은 AZ.
   `cargo build --release -p chalkak-loadgen`으로 만든 `chalkak-loadgen` 하나면 된다(런타임 의존성 없음).
3. 시드: 애플리케이션이 한 번 떠서 Flyway가 스키마를 만든 뒤 시드를 넣는다.
   ```bash
   chalkak-loadgen seed --db "host=<DB> port=5432 user=chalkak password=... dbname=chalkak"
   ```
4. 관리자 계정이 있어야 시드의 알림이 들어간다(앱이 `ADMIN_USERNAME`/`ADMIN_PASSWORD_HASH`로 시작 때 만든다).
5. 측정 대상 프로세스의 PID를 부하 발생기가 읽어야 한다(`--pid`). 부하 발생기를 **앱과 같은 호스트에서** 돌릴 수 없으면 앱 호스트에서
   아래 "호스트 측 감시"를 따로 돌리고 부하 발생기에는 `--pid`를 주지 않는다.

## 부하

운영 예상 피크의 50~100%를 고정 RPS로 24시간 건다. 연결을 오래 붙들고 있는 클라이언트와 요청마다 연결을 새로 맺는 클라이언트를
모두 섞어야 FD·스레드 누수가 드러난다.

```bash
# 1) 메인 부하: 고정 RPS, keep-alive, 유휴 연결 50개. 5분마다 한 줄씩 찍는다.
chalkak-loadgen run --base <앱 주소:8080> --secret <ACCESS_TOKEN_SECRET hex> \
    --rps 300 --conns 64 --idle-conns 50 --duration 24h --warmup 5m --interval 300 \
    --db "<접속 문자열>" --pid <앱 PID> --label rust-24h --out soak.json | tee soak.log

# 2) 연결 재생성 부하(별도 프로세스): 요청마다 새 연결. 연결 스레드·FD가 정리되는지 본다.
chalkak-loadgen run --base <앱 주소:8080> --secret <hex> --no-keepalive --rps 20 --conns 4 \
    --duration 24h --warmup 0 --label rust-24h-churn --out soak-churn.json
```

야간 시간대 효과를 보려면 24시간 중 일부 구간에서 RPS를 올리고 내린다(예: 1시간마다 `--rps`를 바꿔 이어 붙임). 같은 `--pid`를
쓰므로 `soak.log`를 이어 붙여서 `trend`에 넣는다. 주기 작업(04:30 리프레시 토큰 정리, 04:40 가입 토큰 정리, 04:50 알림 정리,
매시 Apple 임시 인증 정리, 1분 런타임 스냅샷 로그)이 시험 시간에 걸쳐야 한다.

## 호스트 측 감시 (앱 호스트에서 병행)

5분마다 앱 프로세스의 상태를 CSV로 남긴다. 부하 발생기의 줄이 없을 때 쓴다.

```bash
PID=$(pgrep -f chalkak-server)
while sleep 300; do
  echo "$(date -Is),$(awk '/VmRSS/{print $2}' /proc/$PID/status),$(ls /proc/$PID/fd | wc -l),$(awk '/Threads/{print $2}' /proc/$PID/status)," \
       "$(psql -Atc "select count(*) from pg_stat_activity where datname='chalkak'")"
done >> host-watch.csv
```

## 판정

```bash
chalkak-loadgen trend --log soak.log --skip 0.2 --pool 11   # 풀 10 + 부하 발생기의 --db 연결 1
```

처음 20%(워밍업)를 버리고 최소제곱 기울기를 본다. 누수 의심 기준:

| 항목 | 기준 |
| --- | --- |
| RSS | 기울기가 시간당 평균의 5% 또는 10MB 중 큰 값 초과, 마지막 값이 앞 절반 최댓값보다 높음 |
| FD | 기울기 시간당 5개 초과, 마지막 값이 앞 절반 최댓값보다 높음 |
| 스레드 | 위와 같음(연결 스레드 구조라 연결이 줄면 스레드도 줄어야 한다) |
| DB 연결 | `pg_stat_activity`의 앱 연결이 풀 크기(10)를 넘으면 누수 |

추가로 확인한다.

- 오류율: 전체 24시간 오류 0건이 목표다(`soak.json`의 `total.errors`). 재시작이나 배포가 없었는데 연결이 끊긴 흔적이 있으면 원인을 본다.
- p99 추세: 시간이 갈수록 p99가 올라가면(테이블 부풀음, 풀 대기, 로그 쓰기) 원인을 찾는다. 줄마다 `p99`가 있다.
- 최대 지연이 주기 작업 시각(04:30대)에 튀는지.
- 앱 로그에서 `ERROR` 수와 1분 런타임 스냅샷 로그의 `hikariActive`/`hikariPending`이 0으로 돌아오는지.
- DB 쪽: 장기 트랜잭션(`pg_stat_activity.xact_start`), 사용하지 않는 연결, `pg_stat_user_tables.n_dead_tup` 추세.
- 끝낸 뒤 SIGTERM으로 정상 종료하는지(진행 중 요청을 마치고 30초 안에).

## 기록

결과는 `docs/perf/`에 `soak-<날짜>.md`로 남긴다: 환경(인스턴스 유형, 앱 버전), 부하, `trend` 출력, 이상 징후와 조치.
