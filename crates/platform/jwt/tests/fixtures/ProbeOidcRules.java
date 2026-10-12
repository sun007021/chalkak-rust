// 원본 OidcIdTokenDecoderFactory가 만드는 ID 토큰 디코더(Spring NimbusJwtDecoder + 원본 validator 조합)가
// 클레임·헤더·JWK 조합마다 어떻게 판정하는지 출력한다. Rust RS256 검증과 Validator 테스트의 기대값 근거다.
// (실행: ./run-java.sh ProbeOidcRules.java)
//
// 근거 클래스: OidcIdTokenDecoderFactory#createDecoder, OidcIdTokenAudienceValidator, OidcIdTokenClaimsValidator,
//   Spring JwtValidators.createDefaultWithIssuer(JwtTimestampValidator 60s + JwtIssuerValidator)
import com.chalkak.backend.auth.domain.SocialProvider;
import com.nimbusds.jose.*; import com.nimbusds.jose.crypto.*; import com.nimbusds.jose.jwk.*; import com.nimbusds.jose.jwk.gen.*;
import com.nimbusds.jose.jwk.source.*; import com.nimbusds.jose.proc.SecurityContext; import com.nimbusds.jwt.*;
import org.springframework.security.oauth2.jwt.*;
import java.lang.reflect.*; import java.time.*; import java.util.*;

public class ProbeOidcRules {
  static RSAKey key; static JwtDecoder dec;
  static long now() { return Instant.now().getEpochSecond(); }
  static String tok(JWSHeader h, Map<String,Object> m) throws Exception {
    JWTClaimsSet.Builder b = new JWTClaimsSet.Builder();
    for (var e : m.entrySet()) b.claim(e.getKey(), e.getValue());
    var j = new SignedJWT(h, b.build()); j.sign(new RSASSASigner(key)); return j.serialize();
  }
  static Map<String,Object> base() {
    Map<String,Object> m = new LinkedHashMap<>();
    m.put("iss","https://iss"); m.put("aud","A"); m.put("sub","s"); m.put("iat", now()); m.put("exp", now()+300);
    return m;
  }
  static void t(String name, JWSHeader h, Map<String,Object> m) throws Exception {
    try { dec.decode(tok(h,m)); System.out.println("OK    "+name); }
    catch (Exception e) { System.out.println("FAIL  "+name+"  :: "+e.getMessage()); }
  }
  static JWSHeader h() { return new JWSHeader.Builder(JWSAlgorithm.RS256).keyID("k1").build(); }
  public static void main(String[] a) throws Exception {
    key = new RSAKeyGenerator(2048).keyID("k1").keyUse(KeyUse.SIGNATURE).generate();
    JWKSet set = new JWKSet(key.toPublicJWK());
    Class<?> f = Class.forName("com.chalkak.backend.auth.infrastructure.infra.oidc.OidcIdTokenDecoderFactory");
    Method m = f.getDeclaredMethod("createDecoder", SocialProvider.class, String.class, String.class, JWKSource.class);
    m.setAccessible(true);
    dec = (JwtDecoder) m.invoke(null, SocialProvider.GOOGLE, "https://iss", "A", new ImmutableJWKSet<SecurityContext>(set));
    Map<String,Object> x;
    t("baseline", h(), base());
    for (long d : new long[]{0, 59, 60, 61, 65}) { x=base(); x.put("exp", now()-d); x.put("iat", now()-d-100); t("exp "+d+"s ago", h(), x); }
    for (long d : new long[]{30, 59, 61, 65}) { x=base(); x.put("nbf", now()+d); t("nbf +"+d, h(), x); }
    for (long d : new long[]{30, 59, 61, 65}) { x=base(); x.put("iat", now()+d); x.put("exp", now()+d+300); t("iat +"+d, h(), x); }
    x=base(); x.remove("exp"); t("no exp", h(), x);
    x=base(); x.remove("iat"); t("no iat", h(), x);
    x=base(); x.put("exp", now()-10); t("exp 10s ago iat now", h(), x);
    x=base(); x.put("iss","https://other"); t("iss mismatch", h(), x);
    x=base(); x.remove("iss"); t("no iss", h(), x);
    x=base(); x.put("aud", List.of("A","B")); t("aud [A,B]", h(), x);
    x=base(); x.put("aud", List.of("A")); t("aud [A]", h(), x);
    x=base(); x.put("aud", "B"); t("aud B", h(), x);
    x=base(); x.remove("aud"); t("no aud", h(), x);
    t("no kid", new JWSHeader.Builder(JWSAlgorithm.RS256).build(), base());
    t("unknown kid", new JWSHeader.Builder(JWSAlgorithm.RS256).keyID("zz").build(), base());
    t("typ foo", new JWSHeader.Builder(JWSAlgorithm.RS256).keyID("k1").type(new JOSEObjectType("foo")).build(), base());
    t("crit", new JWSHeader.Builder(JWSAlgorithm.RS256).keyID("k1").criticalParams(Set.of("x")).customParam("x",1).build(), base());
    // alg other
    try { var j = new SignedJWT(new JWSHeader.Builder(JWSAlgorithm.RS384).keyID("k1").build(), new JWTClaimsSet.Builder().issuer("https://iss").audience("A").issueTime(new Date()).expirationTime(Date.from(Instant.now().plusSeconds(300))).build()); j.sign(new RSASSASigner(key)); dec.decode(j.serialize()); System.out.println("OK RS384"); } catch (Exception e) { System.out.println("FAIL  RS384 :: "+e.getMessage()); }
    // jwk matcher
    System.out.println("matcher: "+JWKMatcher.forJWSHeader(h()));
    for (String ty : new String[]{"jwt","JWT","Jwt","at+jwt","JOSE"}) t("typ "+ty, new JWSHeader.Builder(JWSAlgorithm.RS256).keyID("k1").type(new JOSEObjectType(ty)).build(), base());
    x=base(); x.put("exp", String.valueOf(now()+300)); t("exp as string", h(), x);
    x=base(); x.put("exp", (double)(now()+300)+0.7); t("exp float", h(), x);
    x=base(); x.put("iat", (double)now()+0.7); t("iat float", h(), x);
    x=base(); x.put("nbf", "abc"); t("nbf string", h(), x);
    x=base(); x.put("aud", 5); t("aud number", h(), x);
    x=base(); x.put("aud", List.of(5)); t("aud [5]", h(), x);
    x=base(); x.put("iss", 5); t("iss number", h(), x);
    x=base(); x.put("sub", 5); t("sub number", h(), x);
    x=base(); x.put("exp", now()+300); x.put("iat", now()+300); t("exp==iat", h(), x);
    x=base(); x.put("exp", now()+300); x.put("iat", now()+301); x.put("nbf", now()-5); t("exp<iat", h(), x);
    x=base(); x.put("nbf", now()+5); t("nbf+5", h(), x);
    x=base(); x.put("exp", null); t("exp null", h(), x);
    x=base(); x.put("iat", null); t("iat null", h(), x);
    x=base(); x.put("aud", null); t("aud null", h(), x);
    x=base(); x.put("aud", List.of()); t("aud []", h(), x);
    x=base(); x.put("aud", List.of("A","A")); t("aud [A,A]", h(), x);
    // jwk set parse variants
    String[] sets = {
      "{\"keys\":[{\"kty\":\"oct\",\"k\":\"AAAA\",\"kid\":\"o\"}]}",
      "{\"keys\":[{\"kty\":\"FOO\",\"kid\":\"o\"}]}",
      "{\"keys\":[{\"kty\":\"RSA\",\"kid\":\"o\"}]}",
      "{\"keys\":[{\"kid\":\"o\"}]}",
      "{\"keys\":[]}", "{}", "[]", "{\"keys\":{}}",
      "{\"keys\":[{\"kty\":\"RSA\",\"kid\":\"o\",\"n\":\"AQAB\",\"e\":\"AQAB\"}, {\"kty\":\"EC\",\"crv\":\"P-256\",\"x\":\"AAAA\",\"y\":\"AAAA\",\"kid\":\"e\"}]}",
    };
    for (String s : sets) { try { var js = JWKSet.parse(s); System.out.println("parse OK keys="+js.getKeys().size()+" :: "+s); } catch (Exception e) { System.out.println("parse FAIL "+e.getMessage()+" :: "+s); } }
    // use=enc / alg mismatch keys
    for (String variant : new String[]{"use=enc", "alg=RS384", "alg=RS256", "key_ops=verify", "key_ops=sign"}) {
      RSAKey.Builder b = new RSAKey.Builder(key.toRSAPublicKey()).keyID("k1");
      String[] kv = variant.split("=");
      if (kv[0].equals("use")) b.keyUse(KeyUse.ENCRYPTION);
      if (kv[0].equals("alg")) b.algorithm(JWSAlgorithm.parse(kv[1]));
      if (kv[0].equals("key_ops")) b.keyOperations(Set.of(kv[1].equals("verify") ? KeyOperation.VERIFY : KeyOperation.SIGN));
      var d2 = (JwtDecoder) m.invoke(null, SocialProvider.GOOGLE, "https://iss", "A", new ImmutableJWKSet<SecurityContext>(new JWKSet(b.build())));
      try { d2.decode(tok(h(), base())); System.out.println("OK    jwk "+variant); } catch (Exception e) { System.out.println("FAIL  jwk "+variant+" :: "+e.getMessage()); }
    }
  }
}
