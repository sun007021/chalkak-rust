// 원본 코드로 토큰을 만들어 oracle.json에 기록한다. Rust 테스트가 이 기대값을 그대로 쓴다.
//
// 실행: ./run-java.sh GenFixtures.java > oracle.json
//
// 근거 클래스(원본 backend):
//   auth.infrastructure.infra.access.JwtAccessTokenProvider      (HS256, NimbusJwtEncoder)
//   auth.infrastructure.infra.signup.JwtSocialSignupTokenProvider (HS256, NimbusJwtEncoder)
//   auth.infrastructure.infra.apple.AppleClientSecretGenerator    (ES256, Nimbus ECDSASigner)
//   RS256은 OidcIdTokenDecoderFactory가 쓰는 Nimbus RSASSASigner/JWKSet 형식을 그대로 쓴다.
//
// 시각은 고정한다. HS256 토큰은 jti만 무작위(UUID.randomUUID)라 값을 함께 기록하고,
// Rust가 같은 jti로 발급해 바이트가 같은지 비교한다.
import com.chalkak.backend.auth.domain.*;
import com.chalkak.backend.auth.infrastructure.infra.access.*;
import com.chalkak.backend.auth.infrastructure.infra.apple.*;
import com.chalkak.backend.auth.infrastructure.infra.oidc.apple.AppleOidcProperties;
import com.chalkak.backend.auth.infrastructure.infra.signup.*;
import com.nimbusds.jose.*;
import com.nimbusds.jose.crypto.*;
import com.nimbusds.jose.jwk.*;
import com.nimbusds.jose.jwk.gen.*;
import com.nimbusds.jose.util.JSONObjectUtils;
import com.nimbusds.jwt.*;
import java.lang.reflect.Constructor;
import java.nio.charset.StandardCharsets;
import java.security.interfaces.ECPrivateKey;
import java.time.*;
import java.util.*;

public class GenFixtures {

    static final Instant NOW = Instant.parse("2026-10-12T00:00:00Z");
    static final Clock CLOCK = Clock.fixed(NOW, ZoneOffset.UTC);
    static final String ACCESS_SECRET = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    static final String SIGNUP_SECRET = "f0e1d2c3b4a5968778695a4b3c2d1e0f00112233445566778899aabbccddeeff";

    static Object construct(Class<?> type, Object... args) throws Exception {
        for (Constructor<?> c : type.getDeclaredConstructors()) {
            if (c.getParameterCount() == args.length) {
                c.setAccessible(true);
                return c.newInstance(args);
            }
        }
        throw new IllegalStateException("constructor not found: " + type);
    }

    static String jti(String token) throws Exception {
        return SignedJWT.parse(token).getJWTClaimsSet().getJWTID();
    }

    public static void main(String[] args) throws Exception {
        Map<String, Object> out = new LinkedHashMap<>();
        out.put("now_epoch", NOW.getEpochSecond());

        // 원본 액세스 토큰 (JwtAccessTokenProvider)
        AccessTokenProperties accessProps = new AccessTokenProperties(
                "chalkak-backend", "chalkak-access", Duration.ofMinutes(15), ACCESS_SECRET);
        JwtAccessTokenProvider access = (JwtAccessTokenProvider) construct(
                JwtAccessTokenProvider.class, accessProps, CLOCK);
        Map<String, Object> accessOut = new LinkedHashMap<>();
        accessOut.put("secret_hex", ACCESS_SECRET);
        accessOut.put("issuer", "chalkak-backend");
        accessOut.put("audience", "chalkak-access");
        accessOut.put("expiration_secs", 900);
        List<Object> accessTokens = new ArrayList<>();
        for (AccessTokenScope scope : AccessTokenScope.values()) {
            UUID sub = UUID.fromString(scope == AccessTokenScope.USER
                    ? "01990000-0000-7000-8000-000000000001"
                    : "01990000-0000-7000-8000-0000000000a1");
            String token = access.issue(sub, scope).value();
            // 우리가 만든 토큰을 원본 디코더가 그대로 받는지도 같이 확인한다.
            access.jwtDecoder().decode(token);
            Map<String, Object> t = new LinkedHashMap<>();
            t.put("scope", scope.name());
            t.put("sub", sub.toString());
            t.put("jti", jti(token));
            t.put("token", token);
            accessTokens.add(t);
        }
        accessOut.put("tokens", accessTokens);
        out.put("access", accessOut);

        // 소셜 가입 토큰 (JwtSocialSignupTokenProvider)
        SocialSignupTokenProperties signupProps = new SocialSignupTokenProperties(
                "chalkak-backend", "chalkak-social-signup", Duration.ofMinutes(5), SIGNUP_SECRET);
        JwtSocialSignupTokenProvider signup = (JwtSocialSignupTokenProvider) construct(
                JwtSocialSignupTokenProvider.class, signupProps, CLOCK);
        Map<String, Object> signupOut = new LinkedHashMap<>();
        signupOut.put("secret_hex", SIGNUP_SECRET);
        signupOut.put("issuer", "chalkak-backend");
        signupOut.put("audience", "chalkak-social-signup");
        signupOut.put("expiration_secs", 300);
        List<Object> signupTokens = new ArrayList<>();
        UUID uploadId = UUID.fromString("01990000-0000-7000-8000-0000000000b1");
        for (String email : new String[] {"user@example.com", null}) {
            VerifiedSocialIdentity identity = new VerifiedSocialIdentity(
                    SocialProvider.KAKAO, "kakao-subject-12345", email);
            String token = signup.issue(identity, uploadId).value();
            signup.verify(token);
            Map<String, Object> t = new LinkedHashMap<>();
            t.put("provider", "KAKAO");
            t.put("sub", "kakao-subject-12345");
            t.put("email", email);
            t.put("upload_id", uploadId.toString());
            t.put("jti", jti(token));
            t.put("token", token);
            signupTokens.add(t);
        }
        // JSON 문자열 이스케이프 규칙(Nimbus의 Gson 직렬화)을 고정하기 위한 특수 문자 사례.
        String trickySubject = "s\"\\/\b\f\n\r\t\u0001\u001f\u007f <>&'=\u00e9\ud55c\uae00\u2028\u2029\ud83d\ude00";
        String trickyEmail = "o'neil+tag&x=<y>@example.com";
        {
            VerifiedSocialIdentity identity = new VerifiedSocialIdentity(
                    SocialProvider.APPLE, trickySubject, trickyEmail);
            String token = signup.issue(identity, uploadId).value();
            signup.verify(token);
            Map<String, Object> t = new LinkedHashMap<>();
            t.put("provider", "APPLE");
            t.put("sub", trickySubject);
            t.put("email", trickyEmail);
            t.put("upload_id", uploadId.toString());
            t.put("jti", jti(token));
            t.put("token", token);
            signupTokens.add(t);
        }
        signupOut.put("tokens", signupTokens);
        out.put("signup", signupOut);

        // RS256 (ID 토큰 검증): 키 두 개로 키 회전을 흉내 낸다.
        RSAKey k1 = new RSAKeyGenerator(2048).keyID("kid-1").keyUse(KeyUse.SIGNATURE)
                .algorithm(JWSAlgorithm.RS256).generate();
        RSAKey k2 = new RSAKeyGenerator(2048).keyID("kid-2").keyUse(KeyUse.SIGNATURE)
                .algorithm(JWSAlgorithm.RS256).generate();
        Map<String, Object> rs = new LinkedHashMap<>();
        rs.put("jwks_k1", new JWKSet(k1.toPublicJWK()).toString());
        rs.put("jwks_k1_k2", new JWKSet(List.of(k1.toPublicJWK(), k2.toPublicJWK())).toString());
        List<Object> rsTokens = new ArrayList<>();
        for (RSAKey k : new RSAKey[] {k1, k2}) {
            JWTClaimsSet claims = new JWTClaimsSet.Builder()
                    .issuer("https://accounts.google.com")
                    .audience("google-client-id")
                    .subject("google-subject")
                    .issueTime(Date.from(NOW))
                    .expirationTime(Date.from(NOW.plusSeconds(3600)))
                    .claim("nonce", "abc")
                    .claim("email", "user@example.com")
                    .claim("email_verified", true)
                    .build();
            SignedJWT jwt = new SignedJWT(
                    new JWSHeader.Builder(JWSAlgorithm.RS256).keyID(k.getKeyID()).type(JOSEObjectType.JWT).build(),
                    claims);
            jwt.sign(new RSASSASigner(k));
            Map<String, Object> t = new LinkedHashMap<>();
            t.put("kid", k.getKeyID());
            t.put("token", jwt.serialize());
            rsTokens.add(t);
        }
        rs.put("tokens", rsTokens);
        out.put("rs256", rs);

        // ES256 (Apple client secret): 실제 AppleClientSecretGenerator로 만든다.
        ECKey ec = new ECKeyGenerator(Curve.P_256).keyID("APPLEKEY01").generate();
        String pem = "-----BEGIN PRIVATE KEY-----\n"
                + Base64.getMimeEncoder(64, "\n".getBytes(StandardCharsets.US_ASCII))
                        .encodeToString(ec.toECPrivateKey().getEncoded())
                + "\n-----END PRIVATE KEY-----\n";
        String pemBase64 = Base64.getEncoder().encodeToString(pem.getBytes(StandardCharsets.US_ASCII));
        AppleClientSecretGenerator apple = new AppleClientSecretGenerator(
                new AppleTokenProperties("APPLETEAM1", "APPLEKEY01", pemBase64,
                        "https://appleid.apple.com/auth/token", "https://appleid.apple.com/auth/revoke"),
                new AppleOidcProperties("https://appleid.apple.com", "https://appleid.apple.com/auth/keys",
                        "com.chalkak.ios"),
                CLOCK);
        Map<String, Object> es = new LinkedHashMap<>();
        es.put("private_key_pem", pem);
        es.put("public_jwk", ec.toPublicJWK().toJSONObject());
        es.put("team_id", "APPLETEAM1");
        es.put("key_id", "APPLEKEY01");
        es.put("client_id", "com.chalkak.ios");
        // ECDSA는 서명마다 k가 달라 바이트 비교는 못 한다. Rust가 공개키로 검증한다.
        es.put("client_secret", apple.generate());
        out.put("es256", es);

        System.out.println(JSONObjectUtils.toJSONString(out));
    }
}
