// 원본 JWKS 소스 체인의 캐시·재조회·속도 제한·장애 허용 동작을 실제 Nimbus로 관찰한다.
// Rust JwksCache 테스트(tests/jwks_cache.rs)의 기대값 근거다. (실행: ./run-java.sh ProbeJwksChain.java)
//
// 근거 클래스: OidcIdTokenDecoderFactory#createJwkSource (원본 메서드를 그대로 호출한다. 시간은 실제 시간이다)
//  1) 기본 설정(캐시 5분, 구간 30초 2회)에서 시간이 필요 없는 규칙: 첫 조회, 모르는 kid 재조회, 속도 제한, 재시도,
//     기동 직후 연속 실패
//  2) 시간이 걸리는 규칙(캐시 만료, 장애 시 이전 목록 사용, 50분 한도)은 같은 체인을 JWKSourceBuilder로 조립하되
//     시간만 1/100로 줄여 관찰한다(캐시 3초, 구간 1초, 장애 허용 10초 — 원본 5분/30초/50분과 같은 비율은 아니다).
import com.chalkak.backend.auth.domain.SocialProvider;
import com.nimbusds.jose.JWSAlgorithm;
import com.nimbusds.jose.JWSHeader;
import com.nimbusds.jose.jwk.JWKMatcher;
import com.nimbusds.jose.jwk.JWKSelector;
import com.nimbusds.jose.jwk.RSAKey;
import com.nimbusds.jose.jwk.gen.RSAKeyGenerator;
import com.nimbusds.jose.jwk.source.JWKSource;
import com.nimbusds.jose.jwk.source.JWKSourceBuilder;
import com.nimbusds.jose.proc.SecurityContext;
import com.nimbusds.jose.util.Resource;
import com.nimbusds.jose.util.ResourceRetriever;
import java.io.IOException;
import java.lang.reflect.Method;
import java.net.URI;
import java.net.URL;
import java.util.ArrayDeque;
import java.util.Deque;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

public class ProbeJwksChain {

    static final AtomicInteger CALLS = new AtomicInteger();
    static final Deque<Object> SCRIPT = new ArrayDeque<>(); // IOException이면 실패, String이면 그 본문
    static String body;

    static class Retriever implements ResourceRetriever {
        public Resource retrieveResource(URL url) throws IOException {
            CALLS.incrementAndGet();
            Object next = SCRIPT.poll();
            if (next instanceof IOException e) throw e;
            return new Resource(next instanceof String s ? s : body, "application/json");
        }
    }

    static String jwks(String... kids) throws Exception {
        StringBuilder sb = new StringBuilder("{\"keys\":[");
        for (int i = 0; i < kids.length; i++) {
            RSAKey k = new RSAKeyGenerator(2048).keyID(kids[i]).generate();
            sb.append(i > 0 ? "," : "").append(k.toPublicJWK().toJSONString());
        }
        return sb.append("]}").toString();
    }

    static String select(JWKSource<SecurityContext> source, String kid) {
        JWSHeader h = new JWSHeader.Builder(JWSAlgorithm.RS256).keyID(kid).build();
        try {
            int before = CALLS.get();
            var keys = source.get(new JWKSelector(JWKMatcher.forJWSHeader(h)), null);
            return "kid=" + kid + " -> " + (keys.isEmpty() ? "no key" : "key") + "  (fetches " + (CALLS.get() - before) + ")";
        } catch (Exception e) {
            return "kid=" + kid + " -> " + e.getClass().getSimpleName() + ": " + e.getMessage();
        }
    }

    static void fail(int n) { for (int i = 0; i < n; i++) SCRIPT.add(new IOException("down")); }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Class<?> f = Class.forName("com.chalkak.backend.auth.infrastructure.infra.oidc.OidcIdTokenDecoderFactory");
        Method create = f.getDeclaredMethod("createJwkSource", SocialProvider.class, String.class, ResourceRetriever.class);
        create.setAccessible(true);
        URL url = URI.create("https://example.invalid/jwks").toURL();

        System.out.println("== 1) 원본 createJwkSource, 기본 설정");
        body = jwks("k1");
        JWKSource<SecurityContext> s = (JWKSource<SecurityContext>) create.invoke(null, SocialProvider.GOOGLE, url.toString(), new Retriever());
        System.out.println(select(s, "k1") + "   <- 첫 조회");
        System.out.println(select(s, "k1") + "   <- 캐시");
        System.out.println(select(s, "x1") + "   <- 모르는 kid, 재조회(구간 2회째)");
        System.out.println(select(s, "x2") + "   <- 3회째는 제한");
        System.out.println(select(s, "k1") + "   <- 캐시된 kid는 제한과 무관");

        CALLS.set(0);
        body = jwks("k1");
        s = (JWKSource<SecurityContext>) create.invoke(null, SocialProvider.GOOGLE, url.toString(), new Retriever());
        fail(1);
        System.out.println(select(s, "k1") + "   <- 첫 조회에서 한 번 실패하면 재시도");

        CALLS.set(0);
        s = (JWKSource<SecurityContext>) create.invoke(null, SocialProvider.GOOGLE, url.toString(), new Retriever());
        fail(2);
        System.out.println(select(s, "k1") + "   <- 두 번 실패(기동 직후)");
        fail(2);
        System.out.println(select(s, "k1") + "   <- 두 번째 요청도 실패");
        System.out.println(select(s, "k1") + "   <- 세 번째는 구간 소진으로 제한");

        CALLS.set(0);
        s = (JWKSource<SecurityContext>) create.invoke(null, SocialProvider.GOOGLE, url.toString(), new Retriever());
        SCRIPT.add("not json");
        SCRIPT.add("not json");
        System.out.println(select(s, "k1") + "   <- 본문 해석 실패도 재시도 대상");

        System.out.println("== 2) 같은 체인, 시간 축소(캐시 3초, 구간 1초, 장애 허용 10초)");
        CALLS.set(0);
        body = jwks("k1");
        JWKSource<SecurityContext> t = JWKSourceBuilder.<SecurityContext>create(url, new Retriever())
                .cache(3000, 15000).refreshAheadCache(false).rateLimited(1000).retrying(true)
                .outageTolerant(10000).build();
        System.out.println("t=0s    " + select(t, "k1"));
        Thread.sleep(3200);
        fail(2);
        System.out.println("t=3.2s  " + select(t, "k1") + "   <- 만료 후 두 번 실패, 이전 목록 사용");
        System.out.println("t=3.2s  " + select(t, "k1") + "   <- 이전 목록이 캐시를 다시 채움");
        fail(2);
        System.out.println("t=3.2s  " + select(t, "k2") + "   <- 장애 중 모르는 kid");
        Thread.sleep(3200);
        fail(2);
        System.out.println("t=6.4s  " + select(t, "k1"));
        Thread.sleep(3200);
        fail(2);
        System.out.println("t=9.6s  " + select(t, "k1"));
        Thread.sleep(3200);
        fail(2);
        System.out.println("t=12.8s " + select(t, "k1") + "   <- 마지막 성공(t=0)으로부터 10초 초과");
    }
}
