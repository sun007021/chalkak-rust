import java.time.Duration;
import java.util.*;
import software.amazon.awssdk.auth.credentials.DefaultCredentialsProvider;
import software.amazon.awssdk.http.*;
import software.amazon.awssdk.regions.Region;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.model.PutObjectRequest;
import software.amazon.awssdk.services.s3.presigner.S3Presigner;
import software.amazon.awssdk.services.s3.presigner.model.PutObjectPresignRequest;

/**
 * 원본 백엔드(S3PostImageUploadIssuer, S3SignatureImageUploadIssuer, S3ClientConfig, S3*ImageStorage.headObject)와
 * 같은 방식으로 SDK v2(2.54.1)를 호출해 presigned PUT URL과 HeadObject 서명 헤더를 출력한다.
 * 네트워크 호출은 없다(HeadObject는 요청을 기록만 하는 가짜 SdkHttpClient). SDK는 서명 시각을 Instant.now()로
 * 고정해서 쓰므로, 출력의 X-Amz-Date / x-amz-date 가 그 줄의 서명 시각이다. 기대값은 그 시각을 테스트에 박아 쓴다.
 *
 * 실행: tests/fixtures/run.sh (자격증명과 AWS_ENDPOINT_URL_S3 는 환경변수로 준다)
 */
public class PresignFixture {
    static final Duration FIVE_MIN = Duration.ofMinutes(5);

    public static void main(String[] args) throws Exception {
        String region = "ap-northeast-2";
        String[][] keys = {
            {"chalkak-bucket", "images/staging/dev/signatures/0190f3a2-7b1c-7d2e-8f4a-123456789abc.png"},
            {"chalkak-bucket", "with space/한글+plus/a=b&c~d.png"},
            {"my.dotted.bucket", "images/a.png"},
        };
        try (S3Presigner presigner = S3Presigner.builder()
                .region(Region.of(region))
                .credentialsProvider(DefaultCredentialsProvider.builder().build())
                .build()) {
            for (String[] bk : keys) {
                // S3SignatureImageUploadIssuer.issue / S3PostImageUploadIssuer.issue
                System.out.println("PUT_SIMPLE\t" + bk[0] + "\t" + bk[1] + "\t" + presign(presigner, PutObjectRequest.builder()
                        .bucket(bk[0]).key(bk[1]).contentType("image/png").build()));
                // presignProcessingUpload
                System.out.println("PUT_PROCESSING\t" + bk[0] + "\t" + bk[1] + "\t" + presign(presigner, PutObjectRequest.builder()
                        .bucket(bk[0]).key(bk[1]).contentType("image/webp")
                        .cacheControl("public, max-age=31536000, immutable").ifNoneMatch("*").build()));
            }
        }
        for (String[] bk : keys) {
            System.out.println("HEAD\t" + bk[0] + "\t" + bk[1] + "\t" + head(region, bk[0], bk[1]));
        }
    }

    static String presign(S3Presigner p, PutObjectRequest r) {
        return p.presignPutObject(PutObjectPresignRequest.builder().signatureDuration(FIVE_MIN).putObjectRequest(r).build())
                .url().toString();
    }

    static String head(String region, String bucket, String key) {
        List<SdkHttpRequest> captured = new ArrayList<>();
        SdkHttpClient fake = new SdkHttpClient() {
            public ExecutableHttpRequest prepareRequest(HttpExecuteRequest request) {
                captured.add(request.httpRequest());
                return new ExecutableHttpRequest() {
                    public HttpExecuteResponse call() {
                        return HttpExecuteResponse.builder()
                                .response(SdkHttpResponse.builder().statusCode(200).build()).build();
                    }
                    public void abort() {}
                };
            }
            public void close() {}
        };
        try (S3Client c = S3Client.builder().region(Region.of(region))
                .credentialsProvider(DefaultCredentialsProvider.builder().build())
                .httpClient(fake).build()) {
            c.headObject(r -> r.bucket(bucket).key(key));
        }
        SdkHttpRequest req = captured.get(0);
        StringBuilder sb = new StringBuilder(req.method() + " " + req.getUri());
        new TreeMap<>(req.headers()).forEach((k, v) -> sb.append(" | ").append(k).append(": ").append(String.join(",", v)));
        return sb.toString();
    }
}
