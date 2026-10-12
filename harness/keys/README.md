# 하네스 테스트 OIDC 키

`oidc-test-private.pem`은 하네스 전용 RSA 개인키다. 실제 서비스의 키가 아니며 비밀이 아니다.
러너(`harness/parity`)가 이 키로 ID Token을 RS256 서명하고, 가짜 서버(`harness/fakes`)가 `jwks.json`으로
공개키를 내놓는다. 두 파일은 한 쌍이어야 하며 `harness/parity`의 단위 테스트가 짝을 확인한다.
