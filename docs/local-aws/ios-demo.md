# iOS 시뮬레이터로 앱 시연하기 (로컬 Rust 백엔드)

로컬에서 Rust 백엔드 전체를 띄우고 iOS 시뮬레이터의 원본 앱을 붙여 시연하는 절차다. 서버 쪽은 한 줄(`scripts/up-all.sh`)이고, 앱 쪽은 이 문서의 준비가 필요하다.
세부 근거와 Android·실기기 절차는 [`app.md`](app.md), 서버 구성은 [`local-aws/README.md`](../../local-aws/README.md).
2026-10-19~21 앱 E2E(iOS 27 시뮬레이터, 카카오 실로그인)에서 확인한 순서다.

## 준비물
- Mac(Apple Silicon 확인됨), Docker Desktop, Xcode(시뮬레이터 런타임이 앱의 배포 타깃 `IPHONEOS_DEPLOYMENT_TARGET`을 지원해야 한다), python3.
- 원본 앱 코드: `woowacourse-teams/2026-Chalkak`의 `client/iOS`. 작업 중인 브랜치를 건드리지 않게 `git worktree add ../chalkak-client origin/main`으로 따로 꺼내는 편이 안전하다. 아래 변경은 모두 **커밋하지 않는다**.

## 1. 백엔드 올리기
```bash
cd chalkak-rust/local-aws
scripts/up-all.sh            # 준비 -> Linux 빌드 -> 기동 -> 스모크. 처음엔 빌드 때문에 몇 분 걸린다
```
- 시뮬레이터만 쓰면 `HOST_IP=127.0.0.1`(기본)로 충분하다. 실기기 iPhone을 쓰면 `HOST_IP=<Mac LAN IP> scripts/up-all.sh`.
- 스모크가 통과해야 서버가 정상이다. 앱이 안 될 때 서버 문제인지 가르는 기준이 된다.
- 앱 API 주소: `https://api.127.0.0.1.sslip.io/api/v1/` (sslip.io가 이름 안의 IP로 응답한다).

## 2. 로컬 CA를 시뮬레이터에 설치
앱은 HTTPS만 허용하고 ATS 예외가 없다. Caddy가 만든 로컬 CA를 시뮬레이터가 믿어야 한다.
```bash
xcrun simctl boot "iPhone 16"        # 이미 켜져 있으면 생략. 기기 이름은 xcrun simctl list devices 로 확인
xcrun simctl keychain booted add-root-cert local-aws/.state/caddy/data/caddy/pki/authorities/local/root.crt
```
시뮬레이터를 초기화하면 사라지니 다시 넣는다. 확인: 시뮬레이터 Safari에서 `https://api.127.0.0.1.sslip.io/actuator/health`가 경고 없이 열린다.

## 3. 앱 설정 (커밋하지 않는 로컬 파일)
`client/iOS/chalkak/chalkak/Configuration/Secrets.xcconfig`를 새로 만든다.
```xcconfig
API_BASE_URL = https:/$()/api.127.0.0.1.sslip.io/api/v1/
GOOGLE_SERVER_CLIENT_ID = dummy
GOOGLE_IOS_CLIENT_ID = dummy
GOOGLE_REVERSED_CLIENT_ID = dummy
KAKAO_NATIVE_APP_KEY = <카카오 실로그인을 쓸 때만 실제 값, 아니면 dummy>
```
- **함정:** `API_BASE_URL`이 https가 아니거나 잘못되면 앱이 조용히 팀 dev 서버(`chalkak-dev.pysun.kr`)로 되돌아간다. 백엔드 로그에 요청이 안 찍히면 이것부터 의심한다.
- `GoogleService-Info.plist`가 없으면 더미를 넣는다. `API_KEY` 값은 **39자**여야 한다(아니면 시작 시 Firebase 초기화에서 크래시). 더미로는 실제 푸시를 받지 못한다.

## 4. 강제 업데이트 화면 우회
`AppVersionGate`가 같은 번들 ID의 실제 App Store 버전을 조회해, 로컬 빌드 버전이 낮으면 강제 업데이트를 띄운다. 로컬 전용 변경으로 게이트를 건너뛴다(커밋 금지).

## 5. 로그인 방식 고르기
| 방식 | 방법 | 비고 |
| --- | --- | --- |
| 카카오 실계정 | `local-aws/env/real-oidc.env.example`을 `env/real-oidc.env`로 복사해 `KAKAO_OIDC_APP_KEY`를 앱의 `KAKAO_NATIVE_APP_KEY`와 같게 채우고 `scripts/up-all.sh --real-oidc` | 이 방식으로 iOS 전체 기능 시연이 통과했다. 기본 가짜 모드의 `smoke.sh`는 이 모드에서 401로 실패하는 것이 정상이다 |
| Apple | 번들 ID `com.stonefive.chalkak`과 팀 키로 `APPLE_*` 설정, 시뮬레이터에 Apple ID 로그인 | 탈퇴를 하면 해당 Apple ID의 앱 연동이 실제로 해제된다. 시연에서는 탈퇴를 빼는 편이 안전하다 |
| 가짜 토큰 | 앱의 로그인 클라이언트가 하네스 키로 서명한 토큰을 반환하도록 로컬 패치 | 앱 코드 변경이 필요하다 |

## 6. 빌드와 시연 흐름
Xcode에서 `client/iOS/chalkak`을 열고 시뮬레이터를 골라 실행한다(시뮬레이터는 Sign to Run Locally라 팀 로그인이 필요 없다). 시연 순서 예:
로그인·가입(사인 업로드) -> 홈 -> 오늘의 사진 업로드(이미지 Lambda가 WebP로 변환) -> 관리자 승인(API 또는 admin-web) -> 알림함·전시 피드 -> 좋아요 -> 기록 -> 설정.
화면별 전체 항목은 프로젝트의 `plan/app-e2e-checklist.md`.

## 7. 푸시 시연
Rust 프로필은 주제·공지 푸시와 PushWorker가 켜져 있고 FCM은 가짜다. 가짜 FCM은 기기로 전달하지 않으므로 시뮬레이터에는 직접 넣는다([`app.md`](app.md) "기기에서 푸시 알림 표시와 탭 재현하기").
```bash
xcrun simctl push booted com.stonefive.chalkak /tmp/push.json
```

## 막힐 때
| 증상 | 확인 |
| --- | --- |
| 앱이 네트워크 오류 | CA 설치(2절), `API_BASE_URL`(3절), `scripts/up-all.sh status` |
| 서버 로그에 요청 없음 | `API_BASE_URL`이 팀 dev 서버로 되돌아간 것 |
| 시작하자마자 크래시 | 더미 plist `API_KEY` 39자 |
| 강제 업데이트 화면 | 4절 |
| 업로드 후 처리 안 됨 | `docker compose --profile rust logs lambda-image esm-poller`, `scripts/trace.sh <uploadId>` |
| 443 포트 사용 중 | 다른 프로세스를 끈다(presigned URL 서명 때문에 443 고정) |
