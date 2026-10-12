// Rust가 발급한 토큰을 원본 Java 검증 코드로 검증한다.
//
// 실행: ./run-java.sh VerifyRustTokens.java rust-tokens.json
// 입력 rust-tokens.json의 각 사례는 {name, kind, token, expect: "accept"|"reject"}다. expect는 Rust 쪽 판정이고,
// 이 프로그램은 원본 판정이 같은지 확인해 다르면 종료 코드 1로 끝난다.
//
// 근거 클래스:
//   kind=access  JwtAccessTokenProvider#jwtDecoder   (HS256 + 30초 허용 오차 + issuer/aud/purpose/sub 검증)
//   kind=signup  JwtSocialSignupTokenProvider#verify (같은 구성 + iat/exp 필수)
//   kind=es256   Nimbus ECDSAVerifier                (AppleClientSecretGenerator가 서명한 토큰과 같은 방식으로 검증)
// 시각은 입력의 now_ms로 고정한다(원본 생성자에 Clock.fixed를 넣는다).
import com.chalkak.backend.auth.infrastructure.infra.access.*;
import com.chalkak.backend.auth.infrastructure.infra.signup.*;
import com.nimbusds.jose.JWSAlgorithm;
import com.nimbusds.jose.crypto.ECDSAVerifier;
import com.nimbusds.jose.jwk.ECKey;
import com.nimbusds.jose.util.JSONObjectUtils;
import com.nimbusds.jwt.SignedJWT;
import java.lang.reflect.Constructor;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.*;
import java.util.List;
import java.util.Map;

public class VerifyRustTokens {

    static Object construct(Class<?> type, Object... args) throws Exception {
        for (Constructor<?> c : type.getDeclaredConstructors()) {
            if (c.getParameterCount() == args.length) {
                c.setAccessible(true);
                return c.newInstance(args);
            }
        }
        throw new IllegalStateException("constructor not found: " + type);
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Map<String, Object> input = JSONObjectUtils.parse(Files.readString(Path.of(args[0]), StandardCharsets.UTF_8));
        Clock clock = Clock.fixed(Instant.ofEpochMilli(((Number) input.get("now_ms")).longValue()), ZoneOffset.UTC);

        JwtAccessTokenProvider access = (JwtAccessTokenProvider) construct(
                JwtAccessTokenProvider.class,
                new AccessTokenProperties("chalkak-backend", "chalkak-access", Duration.ofMinutes(15),
                        (String) input.get("access_secret_hex")),
                clock);
        JwtSocialSignupTokenProvider signup = (JwtSocialSignupTokenProvider) construct(
                JwtSocialSignupTokenProvider.class,
                new SocialSignupTokenProperties("chalkak-backend", "chalkak-social-signup", Duration.ofMinutes(5),
                        (String) input.get("signup_secret_hex")),
                clock);
        ECKey ecKey = ECKey.parse(JSONObjectUtils.toJSONString((Map<String, Object>) input.get("es256_public_jwk")));

        int mismatches = 0;
        for (Object o : (List<Object>) input.get("cases")) {
            Map<String, Object> c = (Map<String, Object>) o;
            String name = (String) c.get("name");
            String token = (String) c.get("token");
            String kind = (String) c.get("kind");
            String verdict;
            String detail = "";
            try {
                switch (kind) {
                    case "access" -> access.jwtDecoder().decode(token);
                    case "signup" -> signup.verify(token);
                    case "es256" -> {
                        SignedJWT jwt = SignedJWT.parse(token);
                        if (!JWSAlgorithm.ES256.equals(jwt.getHeader().getAlgorithm())
                                || !jwt.verify(new ECDSAVerifier(ecKey))) {
                            throw new IllegalStateException("signature rejected");
                        }
                    }
                    default -> throw new IllegalArgumentException(kind);
                }
                verdict = "accept";
            } catch (Throwable e) {
                verdict = "reject";
                detail = e.getClass().getSimpleName() + ": " + e.getMessage();
            }
            boolean same = verdict.equals(c.get("expect"));
            if (!same) mismatches++;
            System.out.println((same ? "OK       " : "MISMATCH ") + String.format("%-48s", name) + " java=" + verdict
                    + " rust=" + c.get("expect") + (detail.isEmpty() ? "" : "  (" + detail + ")"));
        }
        System.out.println(mismatches == 0 ? "ALL MATCH" : mismatches + " MISMATCH");
        System.exit(mismatches == 0 ? 0 : 1);
    }
}
