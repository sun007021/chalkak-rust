# 성능 비교 결과 (Java 대 Rust)

- 라운드: Java 3, Rust 3 (표의 값은 **중앙값 (최소–최대)**)
- 측정 중 호스트 부하 평균(1분): 중앙값 8.6, 최소 1.3, 최대 20.1 (코어 수 대비 높으면 절대값은 믿을 수 없고 Java와 Rust의 상대 비교만 의미가 있다)

## 게이트 요약

통과 43, 통과*(노이즈 이내) 3, **실패 7**, 데이터 없음 0

| 항목 | 엔드포인트 | Java | Rust | 판정 |
| --- | --- | ---: | ---: | --- |
| p99(고정 RPS 혼합) | DELETE /api/v1/posts/{postId}/likes | 35.6 (34.3–40.2) ms | 22.8 (20.0–53.8) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/notifications | 22.3 (16.4–25.5) ms | 18.8 (8.5–22.8) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/notifications/unread-status | 21.8 (19.3–22.3) ms | 9.8 (6.2–18.9) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts | 29.8 (27.4–50.7) ms | 21.4 (15.1–29.8) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/calendar | 24.8 (20.7–36.4) ms | 18.6 (13.8–19.6) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/calendar/months | 25.1 (15.2–37.9) ms | 13.8 (12.7–20.6) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/today | 28.8 (23.0–32.0) ms | 13.4 (11.5–21.6) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts/{postId} | 29.8 (23.9–40.2) ms | 18.3 (17.2–27.0) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts?sort=popular | 76.3 (75.3–76.3) ms | 31.9 (29.7–36.4) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts?sort=random | 46.3 (38.1–58.1) ms | 20.0 (18.2–32.8) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/posts?topicDate=past | 37.4 (30.8–43.5) ms | 22.0 (13.1–30.3) ms | 통과 |
| p99(고정 RPS 혼합) | GET /api/v1/topics | 16.9 (15.7–23.3) ms | 8.3 (6.9–16.9) ms | 통과 |
| p99(고정 RPS 혼합) | PATCH /api/v1/notifications/read-all | 16.5 (9.5–46.9) ms | 14.4 (6.0–35.5) ms | 통과 |
| p99(고정 RPS 혼합) | POST /api/v1/feedbacks | 22.5 (14.0–70.7) ms | 20.6 (9.8–22.1) ms | 통과 |
| p99(고정 RPS 혼합) | POST /api/v1/posts/uploads | 20.4 (16.6–23.5) ms | 10.8 (9.3–14.0) ms | 통과 |
| p99(고정 RPS 혼합) | PUT /api/v1/posts/{postId} | 26.3 (18.7–31.6) ms | 23.4 (11.2–29.0) ms | 통과 |
| p99(고정 RPS 혼합) | PUT /api/v1/posts/{postId}/likes | 38.9 (37.9–44.3) ms | 31.1 (22.1–35.6) ms | 통과 |
| 처리량(엔드포인트별 최대) | DELETE /api/v1/posts/{postId}/likes | 730 (653–738) req/s | 454 (420–494) req/s | 실패 |
| 처리량(엔드포인트별 최대) | GET /api/v1/notifications | 1136 (1100–1150) req/s | 986 (978–1006) req/s | 실패 |
| 처리량(엔드포인트별 최대) | GET /api/v1/notifications/unread-status | 1299 (1278–1462) req/s | 1536 (1404–1574) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts | 587 (570–591) req/s | 494 (477–509) req/s | 실패 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/calendar | 827 (734–831) req/s | 837 (804–850) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/calendar/months | 1038 (836–1048) req/s | 694 (639–725) req/s | 실패 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/today | 951 (892–1056) req/s | 796 (795–877) req/s | 통과* |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts/{postId} | 837 (736–849) req/s | 535 (509–561) req/s | 실패 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts?sort=popular | 69 (62–70) req/s | 183 (167–192) req/s | 통과 |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts?sort=random | 282 (265–299) req/s | 251 (241–262) req/s | 통과* |
| 처리량(엔드포인트별 최대) | GET /api/v1/posts?topicDate=past | 609 (447–627) req/s | 553 (523–561) req/s | 통과* |
| 처리량(엔드포인트별 최대) | GET /api/v1/topics | 1766 (1765–1780) req/s | 4028 (3758–4171) req/s | 통과 |
| 처리량(엔드포인트별 최대) | PATCH /api/v1/notifications/read-all | 1473 (1379–1579) req/s | 1571 (1496–1664) req/s | 통과 |
| 처리량(엔드포인트별 최대) | POST /api/v1/feedbacks | 1061 (1059–1183) req/s | 1647 (1501–1704) req/s | 통과 |
| 처리량(엔드포인트별 최대) | POST /api/v1/posts/uploads | 842 (779–975) req/s | 923 (921–1017) req/s | 통과 |
| 처리량(엔드포인트별 최대) | PUT /api/v1/posts/{postId} | 836 (752–837) req/s | 594 (582–606) req/s | 실패 |
| 처리량(엔드포인트별 최대) | PUT /api/v1/posts/{postId}/likes | 708 (705–740) req/s | 428 (379–454) req/s | 실패 |
| 처리량(혼합 최대) | 전체 | 583 (578–583) req/s | 583 (513–590) req/s | 통과 |
| p99(고정 RPS 혼합) | 전체 | 39.9 (36.4–44.0) ms | 18.8 (17.4–23.8) ms | 통과 |
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
| 고정 RPS | p50 ms | 4.4 (4.4–4.4) | 2.9 (2.9–2.9) |
| 고정 RPS | p95 ms | 22.4 (21.0–22.8) | 9.0 (8.9–9.0) |
| 고정 RPS | p99 ms | 39.9 (36.4–44.0) | 18.8 (17.4–23.8) |
| 고정 RPS | 최대 ms | 111.5 (105.2–119.4) | 64.3 (46.9–78.0) |
| 고정 RPS | 오류 수 | 0 (0–0) | 0 (0–0) |
| 고정 RPS | CPU 코어 | 0.65 (0.65–0.68) | 0.19 (0.19–0.19) |
| 고정 RPS | 요청당 CPU ms | 2.179 (2.171–2.251) | 0.628 (0.619–0.636) |
| 고정 RPS | RSS MB(최대) | 1009 (974–1043) | 23 (23–23) |
| 고정 RPS | FD(최대) | 88 (88–89) | 110 (110–110) |
| 고정 RPS | 스레드(최대) | 79 (78–91) | 66 (66–66) |
| 고정 RPS | 요청당 DB 문장 | 3.26 (3.26–3.33) | 3.26 (3.26–3.33) |
| 고정 RPS | 요청당 DB ms | 1.448 (1.405–1.467) | 0.540 (0.519–0.543) |
| 최대 처리량 | 처리량 req/s | 583 (578–583) | 583 (513–590) |
| 최대 처리량 | p50 ms | 99.8 (99.3–99.8) | 98.8 (98.3–105.0) |
| 최대 처리량 | p95 ms | 236.5 (234.5–238.6) | 220.2 (218.1–284.7) |
| 최대 처리량 | p99 ms | 305.2 (305.2–313.3) | 309.2 (307.2–387.1) |
| 최대 처리량 | 최대 ms | 601.1 (488.3–667.6) | 699.5 (575.8–787.6) |
| 최대 처리량 | 오류 수 | 0 (0–0) | 0 (0–0) |
| 최대 처리량 | CPU 코어 | 1.23 (1.23–1.25) | 0.35 (0.34–0.35) |
| 최대 처리량 | 요청당 CPU ms | 2.108 (2.104–2.164) | 0.599 (0.589–0.665) |
| 최대 처리량 | RSS MB(최대) | 1015 (982–1047) | 24 (23–24) |
| 최대 처리량 | FD(최대) | 89 (89–89) | 111 (111–111) |
| 최대 처리량 | 스레드(최대) | 91 (91–91) | 67 (67–67) |
| 최대 처리량 | 요청당 DB 문장 | 3.25 (3.24–3.25) | 3.25 (3.23–3.25) |
| 최대 처리량 | 요청당 DB ms | 5.314 (5.156–5.328) | 2.679 (2.635–3.252) |

## 엔드포인트별 지연 (고정 RPS 혼합, ms)

| 엔드포인트 | Java p50 | Java p99 | Rust p50 | Rust p99 |
| --- | ---: | ---: | ---: | ---: |
| DELETE /api/v1/posts/{postId}/likes | 5.0 (4.9–5.0) | 35.6 (34.3–40.2) | 4.0 (4.0–4.2) | 22.8 (20.0–53.8) |
| GET /api/v1/notifications | 3.1 (3.1–3.2) | 22.3 (16.4–25.5) | 2.1 (2.0–2.1) | 18.8 (8.5–22.8) |
| GET /api/v1/notifications/unread-status | 2.7 (2.6–2.7) | 21.8 (19.3–22.3) | 1.7 (1.7–1.7) | 9.8 (6.2–18.9) |
| GET /api/v1/posts | 5.7 (5.5–5.7) | 29.8 (27.4–50.7) | 3.4 (3.3–3.4) | 21.4 (15.1–29.8) |
| GET /api/v1/posts/calendar | 4.3 (4.2–4.3) | 24.8 (20.7–36.4) | 2.4 (2.4–2.4) | 18.6 (13.8–19.6) |
| GET /api/v1/posts/calendar/months | 3.6 (3.6–3.7) | 25.1 (15.2–37.9) | 2.6 (2.6–2.6) | 13.8 (12.7–20.6) |
| GET /api/v1/posts/today | 3.6 (3.6–3.6) | 28.8 (23.0–32.0) | 2.4 (2.4–2.5) | 13.4 (11.5–21.6) |
| GET /api/v1/posts/{postId} | 4.1 (4.0–4.1) | 29.8 (23.9–40.2) | 3.2 (3.2–3.3) | 18.3 (17.2–27.0) |
| GET /api/v1/posts?sort=popular | 24.1 (23.2–24.1) | 76.3 (75.3–76.3) | 8.3 (8.1–8.7) | 31.9 (29.7–36.4) |
| GET /api/v1/posts?sort=random | 9.0 (8.9–9.2) | 46.3 (38.1–58.1) | 6.4 (6.3–6.6) | 20.0 (18.2–32.8) |
| GET /api/v1/posts?topicDate=past | 5.3 (5.3–5.5) | 37.4 (30.8–43.5) | 3.2 (3.2–3.3) | 22.0 (13.1–30.3) |
| GET /api/v1/topics | 2.0 (2.0–2.0) | 16.9 (15.7–23.3) | 1.1 (1.1–1.1) | 8.3 (6.9–16.9) |
| PATCH /api/v1/notifications/read-all | 2.7 (2.6–2.9) | 16.5 (9.5–46.9) | 1.7 (1.7–1.8) | 14.4 (6.0–35.5) |
| POST /api/v1/feedbacks | 3.9 (3.6–4.0) | 22.5 (14.0–70.7) | 2.3 (2.2–2.3) | 20.6 (9.8–22.1) |
| POST /api/v1/posts/uploads | 4.6 (4.3–5.2) | 20.4 (16.6–23.5) | 3.0 (2.9–3.1) | 10.8 (9.3–14.0) |
| PUT /api/v1/posts/{postId} | 4.8 (4.8–5.1) | 26.3 (18.7–31.6) | 3.8 (3.6–3.9) | 23.4 (11.2–29.0) |
| PUT /api/v1/posts/{postId}/likes | 5.1 (5.1–5.2) | 38.9 (37.9–44.3) | 4.2 (4.1–4.4) | 31.1 (22.1–35.6) |

## 요청당 SQL과 DB 시간 (`pg_stat_statements`)

| 엔드포인트 | Java 데이터 | Rust 데이터 | Java 제어 | Rust 제어 | Java DB ms | Rust DB ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| GET /api/v1/posts | 3.00 | 3.00 | 1.00 | 0.00 | 0.775 | 0.483 |
| GET /api/v1/posts?sort=popular | 5.00 | 5.00 | 1.00 | 0.00 | 16.516 | 4.584 |
| GET /api/v1/posts?sort=random | 3.00 | 3.00 | 1.00 | 0.00 | 3.089 | 3.415 |
| GET /api/v1/posts?topicDate=past | 3.00 | 3.00 | 1.00 | 0.00 | 0.531 | 0.446 |
| GET /api/v1/posts/{postId} | 5.00 | 5.00 | 2.00 | 0.00 | 0.158 | 0.117 |
| GET /api/v1/posts/today | 4.00 | 4.00 | 2.00 | 0.00 | 0.083 | 0.063 |
| GET /api/v1/posts/calendar | 3.00 | 3.00 | 2.00 | 0.00 | 0.336 | 0.087 |
| GET /api/v1/posts/calendar/months | 3.00 | 3.00 | 2.00 | 0.00 | 0.271 | 0.236 |
| GET /api/v1/topics | 1.00 | 1.00 | 1.00 | 0.00 | 0.031 | 0.016 |
| GET /api/v1/notifications | 2.00 | 2.00 | 2.00 | 0.00 | 0.108 | 0.110 |
| GET /api/v1/notifications/unread-status | 2.00 | 2.00 | 2.00 | 0.00 | 0.067 | 0.037 |
| PUT /api/v1/posts/{postId}/likes | 6.85 | 6.85 | 2.00 | 2.00 | 0.269 | 0.283 |
| DELETE /api/v1/posts/{postId}/likes | 5.00 | 5.00 | 2.00 | 2.00 | 0.162 | 0.138 |
| PUT /api/v1/posts/{postId} | 5.00 | 5.00 | 2.00 | 2.00 | 0.179 | 0.108 |
| POST /api/v1/feedbacks | 3.00 | 3.00 | 2.00 | 0.00 | 0.197 | 0.074 |
| POST /api/v1/posts/uploads | 4.00 | 4.00 | 2.00 | 2.00 | 0.174 | 0.115 |
| PATCH /api/v1/notifications/read-all | 2.00 | 2.00 | 2.00 | 0.00 | 0.210 | 0.173 |

제어 문장은 BEGIN/COMMIT/SET 등이다. JDBC가 트랜잭션 경계를 문장으로 보내는 방식과 `postgres` crate가 보내는 방식이 달라 따로 센다. 게이트는 데이터 문장(SELECT/INSERT/UPDATE/DELETE)으로 판정한다.

## 동시성 시험: java

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
| admin-ban-unban-mix | 통과 | `{"200":4,"400":4}` |  |
| refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| admin-refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| signup-same-token-x10 | 통과 | `{"200":1,"400":9}` |  |
| signup-same-identity-x4 | 통과 | `{"200":1,"500":3}` |  |
| withdraw-lock-order-x12 | 통과 | `{"204":10,"401":26}` |  |
| withdraw-duplicate-x12 | 통과 | `{"204":5,"401":19,"500":12}` |  |
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
| update-vs-delete | 통과 | `{"200":4,"204":1,"404":4}` |  |
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
| admin-ban-unban-mix | 통과 | `{"200":5,"400":3}` |  |
| refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| admin-refresh-reuse-x20 | 통과 | `{"200":1,"401":19}` |  |
| signup-same-token-x10 | 통과 | `{"200":1,"400":9}` |  |
| signup-same-identity-x4 | 통과 | `{"200":1,"500":3}` |  |
| withdraw-lock-order-x12 | 통과 | `{"204":29,"401":7}` |  |
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
| spike-50 | 통과 | 급증 p99 381ms max 631ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 97 | 통과 | 급증 p99 218ms max 498ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 2 |
| spike-200 | 통과 | 급증 p99 1262ms max 2627ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 223 | 통과 | 급증 p99 1073ms max 1885ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 2 |
| spike-400 | 통과 | 급증 p99 2056ms max 2801ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 223 | 통과 | 급증 p99 1753ms max 2612ms 오류 0; 복귀 0초; 누수검사 후 DB연결 10 스레드 2 |
| spike-800 | 통과 | 급증 p99 3506ms max 4173ms 오류 0; 복귀 1초; 누수검사 후 DB연결 10 스레드 223 | 통과 | 급증 p99 3408ms max 5196ms 오류 0; 복귀 1초; 누수검사 후 DB연결 10 스레드 2 |
| storm-1000 | 통과 | 성공 1000/1000 오류 0 전체 1.2초; p99 1233ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 성공 1000/1000 오류 0 전체 1.5초; p99 1446ms; 누수검사 후 DB연결 10 스레드 2 |
| storm-3000 | 통과 | 성공 3000/3000 오류 0 전체 2.3초; p99 2146ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 성공 3000/3000 오류 0 전체 3.9초; p99 3817ms; 누수검사 후 DB연결 10 스레드 2 |
| idle-2000 | 통과 | 붙음 2000/2000 유휴 CPU 0.00코어(1000개당 0.000) RSS 725MB 스레드 223; p99 26ms -> 26ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 붙음 2000/2000 유휴 CPU 0.00코어(1000개당 0.000) RSS 69MB 스레드 2002; p99 11ms -> 46ms; 누수검사 후 DB연결 10 스레드 2 |
| idle-5000 | 통과 | 붙음 5000/5000 유휴 CPU 0.00코어(1000개당 0.001) RSS 811MB 스레드 223; p99 26ms -> 25ms; 누수검사 후 DB연결 10 스레드 223 | 통과 | 붙음 5000/5000 유휴 CPU 0.00코어(1000개당 0.000) RSS 137MB 스레드 5002; p99 14ms -> 12ms; 누수검사 후 DB연결 10 스레드 2 |
| slow-400 | 통과 | 멈춘 연결 끊김 100/100 (최초 60.1초); 한 바이트씩 끊김 0/300; 일반 p99 26ms 오류 0; 누수검사 후 DB연결 10 스레드 223 | 통과 | 멈춘 연결 끊김 100/100 (최초 21.8초); 한 바이트씩 끊김 300/300; 일반 p99 15ms 오류 0; 누수검사 후 DB연결 10 스레드 2 |
| fairness-400 | 통과 | p99 1393ms p99.9 1819ms max 2190ms 500 0; 연결별 최소/중앙 43/68; 누수검사 후 DB연결 10 스레드 224 | 통과 | p99 1417ms p99.9 1917ms max 2997ms 500 0; 연결별 최소/중앙 43/53; 누수검사 후 DB연결 10 스레드 2 |
| fairness-1000 | 통과 | p99 2802ms p99.9 4358ms max 4974ms 500 0; 연결별 최소/중앙 16/29; 누수검사 후 DB연결 10 스레드 224 | 통과 | p99 3228ms p99.9 4014ms max 6163ms 500 0; 연결별 최소/중앙 16/23; 누수검사 후 DB연결 10 스레드 2 |
| dbkill | 통과 | 오류 2 복구 0.0초 / 오류 2 복구 0.0초 / 오류 2 복구 0.0초; 저부하 뒤 연결 10 채운 뒤 10; 누수검사 후 DB연결 10 스레드 224 | 통과 | 오류 1 복구 0.0초 / 오류 1 복구 0.0초 / 오류 1 복구 0.0초; 저부하 뒤 연결 7 채운 뒤 10; 누수검사 후 DB연결 10 스레드 5 |
| outage-pause | 통과 | pause 복구 0.0초 중단 중 오류 0; health {"200":1}; 누수검사 후 DB연결 10 스레드 224 | 통과 | pause 복구 0.0초 중단 중 오류 0; health {"200":2}; 누수검사 후 DB연결 10 스레드 2 |
| outage-stop | 통과 | stop 복구 0.0초 중단 중 오류 3; health {"200":1,"503":1}; 누수검사 후 DB연결 10 스레드 224 | 통과 | stop 복구 0.0초 중단 중 오류 1; health {"200":2}; 누수검사 후 DB연결 10 스레드 2 |
| abort | 통과 | 끊긴 50 처리된 행 50; 누수검사 후 DB연결 10 스레드 224 | 통과 | 끊긴 50 처리된 행 50; 누수검사 후 DB연결 10 스레드 2 |
