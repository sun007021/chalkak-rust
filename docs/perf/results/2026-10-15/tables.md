# 성능 비교 결과 (Java 대 Rust)

- 라운드: Java 3, Rust 3 (표의 값은 **중앙값 (최소–최대)**)
- 측정 중 호스트 부하 평균(1분): 중앙값 12.3, 최소 4.7, 최대 21.9 (코어 수 대비 높으면 절대값은 믿을 수 없고 Java와 Rust의 상대 비교만 의미가 있다)

## 게이트 요약

통과 53, 통과*(노이즈 이내) 0, **실패 0**, 데이터 없음 0

| 항목 | 엔드포인트 | Java | Rust | 판정 |
| --- | --- | ---: | ---: | --- |
| p99(고정 RPS 혼합) | DELETE /api/v1/posts/{postId}/likes | 96.3 (39.9–98.3) ms | 21.6 (14.0–24.1) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/notifications | 57.1 (46.6–127.5) ms | 7.9 (7.3–14.3) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/notifications/unread-status | 61.7 (25.0–84.5) ms | 10.8 (9.5–11.7) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts | 85.0 (47.1–115.7) ms | 13.4 (11.1–15.1) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/calendar | 67.6 (45.3–94.2) ms | 10.8 (8.7–11.3) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/calendar/months | 54.3 (32.1–128.5) ms | 8.6 (7.1–9.3) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/today | 56.6 (46.1–103.9) ms | 14.0 (12.5–17.2) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/{postId} | 78.3 (42.5–105.5) ms | 13.9 (10.6–16.8) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts?sort=popular | 150.5 (126.5–184.3) ms | 31.9 (28.7–47.9) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts?sort=random | 92.2 (66.0–149.5) ms | 21.4 (18.4–44.0) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts?topicDate=past | 60.7 (55.6–135.2) ms | 13.7 (7.9–15.2) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/topics | 40.7 (30.3–79.4) ms | 8.4 (8.1–10.2) ms | 통과 |
| p99(고정 RPS 혼합) | PATCH /api/v1/notifications/read-all | 47.9 (25.7–116.9) ms | 12.4 (9.3–23.2) ms | 통과 |
| p99(고정 RPS 혼합) | POST /api/v1/feedbacks | 60.4 (29.3–131.1) ms | 20.1 (18.8–22.9) ms | 통과 |
| p99(고정 RPS 혼합) | POST /api/v1/posts/uploads | 107.0 (47.9–140.1) ms | 13.9 (10.7–17.5) ms | 통과 |
| p99(고정 RPS 혼합) | PUT /api/v1/posts/{postId} | 108.0 (38.1–201.7) ms | 11.9 (8.5–49.5) ms | 통과 |
| p99(고정 RPS 혼합) | PUT /api/v1/posts/{postId}/likes | 57.6 (41.5–144.4) ms | 16.6 (14.1–21.5) ms | 통과 |
| 처리량(엔드포인트별 최대) | DELETE /api/v1/posts/{postId}/likes | 586 (533–639) req/s | 1579 (1460–1620) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/notifications | 844 (810–961) req/s | 4172 (3820–4507) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/notifications/unread-status | 1169 (1066–1194) req/s | 5499 (5135–5764) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts | 465 (445–518) req/s | 1316 (1285–1411) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/calendar | 678 (658–680) req/s | 3345 (3282–3489) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/calendar/months | 835 (754–844) req/s | 2724 (2654–3069) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/today | 800 (743–859) req/s | 3391 (3303–3766) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/{postId} | 695 (664–751) req/s | 2775 (2365–2830) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts?sort=popular | 69 (64–73) req/s | 206 (188–221) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts?sort=random | 270 (266–274) req/s | 369 (359–391) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts?topicDate=past | 520 (512–549) req/s | 1823 (1706–2009) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/topics | 1574 (1490–1585) req/s | 6941 (6740–6970) req/s | 통과 |
| 처리량(엔드포인트별 최대) | PATCH /api/v1/notifications/read-all | 1221 (1174–1225) req/s | 5285 (5220–5379) req/s | 통과 |
| 처리량(엔드포인트별 최대) | POST /api/v1/feedbacks | 908 (839–983) req/s | 2751 (2536–3183) req/s | 통과 |
| 처리량(엔드포인트별 최대) | POST /api/v1/posts/uploads | 745 (622–750) req/s | 1960 (1834–2098) req/s | 통과 |
| 처리량(엔드포인트별 최대) | PUT /api/v1/posts/{postId} | 655 (560–682) req/s | 1705 (1617–1793) req/s | 통과 |
| 처리량(엔드포인트별 최대) | PUT /api/v1/posts/{postId}/likes | 582 (553–624) req/s | 1234 (1208–1329) req/s | 통과 |
| 처리량(혼합 최대) | 전체 | 517 (496–522) req/s | 1404 (1279–1442) req/s | 통과 |
| p99(고정 RPS 혼합) | 전체 | 81.4 (58.1–129.5) ms | 16.8 (13.6–18.6) ms | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts | 3.00 | 3.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts?sort=popular | 5.00 | 5.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts?sort=random | 3.00 | 3.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts?topicDate=past | 3.00 | 3.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts/{postId} | 5.00 | 5.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts/today | 4.00 | 4.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts/calendar | 3.00 | 3.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/posts/calendar/months | 3.00 | 3.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/topics | 1.00 | 1.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/notifications | 2.00 | 2.00 | 통과 |
| 요청당 SQL(데이터 문장) | GET /api/v1/notifications/unread-status | 2.00 | 2.00 | 통과 |
| 요청당 SQL(데이터 문장) | PUT /api/v1/posts/{postId}/likes | 6.85 | 6.85 | 통과 |
| 요청당 SQL(데이터 문장) | DELETE /api/v1/posts/{postId}/likes | 5.00 | 5.00 | 통과 |
| 요청당 SQL(데이터 문장) | PUT /api/v1/posts/{postId} | 5.00 | 5.00 | 통과 |
| 요청당 SQL(데이터 문장) | POST /api/v1/feedbacks | 3.00 | 3.00 | 통과 |
| 요청당 SQL(데이터 문장) | POST /api/v1/posts/uploads | 4.00 | 4.00 | 통과 |
| 요청당 SQL(데이터 문장) | PATCH /api/v1/notifications/read-all | 2.00 | 2.00 | 통과 |

## 혼합 부하

| 모드 | 지표 | Java | Rust |
| --- | --- | ---: | ---: |
| 고정 RPS | 처리량 req/s | 301 (301–301) | 301 (301–301) |
| 고정 RPS | p50 ms | 5.8 (5.2–8.2) | 1.7 (1.6–1.7) |
| 고정 RPS | p95 ms | 39.7 (29.2–64.8) | 8.0 (7.4–8.4) |
| 고정 RPS | p99 ms | 81.4 (58.1–129.5) | 16.8 (13.6–18.6) |
| 고정 RPS | 최대 ms | 224.5 (192.9–246.7) | 51.9 (46.3–65.9) |
| 고정 RPS | 오류 수 | 0 (0–0) | 0 (0–0) |
| 고정 RPS | CPU 코어 | 0.70 (0.62–0.83) | 0.13 (0.12–0.13) |
| 고정 RPS | 요청당 CPU ms | 2.312 (2.069–2.750) | 0.419 (0.411–0.420) |
| 고정 RPS | RSS MB(최대) | 557 (543–583) | 25 (25–25) |
| 고정 RPS | FD(최대) | 88 (88–88) | 110 (110–110) |
| 고정 RPS | 스레드(최대) | 89 (77–92) | 66 (66–66) |
| 고정 RPS | 요청당 DB 문장 | 3.26 (3.26–3.33) | 3.29 (3.29–3.33) |
| 고정 RPS | 요청당 DB ms | 2.052 (1.818–2.375) | 0.591 (0.521–0.666) |
| 최대 처리량 | 처리량 req/s | 517 (496–522) | 1404 (1279–1442) |
| 최대 처리량 | p50 ms | 112.6 (109.6–113.7) | 37.6 (35.1–42.8) |
| 최대 처리량 | p95 ms | 268.3 (264.2–284.7) | 105.0 (104.4–112.1) |
| 최대 처리량 | p99 ms | 356.4 (337.9–391.2) | 153.6 (149.5–167.9) |
| 최대 처리량 | 최대 ms | 611.5 (515.5–799.0) | 351.3 (292.7–366.5) |
| 최대 처리량 | 오류 수 | 0 (0–0) | 0 (0–0) |
| 최대 처리량 | CPU 코어 | 1.13 (1.08–1.14) | 0.48 (0.43–0.48) |
| 최대 처리량 | 요청당 CPU ms | 2.166 (2.096–2.305) | 0.336 (0.334–0.345) |
| 최대 처리량 | RSS MB(최대) | 558 (546–587) | 26 (26–27) |
| 최대 처리량 | FD(최대) | 89 (89–89) | 111 (111–111) |
| 최대 처리량 | 스레드(최대) | 91 (91–92) | 67 (67–67) |
| 최대 처리량 | 요청당 DB 문장 | 3.25 (3.23–3.25) | 3.26 (3.26–3.35) |
| 최대 처리량 | 요청당 DB ms | 5.865 (5.703–5.889) | 2.237 (2.098–2.602) |

## 엔드포인트별 지연 (고정 RPS 혼합, ms)

| 엔드포인트 | Java p50 | Java p99 | Rust p50 | Rust p99 |
| --- | ---: | ---: | ---: | ---: |
| DELETE /api/v1/posts/{postId}/likes | 7.4 (6.1–9.3) | 96.3 (39.9–98.3) | 2.5 (2.4–2.5) | 21.6 (14.0–24.1) |
| GET /api/v1/notifications | 4.2 (4.1–7.1) | 57.1 (46.6–127.5) | 1.2 (1.2–1.3) | 7.9 (7.3–14.3) |
| GET /api/v1/notifications/unread-status | 3.5 (3.1–5.2) | 61.7 (25.0–84.5) | 1.0 (1.0–1.0) | 10.8 (9.5–11.7) |
| GET /api/v1/posts | 7.4 (6.7–9.3) | 85.0 (47.1–115.7) | 1.9 (1.8–2.1) | 13.4 (11.1–15.1) |
| GET /api/v1/posts/calendar | 5.2 (5.2–7.6) | 67.6 (45.3–94.2) | 1.3 (1.3–1.3) | 10.8 (8.7–11.3) |
| GET /api/v1/posts/calendar/months | 4.6 (4.1–6.6) | 54.3 (32.1–128.5) | 1.4 (1.4–1.4) | 8.6 (7.1–9.3) |
| GET /api/v1/posts/today | 4.8 (4.3–6.5) | 56.6 (46.1–103.9) | 1.3 (1.3–1.3) | 14.0 (12.5–17.2) |
| GET /api/v1/posts/{postId} | 5.4 (4.6–7.8) | 78.3 (42.5–105.5) | 1.5 (1.4–1.5) | 13.9 (10.6–16.8) |
| GET /api/v1/posts?sort=popular | 29.3 (27.5–37.6) | 150.5 (126.5–184.3) | 6.9 (6.1–8.3) | 31.9 (28.7–47.9) |
| GET /api/v1/posts?sort=random | 13.1 (11.4–14.5) | 92.2 (66.0–149.5) | 5.0 (4.8–5.2) | 21.4 (18.4–44.0) |
| GET /api/v1/posts?topicDate=past | 7.6 (7.2–8.6) | 60.7 (55.6–135.2) | 1.8 (1.6–1.8) | 13.7 (7.9–15.2) |
| GET /api/v1/topics | 2.7 (2.5–4.1) | 40.7 (30.3–79.4) | 0.8 (0.8–0.9) | 8.4 (8.1–10.2) |
| PATCH /api/v1/notifications/read-all | 4.3 (3.1–5.6) | 47.9 (25.7–116.9) | 1.0 (1.0–1.2) | 12.4 (9.3–23.2) |
| POST /api/v1/feedbacks | 5.4 (4.5–8.2) | 60.4 (29.3–131.1) | 2.0 (1.8–2.0) | 20.1 (18.8–22.9) |
| POST /api/v1/posts/uploads | 5.7 (5.2–13.4) | 107.0 (47.9–140.1) | 2.4 (2.2–2.5) | 13.9 (10.7–17.5) |
| PUT /api/v1/posts/{postId} | 6.5 (5.3–11.5) | 108.0 (38.1–201.7) | 2.7 (2.4–2.8) | 11.9 (8.5–49.5) |
| PUT /api/v1/posts/{postId}/likes | 6.8 (6.0–10.6) | 57.6 (41.5–144.4) | 2.7 (2.4–2.7) | 16.6 (14.1–21.5) |

## 요청당 SQL과 DB 시간 (`pg_stat_statements`)

| 엔드포인트 | Java 데이터 | Rust 데이터 | Java 제어 | Rust 제어 | Java DB ms | Rust DB ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| GET /api/v1/posts | 3.00 | 3.00 | 1.00 | 0.00 | 0.980 | 0.547 |
| GET /api/v1/posts?sort=popular | 5.00 | 5.00 | 1.00 | 0.00 | 14.811 | 4.186 |
| GET /api/v1/posts?sort=random | 3.00 | 3.00 | 1.00 | 0.00 | 3.301 | 3.031 |
| GET /api/v1/posts?topicDate=past | 3.00 | 3.00 | 1.00 | 0.00 | 0.544 | 0.462 |
| GET /api/v1/posts/{postId} | 5.00 | 5.00 | 2.00 | 0.00 | 0.253 | 0.084 |
| GET /api/v1/posts/today | 4.00 | 4.00 | 2.00 | 0.00 | 0.096 | 0.047 |
| GET /api/v1/posts/calendar | 3.00 | 3.00 | 2.00 | 0.00 | 0.339 | 0.086 |
| GET /api/v1/posts/calendar/months | 3.00 | 3.00 | 2.00 | 0.00 | 0.352 | 0.258 |
| GET /api/v1/topics | 1.00 | 1.00 | 1.00 | 0.00 | 0.040 | 0.017 |
| GET /api/v1/notifications | 2.00 | 2.00 | 2.00 | 0.00 | 0.119 | 0.066 |
| GET /api/v1/notifications/unread-status | 2.00 | 2.00 | 2.00 | 0.00 | 0.083 | 0.023 |
| PUT /api/v1/posts/{postId}/likes | 6.85 | 6.85 | 2.00 | 2.00 | 0.436 | 0.227 |
| DELETE /api/v1/posts/{postId}/likes | 5.00 | 5.00 | 2.00 | 2.00 | 0.193 | 0.111 |
| PUT /api/v1/posts/{postId} | 5.00 | 5.00 | 2.00 | 2.00 | 0.265 | 0.108 |
| POST /api/v1/feedbacks | 3.00 | 3.00 | 2.00 | 0.00 | 0.207 | 0.063 |
| POST /api/v1/posts/uploads | 4.00 | 4.00 | 2.00 | 2.00 | 0.255 | 0.098 |
| PATCH /api/v1/notifications/read-all | 2.00 | 2.00 | 2.00 | 0.00 | 0.277 | 0.156 |

제어 문장은 BEGIN/COMMIT/SET 등이다. JDBC가 트랜잭션 경계를 문장으로 보내는 방식과 `postgres` crate가 보내는 방식이 달라 따로 센다. 게이트는 데이터 문장(SELECT/INSERT/UPDATE/DELETE)으로 판정한다.

## 동시성 시험: java

| 시험 | 결과 | 상태 코드 | 비고 |
| --- | --- | --- | --- |
| like-same-user | 통과 | `{"200":32}` |  |
| like-many-users | 통과 | `{"200":64}` |  |
| like-unlike-same-user | 통과 | `{"200":40}` |  |
| update-same-post | 통과 | `{"200":16}` |  |
| update-vs-delete | 통과 | `{"204":1,"404":8}` |  |
| moderate-approve-x8 | 통과 | `{"200":1,"400":7}` |  |
| moderate-approve-vs-reject | 통과 | `{"200":1,"400":7}` |  |
| moderate-vs-owner-update | 통과 | `{"200":1,"400":6}` |  |
| feedback-burst | 통과 | `{"201":100}` |  |
| atomic-moderate-notification-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-moderate-audit-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-admin-delete-audit-fail | 통과 | `{"500":1}` |  |
| atomic-author-delete-likes-fail | 통과 | `{"500":1}` |  |
| atomic-withdraw-user-update-fail | 통과 | `{"204":1,"500":1}` |  |
| atomic-refresh-rotate-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-admin-refresh-rotate-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-admin-ban-audit-fail | 통과 | `{"200":1,"500":1}` |  |
| visibility-title-update | 통과 | `{"200":3}` |  |
| visibility-like-count | 통과 | `{"200":3}` |  |
| like-vs-author-delete-no-orphan | 통과 | `{"200":60,"204":3}` |  |
| admin-ban-x8 | 통과 | `{"200":1,"400":7}` |  |
| admin-ban-unban-mix | 통과 | `{"200":4,"400":4}` |  |
| refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| admin-refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| signup-same-token-x10 | 통과 | `{"200":1,"400":9}` |  |
| signup-same-identity-x4 | 통과 | `{"200":1,"500":3}` |  |
| withdraw-lock-order-x12 | 통과 | `{"204":19,"401":17}` |  |
| withdraw-duplicate-x12 | 통과 | `{"204":10,"401":19,"500":7}` |  |
| withdraw-vs-refresh | 통과 | `{"200":15,"204":5,"401":15}` |  |
| isolation-clean-slate | 통과 | `{}` |  |
| pool-exhaustion-recovers | 통과 | `{"201":30,"read-200":10}` |  |
| pool-exhaustion-timeout | 통과 | `{"201":10,"500":20,"read-500":10}` |  |

## 동시성 시험: rust

| 시험 | 결과 | 상태 코드 | 비고 |
| --- | --- | --- | --- |
| like-same-user | 통과 | `{"200":32}` |  |
| like-many-users | 통과 | `{"200":64}` |  |
| like-unlike-same-user | 통과 | `{"200":40}` |  |
| update-same-post | 통과 | `{"200":16}` |  |
| update-vs-delete | 통과 | `{"200":8,"204":1}` |  |
| moderate-approve-x8 | 통과 | `{"200":1,"400":7}` |  |
| moderate-approve-vs-reject | 통과 | `{"200":1,"400":7}` |  |
| moderate-vs-owner-update | 통과 | `{"200":1,"400":6}` |  |
| feedback-burst | 통과 | `{"201":100}` |  |
| atomic-moderate-notification-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-moderate-audit-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-admin-delete-audit-fail | 통과 | `{"500":1}` |  |
| atomic-author-delete-likes-fail | 통과 | `{"500":1}` |  |
| atomic-withdraw-user-update-fail | 통과 | `{"204":1,"500":1}` |  |
| atomic-refresh-rotate-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-admin-refresh-rotate-fail | 통과 | `{"200":1,"500":1}` |  |
| atomic-admin-ban-audit-fail | 통과 | `{"200":1,"500":1}` |  |
| visibility-title-update | 통과 | `{"200":3}` |  |
| visibility-like-count | 통과 | `{"200":3}` |  |
| like-vs-author-delete-no-orphan | 통과 | `{"200":60,"204":3}` |  |
| admin-ban-x8 | 통과 | `{"200":1,"400":7}` |  |
| admin-ban-unban-mix | 통과 | `{"200":2,"400":6}` |  |
| refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| admin-refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| signup-same-token-x10 | 통과 | `{"200":1,"400":9}` |  |
| signup-same-identity-x4 | 통과 | `{"200":1,"500":3}` |  |
| withdraw-lock-order-x12 | 통과 | `{"204":30,"401":6}` |  |
| withdraw-duplicate-x12 | 통과 | `{"204":30,"401":6}` |  |
| withdraw-vs-refresh | 통과 | `{"200":15,"204":5,"401":15}` |  |
| isolation-clean-slate | 통과 | `{}` |  |
| pool-exhaustion-recovers | 통과 | `{"201":30,"read-200":10}` |  |
| pool-exhaustion-timeout | 통과 | `{"201":10,"500":20,"read-500":10}` |  |

## 동시성 시험: Java 대 Rust 대조

| 시험 | 상태 코드 분포 | DB 최종 상태 | 비고 |
| --- | --- | --- | --- |
| admin-ban-unban-mix | - | 같음 |  |
| admin-ban-x8 | 같음 | 같음 |  |
| admin-refresh-reuse-x20 | 같음 | 같음 |  |
| atomic-admin-ban-audit-fail | 같음 | 같음 |  |
| atomic-admin-delete-audit-fail | 같음 | 같음 |  |
| atomic-admin-refresh-rotate-fail | 같음 | 같음 |  |
| atomic-author-delete-likes-fail | 같음 | 같음 |  |
| atomic-moderate-audit-fail | 같음 | 같음 |  |
| atomic-moderate-notification-fail | 같음 | 같음 |  |
| atomic-refresh-rotate-fail | 같음 | 같음 |  |
| atomic-withdraw-user-update-fail | 같음 | 같음 |  |
| feedback-burst | - | - |  |
| isolation-clean-slate | 같음 | 같음 |  |
| like-many-users | - | - |  |
| like-same-user | - | - |  |
| like-unlike-same-user | - | - |  |
| like-vs-author-delete-no-orphan | - | 같음 |  |
| moderate-approve-vs-reject | - | - |  |
| moderate-approve-x8 | - | - |  |
| moderate-vs-owner-update | - | - |  |
| pool-exhaustion-recovers | - | - |  |
| pool-exhaustion-timeout | - | - |  |
| refresh-reuse-x20 | 같음 | 같음 |  |
| signup-same-identity-x4 | - | 같음 |  |
| signup-same-token-x10 | - | 같음 |  |
| update-same-post | - | - |  |
| update-vs-delete | - | - |  |
| visibility-like-count | 같음 | 같음 |  |
| visibility-title-update | 같음 | 같음 |  |
| withdraw-duplicate-x12 | - | **다름** | 알려진 차이: Java는 동시 탈퇴에서 social_accounts 엔티티 삭제가 StaleStateException(행 수 0)으로 500, Rust는 집합 삭제라 오류 없음 |
| withdraw-lock-order-x12 | - | 같음 |  |
| withdraw-vs-refresh | - | 같음 |  |

## 스트레스 시험 (`perf.sh stress`)

기준은 두 구현에 같다(docs/perf/stress.md). 이 표는 통과·실패가 Rust 대 Java 비교가 아니라 케이스별 절대 기준이다. 호스트가 공유일 때 수치는 참고용이다.

| 케이스 | Java | Java 핵심 수치 | Rust | Rust 핵심 수치 |
| --- | --- | --- | --- | --- |
| spike-50 | 통과 | 급증 p99 381ms max 631ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 97 | 통과 | 급증 p99 108ms max 353ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 2 |
| spike-200 | 통과 | 급증 p99 1262ms max 2627ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 223 | 통과 | 급증 p99 485ms max 1268ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 2 |
| spike-400 | 통과 | 급증 p99 2056ms max 2801ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 223 | 통과 | 급증 p99 803ms max 1762ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 2 |
| spike-800 | 통과 | 급증 p99 3506ms max 4173ms 오류 0; 복귀 1초; 누수검사 후 DB연결 10 스레드 223 | 통과 | 급증 p99 1704ms max 4247ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 2 |
| storm-1000 | 통과 | 성공 1000/1000 오류 0 전체 1.2초; p99 1233ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 성공 1000/1000 오류 0 전체 1.5초; p99 1475ms; 누수검사 후 DB연결 10 스레드 2 |
| storm-3000 | 통과 | 성공 3000/3000 오류 0 전체 2.3초; p99 2146ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 성공 3000/3000 오류 0 전체 2.8초; p99 2802ms; 누수검사 후 DB연결 10 스레드 4 |
| idle-2000 | 통과 | 붙음 2000/2000 유휴 CPU 0.00코어(1000개당 0.000) RSS 725MB 스레드 223; p99 26ms -> 26ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 붙음 2000/2000 유휴 CPU 0.00코어(1000개당 0.000) RSS 68MB 스레드 2004; p99 12ms -> 19ms; 누수검사 후 DB연결 10 스레드 4 |
| idle-5000 | 통과 | 붙음 5000/5000 유휴 CPU 0.00코어(1000개당 0.001) RSS 811MB 스레드 223; p99 26ms -> 25ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 붙음 5000/5000 유휴 CPU 0.00코어(1000개당 0.000) RSS 137MB 스레드 5002; p99 201ms -> 17ms; 누수검사 후 DB연결 10 스레드 2 |
| slow-400 | 통과 | 멈춘 연결 끊김 100/100 (최초 60.1초); 한 바이트씩 끊김 0/300; 일반 p99 26ms 오류 0; 누수검사 후 DB연결 10 스레드 223 | 통과 | 멈춘 연결 끊김 100/100 (최초 21.5초); 한 바이트씩 끊김 300/300; 일반 p99 13ms 오류 0; 누수검사 후 DB연결 10 스레드 2 |
| fairness-400 | 통과 | p99 1393ms p99.9 1819ms max 2190ms 500 0; 연결별 최소/중앙 43/68; 누수검사 후 DB연결 10 스레드 224 | 통과 | p99 365ms p99.9 1090ms max 1279ms 500 0; 연결별 최소/중앙 218/250; 누수검사 후 DB연결 10 스레드 2 |
| fairness-1000 | 통과 | p99 2802ms p99.9 4358ms max 4974ms 500 0; 연결별 최소/중앙 16/29; 누수검사 후 DB연결 10 스레드 224 | 통과 | p99 1188ms p99.9 1696ms max 2390ms 500 0; 연결별 최소/중앙 72/95; 누수검사 후 DB연결 10 스레드 2 |
| dbkill | 통과 | 오류 2 복구 0.0초 / 오류 2 복구 0.0초 / 오류 2 복구 0.0초; 저부하 뒤 연결 10 채운 뒤 10; 누수검사 후 DB연결 10 스레드 224 | 통과 | 오류 1 복구 0.0초 / 오류 1 복구 0.0초 / 오류 3 복구 0.1초; 저부하 뒤 연결 2 채운 뒤 10; 누수검사 후 DB연결 10 스레드 10 |
| outage-pause | 통과 | pause 복구 0.0초 중단 중 오류 0; health {"200":1}; 누수검사 후 DB연결 10 스레드 224 | 통과 | pause 복구 0.0초 중단 중 오류 0; health {"200":2}; 누수검사 후 DB연결 10 스레드 2 |
| outage-stop | 통과 | stop 복구 0.0초 중단 중 오류 3; health {"200":1,"503":1}; 누수검사 후 DB연결 10 스레드 224 | 통과 | stop 복구 0.0초 중단 중 오류 1; health {"200":2}; 누수검사 후 DB연결 10 스레드 2 |
| abort | 통과 | 끊긴 50 처리된 행 50; 누수검사 후 DB연결 10 스레드 224 | 통과 | 끊긴 50 처리된 행 50; 누수검사 후 DB연결 10 스레드 2 |
