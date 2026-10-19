# 앱을 로컬 AWS 환경에 연결하기 (L3)

Android 에뮬레이터·iOS 시뮬레이터·실기기의 앱을 `local-aws/` 환경(HTTPS `api.<IP>.sslip.io`, 로컬 CA)에 붙이고
계획 6.5 스모크를 사람이 한 번 훑는 절차다. 환경 구성은 [`local-aws/README.md`](../../local-aws/README.md), 검증 기록은 [`verification.md`](verification.md).

> **이 문서의 확인 범위.** 원본 클라이언트(`/home/claude/2026-Chalkak/client`, 읽기 전용)는 파일과 줄을 읽어 근거를 달았다.
> 서버 쪽(`smoke.sh`, `trace.sh`, 실계정 오버레이의 컨테이너 설정)은 이 저장소에서 Rust·Java 둘로 실행해 확인했다.
> 에뮬레이터·시뮬레이터·실기기는 클라우드 세션에 없어서 **아래 기기 쪽 명령은 실행해 보지 못했다**. 각 절에 "미확인"을 적어 둔 곳은 첫 시도에서 확인하고 이 문서를 고친다.

## 0. 먼저 서버 쪽을 확인한다

앱을 붙이기 전에 앱 없이 같은 흐름이 도는지 본다. 앱에서 실패했을 때 서버 문제인지 기기 문제인지 가르는 기준이 된다.

```bash
cd local-aws
echo "HOST_IP=<노트북 LAN IP>" > .env       # 예: 192.168.0.10. 같은 PC의 에뮬레이터·시뮬레이터만 쓰면 127.0.0.1도 된다(아래 1절)
docker compose --profile rust up -d          # 또는 --profile java
scripts/smoke.sh                             # 가입 -> 로그인 -> 게시물 -> 피드 -> 승인 -> 좋아요 -> 알림 -> cdn 이미지 (약 10초)
```

`smoke.sh`가 통과해야 한다. 통과하는데 앱만 안 되면 기기의 CA 신뢰, API_BASE_URL, 소셜 로그인 값 순서로 의심한다(8절).

### HOST_IP 고르기
- 휴대폰(실기기)을 붙일 때는 **노트북의 LAN IP**(휴대폰과 같은 Wi-Fi). 이름은 전부 `*.<IP>.sslip.io`가 되고 sslip.io가 이름 안의 IP로 응답한다. 인터넷이 없는 자리면 dnsmasq 등으로 `api.<IP>.sslip.io`, `cdn.<IP>.sslip.io`, `<버킷>.s3.<IP>.sslip.io`, `s3.<IP>.sslip.io`를 같은 IP로 풀어 준다.
- 에뮬레이터·시뮬레이터도 같은 LAN IP를 써도 된다(호스트 443에 닿는다). 에뮬레이터만 쓴다면 `HOST_IP=10.0.2.2`(에뮬레이터에서 본 호스트)도 되지만, 실기기와 같이 쓸 수 없고 IP를 바꾸면 컨테이너를 다시 띄워야 하니 LAN IP 하나로 통일하는 편이 낫다.
- 호스트 443을 다른 프로세스가 쓰고 있으면 비운다(presigned URL 서명에 포트가 들어가면 안 되어서 443 고정이다, README 참고).
- IP를 바꿔도 CA 루트는 그대로다(`.state/caddy` 를 지우지 않는 한). 기기에 CA를 다시 설치할 필요가 없다. 리프 인증서만 이름마다 새로 발급된다.

## 1. 로컬 CA 파일 꺼내기

edge(Caddy `tls internal`)의 루트 인증서가 앱이 믿어야 할 대상이다.

```bash
cd local-aws
# 방법 1: 호스트 파일로 (compose 를 한 번 띄운 뒤)
cp .state/caddy/data/caddy/pki/authorities/local/root.crt /tmp/chalkak-local-ca.crt
# 방법 2: 컨테이너에서
docker compose cp edge:/data/caddy/pki/authorities/local/root.crt /tmp/chalkak-local-ca.crt

openssl x509 -in /tmp/chalkak-local-ca.crt -noout -subject -enddate -fingerprint -sha256   # 기기에 설치한 것과 같은지 대조
# 연결 점검(호스트에서): 인증서와 이름이 맞으면 {"status":"UP"} 류가 온다
curl --cacert /tmp/chalkak-local-ca.crt --resolve api.<IP>.sslip.io:443:127.0.0.1 https://api.<IP>.sslip.io/actuator/health
```

`.state/caddy`를 지우면 CA가 새로 만들어지므로 기기에서 이전 CA를 지우고 새로 설치한다.

## 2. Android 에뮬레이터

### 앱이 CA를 어떻게 보는가 (근거)
- `android/app/src/main/res/xml/network_security_config.xml:3`: `<base-config cleartextTrafficPermitted="false" />` 뿐이고 `trust-anchors`가 없다. 매니페스트에서 연결된다(`AndroidManifest.xml:24`).
- `targetSdk = 37`(`android/app/build.gradle.kts:46`)이라 `trust-anchors`가 없는 앱은 **시스템 CA만** 믿는다(사용자 CA 불가). 그래서 에뮬레이터는 CA를 **시스템 저장소**에 넣어야 한다. 앱 코드는 바꾸지 않는다.
- API 주소는 HTTPS여야 한다: `NetworkModule.kt:69-71` `toHttpsBaseUrl()`이 아니면 앱이 `require`로 죽는다. 그래서 `http://10.0.2.2:8080` 같은 우회는 불가능하다.

### 에뮬레이터 만들기
시스템 파티션에 쓰려면 `adb root`가 되는 이미지가 필요하다: **Google APIs 이미지(Play 스토어 없음)**. "Google Play" 이미지는 `adb root`가 막혀 있다.

```bash
sdkmanager "system-images;android-33;google_apis;x86_64"        # Apple 실리콘이면 arm64-v8a
avdmanager create avd -n chalkak-api33 -k "system-images;android-33;google_apis;x86_64" -d pixel_7
emulator -avd chalkak-api33 -writable-system -no-snapshot       # -writable-system: 아래 remount 에 필요
```

API 레벨에 따라 CA를 넣는 방법이 다르다. 가능하면 **API 33 이하**로 만든다(파일을 `/system`에 직접 넣는다). API 34 이상은 CA가 APEX(`/apex/com.android.conscrypt/cacerts`)로 옮겨 가서 2-B 방법이 필요하다. 앱의 `minSdk = 26`(`build.gradle.kts:45`)이라 33에서도 돈다.

### 실제로 안정적이었던 조합 (앱 E2E 검증, 2026-10-19)
- 이미지는 **Google APIs API 33**(`google_apis`)을 쓰고 CA는 `/system`에 넣었다(2-A). Google Play 이미지는 `adb root`가 되지 않아 쓰지 못했다.
- 호스트 IP가 바뀌면(Wi-Fi 이동 등) 에뮬레이터의 DNS가 꼬여 sslip.io 이름이 엉뚱한 IP로 풀렸다. 에뮬레이터만 쓸 때는 다음 조합이 안정적이었다.
  - 스택을 `HOST_IP=127.0.0.1`로 띄운다(이름은 `api.127.0.0.1.sslip.io` 등).
  - `adb reverse tcp:443 tcp:443`으로 에뮬레이터의 127.0.0.1:443을 호스트 443으로 넘긴다.
  - 에뮬레이터 `/system/etc/hosts`에 쓰는 이름(`api.`, `cdn.`, `s3.`, `<버킷>.s3.` + `127.0.0.1.sslip.io`)을 `127.0.0.1`로 적어 외부 DNS에 기대지 않는다(`adb root && adb remount` 뒤 편집).
  - 실기기와 함께 쓸 때는 이 조합을 쓸 수 없으니 위 "HOST_IP 고르기"대로 LAN IP 하나로 통일한다.

### 2-A. API 33 이하: /system 에 넣기
```bash
HASH=$(openssl x509 -inform PEM -subject_hash_old -noout -in /tmp/chalkak-local-ca.crt | head -1)
cp /tmp/chalkak-local-ca.crt "/tmp/$HASH.0"

adb root
adb shell avbctl disable-verification        # 이미지에 따라 없을 수 있다(그러면 건너뛴다)
adb disable-verity
adb reboot && adb wait-for-device && adb root
adb remount                                   # 'remount succeeded' 가 나와야 한다
adb push "/tmp/$HASH.0" /system/etc/security/cacerts/
adb shell chmod 644 "/system/etc/security/cacerts/$HASH.0"
adb reboot
```
재부팅 뒤 확인: 에뮬레이터 Chrome에서 `https://api.<IP>.sslip.io/actuator/health`가 경고 없이 열린다. (설정 > 보안 > 암호화 및 사용자 인증정보 > 신뢰할 수 있는 인증서 > 시스템 탭에 로컬 CA가 보인다.)

### 2-B. API 34 이상: 메모리에 덮어쓰기 (재부팅하면 사라진다)
```bash
HASH=$(openssl x509 -inform PEM -subject_hash_old -noout -in /tmp/chalkak-local-ca.crt | head -1)
cp /tmp/chalkak-local-ca.crt "/tmp/$HASH.0"
adb root
adb push "/tmp/$HASH.0" /data/local/tmp/
adb shell "
  set -e
  mkdir -p -m 700 /data/local/tmp/ca-copy
  cp /apex/com.android.conscrypt/cacerts/* /data/local/tmp/ca-copy/
  mount -t tmpfs tmpfs /system/etc/security/cacerts
  mv /data/local/tmp/ca-copy/* /system/etc/security/cacerts/
  cp /data/local/tmp/$HASH.0 /system/etc/security/cacerts/
  chown root:root /system/etc/security/cacerts/*
  chmod 644 /system/etc/security/cacerts/*
  chcon u:object_r:system_file:s0 /system/etc/security/cacerts/*
  for pid in 1 \$(pgrep zygote) \$(pgrep zygote64); do
    nsenter --mount=/proc/\$pid/ns/mnt -- /bin/mount --bind /system/etc/security/cacerts /apex/com.android.conscrypt/cacerts
  done
"
```
이 방식은 **에뮬레이터를 다시 켤 때마다** 해야 하고, 앱을 그 뒤에 (다시) 실행해야 한다. 미확인: 34 이상 이미지의 마운트 네임스페이스 동작은 이미지 빌드마다 다르다. 막히면 API 33 이미지를 쓴다.

### 앱 빌드 설정: `client/android/local.properties`
`android/app/build.gradle.kts:11-25`: 세 값이 모두 **필수**이고(없으면 빌드가 `error("... is missing from local.properties")`), 파일은 `.gitignore`에 있어 커밋되지 않는다(`android/.gitignore:3`).

```properties
# client/android/local.properties  (sdk.dir 은 Android Studio 가 채운 그대로 둔다)
API_BASE_URL=https://api.<IP>.sslip.io/api/v1/
GOOGLE_SERVER_CLIENT_ID=<백엔드 GOOGLE_OIDC_CLIENT_ID 와 같은 웹 클라이언트 ID>
KAKAO_NATIVE_APP_KEY=<백엔드 KAKAO_OIDC_APP_KEY 와 같은 네이티브 앱 키>
```
- `API_BASE_URL` 끝의 `/api/v1/`와 마지막 `/`를 지킨다. Retrofit 기준 주소로 쓰이고(`NetworkModule.kt:26,33`) 요청 경로가 `auth/…`, `posts/…`처럼 상대 경로다. HTTPS가 아니면 앱이 죽는다.
- `GOOGLE_SERVER_CLIENT_ID`는 `GetSignInWithGoogleOption.Builder(serverClientId)`로 쓰인다(`GoogleIdTokenClient.kt:32`). 비면 CONFIGURATION 실패(`:23`).
- `KAKAO_NATIVE_APP_KEY`는 `KakaoSdk.init`(`ChalkakApplication.kt:11`)과 매니페스트 URL 스킴 `kakao${KAKAO_NATIVE_APP_KEY}`(`AndroidManifest.xml:57`)에 쓰인다.
- 가짜 모드(5절)에서는 소셜 로그인을 앱으로 할 수 없으니 값은 자리표시자여도 빌드는 된다.
- `google_services` 플러그인(`build.gradle.kts:8`)이 `app/google-services.json`을 요구한다. 파일은 `.gitignore`(`android/app/.gitignore:2`)라 저장소에 없다. 팀 dev용 파일을 받아 `android/app/`에 둔다(Firebase 푸시·분석용이며 로컬 환경과 무관).

빌드·설치: `cd client/android && ./gradlew :app:installDebug` (에뮬레이터가 떠 있는 상태).

## 3. iOS 시뮬레이터

### 앱이 URL 을 어떻게 읽는가 (근거)
- `iOS/chalkak/Info.plist:49-50`: `API_BASE_URL = $(API_BASE_URL)`. 값은 빌드 설정(xcconfig)에서 온다.
- `iOS/chalkak/chalkak/Configuration/App.xcconfig:4`가 `#include? "Secrets.xcconfig"`로 로컬 파일을 읽고, 그 파일은 `.gitignore`(`iOS/.gitignore:10`)다.
- `AppConfiguration.swift:32-40`: `API_BASE_URL`이 **https이고 host가 있어야** 값으로 인정한다. 아니면 `nil`이 되고,
  - **함정:** 여러 화면의 API 클라이언트는 `nil`이면 `https://chalkak-dev.pysun.kr/api/v1/`(팀 dev 서버)로 **조용히 되돌아간다**(예: `PhotoUploadAPIClient.swift:7`, `FeedAPIClient.swift:7`, `HomeAPIClient.swift:7`, `ContentView.swift:531,568`). URL을 잘못 적으면 로컬이 아니라 팀 dev 서버에 요청이 간다. 서버 로그(`scripts/trace.sh`, backend 로그)에 요청이 안 찍히면 이것부터 의심한다.
- ATS 예외는 필요 없다(HTTPS, 신뢰된 CA).

### 설정: `client/iOS/chalkak/chalkak/Configuration/Secrets.xcconfig` (새로 만든다, 커밋되지 않음)
```xcconfig
// xcconfig 에서 // 는 주석이라 URL 의 // 를 이렇게 쓴다
API_BASE_URL = https:/$()/api.<IP>.sslip.io/api/v1/
GOOGLE_SERVER_CLIENT_ID = <백엔드 GOOGLE_OIDC_CLIENT_ID 와 같은 웹 클라이언트 ID>
GOOGLE_IOS_CLIENT_ID = <iOS 클라이언트 ID>
GOOGLE_REVERSED_CLIENT_ID = <iOS 클라이언트 ID 를 뒤집은 URL 스킴>
KAKAO_NATIVE_APP_KEY = <백엔드 KAKAO_OIDC_APP_KEY 와 같은 네이티브 앱 키>
```
- Google 로그인은 `GOOGLE_IOS_CLIENT_ID`와 `GOOGLE_SERVER_CLIENT_ID`가 **둘 다** 있어야 시작한다(`GoogleLoginClient.swift:27-31`). ID Token의 `aud`는 서버 클라이언트 ID다(같은 파일 `:22-24` 주석). `GOOGLE_REVERSED_CLIENT_ID`는 URL 스킴에 들어간다(`Info.plist:77`).
- Kakao 키는 URL 스킴 `kakao$(KAKAO_NATIVE_APP_KEY)`(`Info.plist:76`)에 쓰인다.

### CA 신뢰
```bash
xcrun simctl boot "iPhone 16"                         # 이미 켜져 있으면 생략. booted 는 켜진 시뮬레이터를 가리킨다
xcrun simctl keychain booted add-root-cert /tmp/chalkak-local-ca.crt
```
시뮬레이터마다 따로 넣는다(초기화하면 사라진다). 확인: 시뮬레이터 Safari에서 `https://api.<IP>.sslip.io/actuator/health`.
빌드는 Xcode에서 시뮬레이터를 고르고 실행한다. 배포 타깃이 `IPHONEOS_DEPLOYMENT_TARGET = 26.5`(`project.pbxproj`)라 Xcode·시뮬레이터 런타임이 이 버전을 지원해야 한다.

### 앱 버전 게이트와 Firebase 설정 (앱 E2E 검증, 2026-10-19)
- **강제 업데이트 화면:** iOS `AppVersionGate`는 같은 번들 ID로 실제 App Store 버전을 조회한다. 로컬 빌드의 버전이 스토어보다 낮으면 강제 업데이트를 띄워 앱을 쓸 수 없다. 로컬에서는 게이트를 건너뛰는 로컬 전용 변경(커밋하지 않는다)이나 스토어에 없는 별도 번들 ID로 빌드한다.
- **더미 `GoogleService-Info.plist`:** 팀 Firebase 파일이 없을 때 더미를 넣는다면 `API_KEY` 값이 **39자**여야 한다. 길이가 다르면 시작할 때 Firebase 초기화에서 크래시가 난다. 더미로는 실제 푸시를 받을 수 없다.

## 4. 실기기

### iPhone
1. CA 파일을 기기로 옮긴다: AirDrop·메일·또는 노트북에서 `cd /tmp && python3 -m http.server 8000` 후 휴대폰 Safari로 `http://<IP>:8000/chalkak-local-ca.crt`를 연다(CA 공개 인증서라 평문 전달해도 된다).
2. 설정 > 일반 > VPN 및 기기 관리에서 내려받은 프로파일을 설치한다.
3. **설정 > 일반 > 정보 > 인증서 신뢰 설정**에서 해당 CA의 **전체 신뢰**를 켠다. 설치만 하고 이 단계를 빼면 신뢰되지 않는다.
4. 휴대폰 Safari에서 `https://api.<IP>.sslip.io/actuator/health`가 경고 없이 열리는지 확인한다.
5. 앱은 Xcode로 기기에 설치(개발자 서명)하고, 위 3절의 `Secrets.xcconfig`로 빌드한다.

### Android 실기기
실기기는 `adb root`가 안 되어 시스템 CA를 못 넣는다. 사용자 CA는 이 앱이 믿지 않으므로(2절 근거) **`debug` 빌드에만** 사용자 CA를 믿게 하는 **로컬 전용 패치**를 쓴다.

```bash
cd client/android
# 1) 패치 파일을 만든다(저장소 밖). debug-overrides 는 android:debuggable=true 인 빌드에서만 적용된다.
cat > /tmp/chalkak-debug-user-ca.patch <<'EOF'
--- a/app/src/main/res/xml/network_security_config.xml
+++ b/app/src/main/res/xml/network_security_config.xml
@@ -1,4 +1,9 @@
 <?xml version="1.0" encoding="utf-8"?>
 <network-security-config>
     <base-config cleartextTrafficPermitted="false" />
+    <debug-overrides>
+        <trust-anchors>
+            <certificates src="user" />
+        </trust-anchors>
+    </debug-overrides>
 </network-security-config>
EOF
git apply /tmp/chalkak-debug-user-ca.patch
git update-index --skip-worktree app/src/main/res/xml/network_security_config.xml   # 실수로 커밋/푸시하지 않게
./gradlew :app:installDebug
```
사용이 끝나면 되돌린다:
```bash
git update-index --no-skip-worktree app/src/main/res/xml/network_security_config.xml
git checkout -- app/src/main/res/xml/network_security_config.xml
```
**이 패치는 커밋하지 않는다.** 원본 클라이언트 저장소는 이번 작업에서 읽기 전용이고, 릴리스 빌드에 사용자 CA 신뢰가 들어가면 안 된다(`debug-overrides`는 릴리스에 적용되지 않지만 파일 자체를 올리지 않는다).

기기에 CA 설치: CA 파일을 옮긴 뒤 설정 > 보안(및 개인정보 보호) > 암호화 및 사용자 인증정보 > 인증서 설치 > **CA 인증서**. (화면 잠금이 있어야 한다. Android 11+는 "CA 인증서"를 직접 선택해야 설치된다.) `API_BASE_URL`은 노트북 LAN IP 이름으로 `local.properties`(2절)에 적는다.

## 5. 소셜 로그인 모드

앱은 실제 Google·Kakao·Apple SDK에서 ID Token을 받는다. 가짜 토큰은 앱이 만들 수 없다.

| | 실계정 모드 (앱 시험 기본) | 가짜 모드 (smoke·부하 시험) |
| --- | --- | --- |
| 용도 | 앱으로 로그인·가입 | 앱 없이 HTTP(`smoke.sh`, `loadgen`) |
| JWKS | 진짜 구글·카카오·애플 공개키 | 하네스 `fakes` (`harness/keys`로 서명한 토큰) |
| 클라이언트 ID | 앱 빌드와 같은 값 | `test-google-client-id` 등 더미 |
| 설정 | `compose.real-oidc.yaml` + `env/real-oidc.env` | 기본(`env/local-aws.env`) |

### 실계정 모드
백엔드의 검증값은 앱이 받는 ID Token의 `aud`와 같아야 한다.

| 백엔드 env | 앱 쪽 값 | 근거 |
| --- | --- | --- |
| `GOOGLE_OIDC_CLIENT_ID` | `GOOGLE_SERVER_CLIENT_ID` (Android `local.properties`, iOS xcconfig) | 앱은 서버 클라이언트 ID로 토큰을 받는다(`GoogleIdTokenClient.kt:32`, `GoogleLoginClient.swift:22-31`). 원본 `infrastructure-setup.md:138` |
| `KAKAO_OIDC_APP_KEY` | `KAKAO_NATIVE_APP_KEY` | Android·iOS Kakao SDK 공통 네이티브 앱 키. 원본 `infrastructure-setup.md:139` |
| `APPLE_OIDC_CLIENT_ID` | iOS 번들 ID `com.stonefive.chalkak` | `project.pbxproj`의 `PRODUCT_BUNDLE_IDENTIFIER`. 네이티브 Apple 로그인의 `aud`는 번들 ID다(일반 Sign in with Apple 동작이며, 팀 dev 값과 같은지 배포 설정에서 대조한다) |
| `APPLE_TEAM_ID`, `APPLE_KEY_ID`, `APPLE_PRIVATE_KEY_BASE64` | 팀 Apple Developer 키 | Apple **가입**의 토큰 교환·철회에 필요. 키가 없으면 Apple은 기존 회원 로그인만 시험하거나 제외 |

전환:
```bash
cd local-aws
cp env/real-oidc.env.example env/real-oidc.env     # git 에 올라가지 않는다. <...> 를 채운다
docker compose -f compose.yaml -f compose.real-oidc.yaml --profile rust up -d    # 또는 --profile java
docker compose exec rust env | grep -E "OIDC_CLIENT_ID|OIDC_APP_KEY|JWKSETURI"   # 적용 확인 (java 면 exec java)
```
- `compose.real-oidc.yaml`이 백엔드의 `env_file` 뒤에 `env/real-oidc.env`를 덧붙인다. 같은 키는 뒤 파일이 이긴다. 오버레이가 하는 일은 두 가지다: (1) 클라이언트 ID를 앱 값으로, (2) `CHALKAK_AUTH_OIDC_*_JWKSETURI`·`CHALKAK_AUTH_APPLE_TOKEN_*`를 가짜 `fakes` 주소에서 **진짜 주소**(원본 `application.yml:96-108`, `:86-87`의 값)로.
- 백엔드 컨테이너가 인터넷으로 구글·카카오·애플 공개키를 가져온다(이 클라우드 세션은 외부 접속이 막혀 있어 실제 가져오기는 못 해 봤다. 오버레이로 기동하고 가짜 토큰이 401 로 거절되는 것까지 확인했다). Rust 는 webpki 루트로, Java 는 JVM 기본 truststore 로 검증하므로 추가 설정이 없다.
- Apple 팀 키가 없으면 `real-oidc.env`의 `APPLE_*` 세 줄을 주석인 채로 둔다(`local-aws.env`의 더미 키로 기동만 된다).
- 이 모드에서는 `smoke.sh`가 401(유효하지 않은 ID Token)로 실패하는 것이 정상이다(가짜 토큰을 안 받는다). 서버 쪽 확인은 가짜 모드로 돌아와서 한다.

가짜 모드로 복귀: `-f compose.real-oidc.yaml` 없이 같은 `docker compose --profile … up -d`. 백엔드 컨테이너만 다시 만들어진다(DB·큐 유지). 백엔드를 Rust ↔ Java 로 바꿀 때는 DB 를 비워야 한다(`local-aws/README.md` 빠른 시작).

### 가짜 모드
기본 구성이다. 앱 없이 `scripts/smoke.sh`로 흐름을 돌리고, 부하 시험은 `harness/loadgen`(9절)으로 가상 사용자를 만든다. 앱 로그인 버튼은 쓸 수 없다. 서버가 가짜 JWKS만 믿기 때문이다.

### 에뮬레이터에서의 소셜 로그인 (미확인)
- Android: Google 로그인은 Credential Manager를 쓰므로 Google Play 서비스와 로그인된 구글 계정이 필요하다. "Google APIs" 이미지에는 Play 서비스가 있지만 Play 스토어 앱은 없다. 에뮬레이터 설정에서 구글 계정을 추가할 수 있는지는 이미지에 따라 다르다. 안 되면 Google·Apple 로그인은 실기기에서 시험한다. Kakao는 웹 계정 로그인으로 가능하다.
- iOS 시뮬레이터: Apple ID 로그인이 필요하다(설정 > 로그인). Google은 시뮬레이터 Safari 웹 로그인 경로로 열린다.

## 6. 확인 순서 (계획 6.5)

앱 두세 대로 아래를 반복한다. 매 단계 서버 쪽 상태는 `scripts/trace.sh`로 본다.

| # | 앱에서 | 서버에서 기대하는 것 | 확인 |
| --- | --- | --- | --- |
| 1 | 로그인(기존 회원) 또는 가입 시작 | `POST /api/v1/auth/social-login` 200 | backend 로그 |
| 2 | 회원가입 사인 그리기 → 제출 | 사인 PNG가 S3 staging에 → SQS → Lambda → 콜백 204 → 가입 성공 | `trace.sh <회원ID>` (users 사인 처리 상태 PROCESSING → 비움) |
| 3 | 오늘 주제 사진 올리기(WebP) | 게시물 `VALIDATING` → 콜백 후 `PENDING` | `trace.sh <업로드ID>` (post_image_uploads ISSUED → READY, S3 original·thumbnail) |
| 4 | 피드에서 이미지 확인 | 이미지 주소가 `https://cdn.<IP>.sslip.io/…` 이고 200 | 승인 후 보인다(승인 전 게시물은 다른 사람 피드에 없다) |
| 5 | 좋아요 | 카운트 증가 | `PUT /posts/{id}/likes` |
| 6 | 관리자 웹(또는 curl)에서 승인 | 알림 행 생성 | `trace.sh <알림ID>` |
| 7 | 알림함 | POST_APPROVED 1건, 미읽음 → 읽음 | notifications |
| 8 | (가짜 FCM에 발송 기록) | L4(Rust 푸시 경로) 이후에 확인 | |

관리자 승인을 앱 없이 하는 curl:
```bash
CA=local-aws/.state/caddy/data/caddy/pki/authorities/local/root.crt; H=api.<IP>.sslip.io
TOKEN=$(curl -s --cacert $CA -H 'Content-Type: application/json' -d '{"username":"operator","password":"operator-password"}' https://$H/api/v1/admin/auth/login | python3 -c 'import sys,json;print(json.load(sys.stdin)["accessToken"])')
curl -s --cacert $CA -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"status":"APPROVED"}' https://$H/api/v1/admin/posts/<postId>/moderation
```

`trace.sh`는 UUID 하나(업로드·알림·회원)를 받아 DB 행, SQS 큐 깊이·속성, S3 객체, 같은 시각의 backend·esm-poller·lambda-image 로그를 한 번에 보여 준다. `smoke.sh`는 끝에 그 세 ID로 부르는 명령을 출력한다.

## 7. 가짜 모드 스모크(`scripts/smoke.sh`)가 하는 일

앱 없이 HTTP로 같은 흐름을 돈다: 가입(사인 업로드) → 로그인·refresh → 게시물 업로드·작성 → 오늘 게시물(PENDING)·승인 전 피드 비노출 → 관리자 승인 → 피드(cdn 이미지 3종 GET) → 좋아요 등록·멱등·취소 → 알림함(POST_APPROVED, 읽음 처리). 사용자 두 명(A 작성자, B 조회자)을 새로 만들므로 여러 번 돌려도 된다. 좋아요를 승인 뒤에 둔 것은 승인 전 게시물이 다른 회원에게 보이지 않기 때문이다.

## 8. 막힐 때

| 증상 | 의심 | 확인 |
| --- | --- | --- |
| 앱이 "네트워크 오류" | 기기가 CA를 안 믿음 | 기기 브라우저로 `https://api.<IP>.sslip.io/actuator/health`; iPhone은 인증서 신뢰 설정의 전체 신뢰; Android 앱은 시스템 CA(2절) 또는 debug 패치(4절) |
| Android 빌드 실패 `... is missing from local.properties` | 필수 3값 누락 | 2절 |
| iOS가 로컬이 아닌 팀 dev 서버로 감 | `API_BASE_URL`이 https 아니거나 xcconfig `//` 주석 처리 | 3절 `https:/$()/`; backend 로그에 요청이 없다 |
| 로그인 401 `유효하지 않은 … ID Token` | 클라이언트 ID/앱키 불일치, 또는 가짜 모드 | 5절 표; `docker compose exec rust env | grep OIDC` |
| 사인/사진 업로드가 PUT에서 실패 | 기기가 `<버킷>.s3.<IP>.sslip.io`를 못 풀거나 CA 미신뢰 | 휴대폰 DNS(sslip.io 필요), 같은 Wi-Fi; Safari로 `https://s3.<IP>.sslip.io/` |
| 업로드 후 계속 VALIDATING/PROCESSING | Lambda·콜백 | `scripts/trace.sh <업로드ID>`의 esm-poller `message_deleted`/`message_kept`, lambda 로그 |
| iOS가 시작하자마자 강제 업데이트 화면 | `AppVersionGate`가 같은 번들 ID의 App Store 버전과 비교 | 3절 "앱 버전 게이트": 게이트 우회 또는 별도 번들 ID |
| iOS가 시작할 때 크래시 | 더미 `GoogleService-Info.plist`의 `API_KEY` 길이 | 39자로 맞춘다(3절) |
| Android 에뮬레이터가 어느 순간 서버에 못 닿음 | 호스트 IP가 바뀌어 DNS가 꼬임 | 2절 "실제로 안정적이었던 조합": `HOST_IP=127.0.0.1`, `adb reverse tcp:443 tcp:443`, `/system/etc/hosts` |
| 이미지가 안 보임 | cdn 이름/경로 | `curl --cacert … https://cdn.<IP>.sslip.io/posts/dev/original/<id>.webp` |

## 9. 부하 발생기에서 업로드 흐름과 동시성(S2)

`harness/loadgen`의 `uploads` 명령이 로컬 AWS 대상으로 presign → PUT → 콜백 대기를 돈다. 앱 두세 대로 6.5를 반복하는 동안 배경 부하로 쓴다. 명령과 옵션, 측정 예는 [`local-aws/README.md`](../../local-aws/README.md)의 "동시성 (ESM_CONCURRENCY)"과 "부하 발생기 업로드 흐름 (S2)" 절에 있다.

```bash
ESM_CONCURRENCY=4 docker compose --profile rust up -d          # Lambda·poller 동시성
target/release/chalkak-loadgen uploads --base 127.0.0.1:18080 --s3 127.0.0.1:4566 --secret <ACCESS_TOKEN_SECRET hex> \
  --db "postgres://chalkak:local-aws-password@127.0.0.1:15440/chalkak" --rate 3 --duration 1m
```
