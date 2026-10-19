# 하네스 테스트 OIDC 키

`oidc-test-private.pem`은 하네스 전용 RSA 개인키다. 실제 서비스의 키가 아니며 비밀이 아니다.
러너(`harness/parity`)가 이 키로 ID Token을 RS256 서명하고, 가짜 서버(`harness/fakes`)가 `jwks.json`으로
공개키를 내놓는다. 두 파일은 한 쌍이어야 하며 `harness/parity`의 단위 테스트가 짝을 확인한다.

## FCM 서비스 계정 (`fcm-service-account.json`)

worker lane(`WORKER=1 harness/scripts/lane.sh up ...`)의 Java·Rust 앱이 읽는 Firebase 서비스 계정 모양의 파일이다. 실제 계정이
아니며 비밀이 아니다. `private_key`는 `crates/app/tests/fixtures/fcm-test-key.pem`과 같은 테스트 전용 RSA 키이고
`token_uri`는 가짜 서버(`http://fakes:9000/oauth2/token`)다. 두 앱은 이 파일을 `/secrets/fcm-service-account.json`으로 읽는다.
