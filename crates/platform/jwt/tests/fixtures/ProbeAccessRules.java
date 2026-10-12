// 원본 JwtAccessTokenProvider / JwtSocialSignupTokenProvider 디코더가 클레임 조합마다 어떻게 판정하는지 출력한다.
// Rust Validator 테스트의 기대값은 이 출력에서 가져왔다. (실행: ./run-java.sh ProbeAccessRules.java)
//
// 근거 클래스: JwtAccessTokenProvider#createJwtDecoder, JwtSocialSignupTokenProvider#createJwtDecoder
//   - NimbusJwtDecoder.withSecretKey(...).macAlgorithm(HS256)
//   - DelegatingOAuth2TokenValidator(JwtTimestampValidator(30s), JwtIssuerValidator, audience, purpose, ...)
import com.chalkak.backend.auth.infrastructure.infra.access.*;
import com.chalkak.backend.auth.infrastructure.infra.signup.*;
import com.nimbusds.jose.*;
import com.nimbusds.jose.crypto.*;
import com.nimbusds.jwt.*;
import java.lang.reflect.Constructor;
import java.time.*;
import java.util.*;
import org.springframework.security.oauth2.jwt.JwtDecoder;

public class ProbeAccessRules {

    static final String HEX = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    static final String SUB = "01990000-0000-7000-8000-000000000001";

    static long now() { return Instant.now().getEpochSecond(); }

    static String sign(JWSHeader header, Map<String, Object> claims) throws Exception {
        JWTClaimsSet.Builder b = new JWTClaimsSet.Builder();
        for (var e : claims.entrySet()) b.claim(e.getKey(), e.getValue());
        SignedJWT jwt = new SignedJWT(header, b.build());
        jwt.sign(new MACSigner(HexFormat.of().parseHex(HEX)));
        return jwt.serialize();
    }

    static Map<String, Object> access() {
        Map<String, Object> m = new LinkedHashMap<>();
        m.put("iss", "chalkak-backend"); m.put("aud", "chalkak-access"); m.put("sub", SUB);
        m.put("iat", now() - 60); m.put("exp", now() + 840); m.put("jti", "j");
        m.put("purpose", "ACCESS"); m.put("scope", "USER");
        return m;
    }

    static Map<String, Object> signup() {
        Map<String, Object> m = new LinkedHashMap<>();
        m.put("iss", "chalkak-backend"); m.put("aud", "chalkak-social-signup"); m.put("sub", "s");
        m.put("iat", now() - 60); m.put("exp", now() + 240); m.put("jti", "j");
        m.put("purpose", "SOCIAL_SIGNUP"); m.put("provider", "KAKAO");
        m.put("uploadId", "01990000-0000-7000-8000-0000000000b1");
        return m;
    }

    static void run(String label, JwtDecoder dec, Map<String, Object> claims) throws Exception {
        run(label, dec, new JWSHeader.Builder(JWSAlgorithm.HS256).type(JOSEObjectType.JWT).build(), claims);
    }

    static void run(String label, JwtDecoder dec, JWSHeader h, Map<String, Object> claims) throws Exception {
        String r;
        try { dec.decode(sign(h, claims)); r = "OK"; }
        catch (Throwable e) { r = "FAIL " + e.getClass().getSimpleName() + ": " + e.getMessage(); }
        System.out.println(String.format("%-34s %s", label, r));
    }

    interface Edit { void apply(Map<String, Object> m); }

    static void cases(String kind, JwtDecoder dec, Map<String, Object> base) throws Exception {
        System.out.println("== " + kind);
        run("baseline", dec, new LinkedHashMap<>(base));
        Object[][] edits = {
            {"exp 10s ago, iat 900s ago", (Edit) m -> { m.put("exp", now() - 10); m.put("iat", now() - 900); }},
            {"exp 29s ago, iat 900s ago", (Edit) m -> { m.put("exp", now() - 29); m.put("iat", now() - 900); }},
            {"exp 31s ago, iat 900s ago", (Edit) m -> { m.put("exp", now() - 31); m.put("iat", now() - 900); }},
            {"exp 10s ago, iat now (exp<iat)", (Edit) m -> { m.put("exp", now() - 10); m.put("iat", now()); }},
            {"exp == iat", (Edit) m -> { m.put("exp", now() + 100); m.put("iat", now() + 100); }},
            {"no exp", (Edit) m -> m.remove("exp")},
            {"no iat", (Edit) m -> m.remove("iat")},
            {"iat +29s", (Edit) m -> { m.put("iat", now() + 29); m.put("exp", now() + 300); }},
            {"iat +31s", (Edit) m -> { m.put("iat", now() + 31); m.put("exp", now() + 300); }},
            {"nbf +29s", (Edit) m -> m.put("nbf", now() + 29)},
            {"nbf +31s", (Edit) m -> m.put("nbf", now() + 31)},
            {"no iss", (Edit) m -> m.remove("iss")},
            {"iss other", (Edit) m -> m.put("iss", "other")},
            {"no aud", (Edit) m -> m.remove("aud")},
            {"aud []", (Edit) m -> m.put("aud", List.of())},
            {"aud other", (Edit) m -> m.put("aud", "other")},
            {"aud [expected, other]", (Edit) m -> m.put("aud", List.of(m.get("aud"), "other"))},
            {"no purpose", (Edit) m -> m.remove("purpose")},
            {"purpose other", (Edit) m -> m.put("purpose", "OTHER")},
            {"no sub", (Edit) m -> m.remove("sub")},
            {"sub blank", (Edit) m -> m.put("sub", " ")},
            {"sub not uuid", (Edit) m -> m.put("sub", "not-a-uuid")},
            {"sub UUID uppercase", (Edit) m -> m.put("sub", SUB.toUpperCase())},
            {"sub 1-2-3-4-5", (Edit) m -> m.put("sub", "1-2-3-4-5")},
            {"no jti", (Edit) m -> m.remove("jti")},
            {"no provider", (Edit) m -> m.remove("provider")},
            {"no uploadId", (Edit) m -> m.remove("uploadId")},
        };
        for (Object[] e : edits) {
            Map<String, Object> m = new LinkedHashMap<>(base);
            ((Edit) e[1]).apply(m);
            run((String) e[0], dec, m);
        }
        run("typ absent", dec, new JWSHeader.Builder(JWSAlgorithm.HS256).build(), new LinkedHashMap<>(base));
        run("typ at+jwt", dec, new JWSHeader.Builder(JWSAlgorithm.HS256).type(new JOSEObjectType("at+jwt")).build(),
                new LinkedHashMap<>(base));
        run("alg HS384", dec, new JWSHeader.Builder(JWSAlgorithm.HS256).build(), new LinkedHashMap<>(base));
    }

    public static void main(String[] args) throws Exception {
        Constructor<JwtAccessTokenProvider> ac = JwtAccessTokenProvider.class
                .getDeclaredConstructor(AccessTokenProperties.class, Clock.class);
        ac.setAccessible(true);
        JwtAccessTokenProvider access = ac.newInstance(
                new AccessTokenProperties("chalkak-backend", "chalkak-access", Duration.ofMinutes(15), HEX),
                Clock.systemUTC());
        cases("access token", access.jwtDecoder(), access());

        Constructor<JwtSocialSignupTokenProvider> sc = JwtSocialSignupTokenProvider.class
                .getDeclaredConstructor(SocialSignupTokenProperties.class, Clock.class);
        sc.setAccessible(true);
        JwtSocialSignupTokenProvider signup = sc.newInstance(
                new SocialSignupTokenProperties("chalkak-backend", "chalkak-social-signup", Duration.ofMinutes(5), HEX),
                Clock.systemUTC());
        // verify()는 UnauthorizedException으로 감싸므로 디코더를 직접 꺼내 쓸 수 없다. 필드를 꺼낸다.
        var f = JwtSocialSignupTokenProvider.class.getDeclaredField("jwtDecoder");
        f.setAccessible(true);
        cases("social signup token", (JwtDecoder) f.get(signup), signup());
    }
}
