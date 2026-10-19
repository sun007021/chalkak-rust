import com.sun.net.httpserver.HttpServer;
import java.net.InetSocketAddress;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.time.Duration;
import java.util.*;
import software.amazon.awssdk.auth.credentials.AwsBasicCredentials;
import software.amazon.awssdk.auth.credentials.StaticCredentialsProvider;
import software.amazon.awssdk.regions.Region;
import software.amazon.awssdk.retries.StandardRetryStrategy;
import software.amazon.awssdk.services.sqs.SqsClient;
import software.amazon.awssdk.services.sqs.model.*;

public class SqsFixture {
    static volatile int status = 200;
    static volatile String body = "";
    static volatile Map<String, String> respHeaders = Map.of();
    static volatile long delayMs = 0;
    static final List<String> captured = new ArrayList<>();

    static String md5(String s) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("MD5").digest(s.getBytes(StandardCharsets.UTF_8)));
    }

    public static void main(String[] args) throws Exception {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", ex -> {
            String req = new String(ex.getRequestBody().readAllBytes(), StandardCharsets.UTF_8);
            StringBuilder sb = new StringBuilder();
            sb.append(ex.getRequestMethod()).append(' ').append(ex.getRequestURI()).append('\n');
            new TreeMap<>(ex.getRequestHeaders()).forEach((k, v) -> sb.append(k.toLowerCase()).append(": ").append(String.join(",", v)).append('\n'));
            sb.append("BODY ").append(req);
            captured.add(sb.toString());
            String out = body.replace("@@MD5@@", "");
            try {
                if (body.contains("@@MD5@@")) {
                    String mb = new com.fasterxml.jackson.databind.ObjectMapper().readTree(req).get("MessageBody").asText();
                    out = body.replace("@@MD5@@", md5(mb));
                }
            } catch (Throwable t) { out = body; }
            if (delayMs > 0) { try { Thread.sleep(delayMs); } catch (InterruptedException e) {} }
            byte[] bytes = out.getBytes(StandardCharsets.UTF_8);
            ex.getResponseHeaders().set("Content-Type", "application/x-amz-json-1.0");
            respHeaders.forEach((k, v) -> ex.getResponseHeaders().set(k, v));
            ex.sendResponseHeaders(status, bytes.length == 0 ? -1 : bytes.length);
            if (bytes.length > 0) try (var o = ex.getResponseBody()) { o.write(bytes); }
            ex.close();
        });
        server.start();
        URI endpoint = URI.create("http://127.0.0.1:" + server.getAddress().getPort());
        SqsClient client = SqsClient.builder().endpointOverride(endpoint).region(Region.AP_NORTHEAST_2)
                .credentialsProvider(StaticCredentialsProvider.create(AwsBasicCredentials.create("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")))
                .overrideConfiguration(c -> c.apiCallTimeout(Duration.ofSeconds(5)).apiCallAttemptTimeout(Duration.ofSeconds(5))
                        .retryStrategy(StandardRetryStrategy.builder().maxAttempts(1).build()))
                .build();
        String queue = "https://sqs.ap-northeast-2.amazonaws.com/000000000000/chalkak-dev-push";
        String msg = "{\"eventId\":\"0190f3a2-7b1c-7d2e-8f4a-123456789abc\",\"text\":\"한글 \\\" \\u0001\"}";

        // 정상 요청 캡처
        status = 200; body = "{\"MessageId\":\"m-1\",\"MD5OfMessageBody\":\"@@MD5@@\"}"; respHeaders = Map.of();
        client.sendMessage(SendMessageRequest.builder().queueUrl(queue).messageBody(msg).build());
        System.out.println("REQUEST_SEND\t" + captured.get(0).replace("\n", "\\n"));
        captured.clear();
        client.receiveMessage(ReceiveMessageRequest.builder().queueUrl(queue).maxNumberOfMessages(1).waitTimeSeconds(20).visibilityTimeout(60)
                .messageSystemAttributeNames(MessageSystemAttributeName.ALL).build());
        System.out.println("REQUEST_RECEIVE\t" + captured.get(0).replace("\n", "\\n"));
        captured.clear();
        client.deleteMessage(DeleteMessageRequest.builder().queueUrl(queue).receiptHandle("rh-1").build());
        System.out.println("REQUEST_DELETE\t" + captured.get(0).replace("\n", "\\n"));
        captured.clear();
        client.changeMessageVisibility(ChangeMessageVisibilityRequest.builder().queueUrl(queue).receiptHandle("rh-1").visibilityTimeout(60).build());
        System.out.println("REQUEST_VISIBILITY\t" + captured.get(0).replace("\n", "\\n"));
        captured.clear();

        String qe = "x-amzn-query-error";
        String[][] cases = {
            // name, status, body, header k, header v
            {"ok", "200", "{\"MessageId\":\"m-1\",\"MD5OfMessageBody\":\"@@MD5@@\"}", "", ""},
            {"ok_no_md5", "200", "{\"MessageId\":\"m-1\"}", "", ""},
            {"ok_bad_md5", "200", "{\"MessageId\":\"m-1\",\"MD5OfMessageBody\":\"00000000000000000000000000000000\"}", "", ""},
            {"ok_no_id", "200", "{\"MD5OfMessageBody\":\"@@MD5@@\"}", "", ""},
            {"ok_blank_id", "200", "{\"MessageId\":\" \",\"MD5OfMessageBody\":\"@@MD5@@\"}", "", ""},
            {"ok_empty_id", "200", "{\"MessageId\":\"\",\"MD5OfMessageBody\":\"@@MD5@@\"}", "", ""},
            {"ok_empty_body", "200", "", "", ""},
            {"ok_empty_object", "200", "{}", "", ""},
            {"ok_invalid_json", "200", "not json", "", ""},
            {"ok_extra_fields", "200", "{\"MessageId\":\"m-2\",\"MD5OfMessageBody\":\"@@MD5@@\",\"SequenceNumber\":\"1\",\"unknown\":{\"a\":[1,2]}}", "", ""},
            {"status_202", "202", "{\"MessageId\":\"m-1\",\"MD5OfMessageBody\":\"@@MD5@@\"}", "", ""},
            {"s500_type", "500", "{\"__type\":\"com.amazonaws.sqs#InternalError\",\"message\":\"x\"}", "", ""},
            {"s500_plain_type", "500", "{\"__type\":\"InternalError\",\"message\":\"x\"}", "", ""},
            {"s503_query_header", "503", "{\"__type\":\"com.amazonaws.sqs#ServiceUnavailable\",\"message\":\"x\"}", qe, "AWS.SimpleQueueService.ServiceUnavailable;Receiver"},
            {"s400_nonexistent", "400", "{\"__type\":\"com.amazonaws.sqs#QueueDoesNotExist\",\"message\":\"x\"}", qe, "AWS.SimpleQueueService.NonExistentQueue;Sender"},
            {"s400_nonexistent_nohdr", "400", "{\"__type\":\"com.amazonaws.sqs#QueueDoesNotExist\",\"message\":\"x\"}", "", ""},
            {"s400_throttled", "400", "{\"__type\":\"com.amazonaws.sqs#RequestThrottled\",\"message\":\"x\"}", "", ""},
            {"s400_throttled_hdr", "400", "{\"__type\":\"com.amazonaws.sqs#RequestThrottled\",\"message\":\"x\"}", qe, "RequestThrottled;Sender"},
            {"s400_throttling", "400", "{\"__type\":\"Throttling\",\"message\":\"x\"}", "", ""},
            {"s400_throttlingexception", "400", "{\"__type\":\"ThrottlingException\",\"message\":\"x\"}", "", ""},
            {"s400_toomany", "400", "{\"__type\":\"TooManyRequestsException\",\"message\":\"x\"}", "", ""},
            {"s400_reqlimit", "400", "{\"__type\":\"RequestLimitExceeded\",\"message\":\"x\"}", "", ""},
            {"s400_slowdown", "400", "{\"__type\":\"SlowDown\",\"message\":\"x\"}", "", ""},
            {"s400_provisioned", "400", "{\"__type\":\"ProvisionedThroughputExceededException\",\"message\":\"x\"}", "", ""},
            {"s400_limitexceeded", "400", "{\"__type\":\"LimitExceededException\",\"message\":\"x\"}", "", ""},
            {"s400_bandwidth", "400", "{\"__type\":\"BandwidthLimitExceeded\",\"message\":\"x\"}", "", ""},
            {"s400_prior", "400", "{\"__type\":\"PriorRequestNotComplete\",\"message\":\"x\"}", "", ""},
            {"s400_ec2throttled", "400", "{\"__type\":\"EC2ThrottledException\",\"message\":\"x\"}", "", ""},
            {"s400_transactioninprogress", "400", "{\"__type\":\"TransactionInProgressException\",\"message\":\"x\"}", "", ""},
            {"s400_throttledexc", "400", "{\"__type\":\"RequestThrottledException\",\"message\":\"x\"}", "", ""},
            {"s429_plain", "429", "{\"__type\":\"TooManyRequests\",\"message\":\"x\"}", "", ""},
            {"s429_nothrottle_code", "429", "{\"__type\":\"SomethingElse\",\"message\":\"x\"}", "", ""},
            {"s403_denied", "403", "{\"__type\":\"AccessDenied\",\"message\":\"x\"}", "", ""},
            {"s403_denied_colon", "403", "{\"__type\":\"AccessDenied:http://internal.amazon.com/coral/com.amazon.coral.service/\",\"message\":\"x\"}", "", ""},
            {"s403_denied_ns_colon", "403", "{\"__type\":\"com.amazon.coral.service#AccessDeniedException:http://x/\",\"message\":\"x\"}", "", ""},
            {"s400_space_type", "400", "{\"__type\":\"SECRET PRIVATE BODY\",\"message\":\"x\"}", "", ""},
            {"s500_space_type", "500", "{\"__type\":\"SECRET PRIVATE BODY\",\"message\":\"x\"}", "", ""},
            {"s400_long_type", "400", "{\"__type\":\"" + "A".repeat(101) + "\",\"message\":\"x\"}", "", ""},
            {"s400_100_type", "400", "{\"__type\":\"" + "A".repeat(100) + "\",\"message\":\"x\"}", "", ""},
            {"s400_msg_only", "400", "{\"message\":\"x\"}", "", ""},
            {"s400_Message_cap", "400", "{\"__type\":\"InvalidParameterValue\",\"Message\":\"x\"}", "", ""},
            {"s400_empty", "400", "", "", ""},
            {"s400_html", "400", "<html>bad</html>", "", ""},
            {"s500_empty", "500", "", "", ""},
            {"s500_html", "500", "<html>bad</html>", "", ""},
            {"s502_html", "502", "<html>bad</html>", "", ""},
            {"s503_empty", "503", "", "", ""},
            {"s301", "301", "", "", ""},
            {"s302_body", "302", "{\"__type\":\"X\"}", "", ""},
            {"s304", "304", "", "", ""},
            {"s404_json", "404", "{\"__type\":\"NotFound\",\"message\":\"x\"}", "", ""},
            {"s404_html", "404", "<html>no</html>", "", ""},
            {"s400_hdr_only", "400", "{\"message\":\"x\"}", qe, "AWS.SimpleQueueService.NonExistentQueue;Sender"},
            {"s400_hdr_badcode", "400", "{\"message\":\"x\"}", qe, "BAD CODE;Sender"},
            {"s400_hdr_nofault", "400", "{\"message\":\"x\"}", qe, "OnlyCode"},
            {"s400_hdr_and_type_differ", "400", "{\"__type\":\"TypeCode\",\"message\":\"x\"}", qe, "HeaderCode;Sender"},
            {"s400_type_nonascii", "400", "{\"__type\":\"오류\",\"message\":\"x\"}", "", ""},
            {"s400_type_null", "400", "{\"__type\":null,\"message\":\"x\"}", "", ""},
            {"s400_type_empty", "400", "{\"__type\":\"\",\"message\":\"x\"}", "", ""},
            {"s400_type_number", "400", "{\"__type\":5,\"message\":\"x\"}", "", ""},
            {"s400_hdr_nofault_type", "400", "{\"__type\":\"TypeCode\",\"message\":\"x\"}", qe, "OnlyCode"},
            {"s400_hdr_empty_type", "400", "{\"__type\":\"TypeCode\",\"message\":\"x\"}", qe, ""},
            {"s400_hdr_three", "400", "{\"__type\":\"TypeCode\",\"message\":\"x\"}", qe, "A;B;C"},
            {"s400_hdr_semicolon_only", "400", "{\"__type\":\"TypeCode\",\"message\":\"x\"}", qe, ";Sender"},
            {"s400_type_two_hash", "400", "{\"__type\":\"a#b#c\",\"message\":\"x\"}", "", ""},
            {"s400_type_colon_hash", "400", "{\"__type\":\"a:b#c\",\"message\":\"x\"}", "", ""},
            {"s400_type_hash_colon", "400", "{\"__type\":\"A#B:C\",\"message\":\"x\"}", "", ""},
            {"s400_throttledexception2", "400", "{\"__type\":\"ThrottledException\",\"message\":\"x\"}", "", ""},
            {"s400_type_bool", "400", "{\"__type\":true,\"message\":\"x\"}", "", ""},
            {"s400_type_object", "400", "{\"__type\":{\"a\":1},\"message\":\"x\"}", "", ""},
            {"s400_code_field", "400", "{\"code\":\"FromCode\",\"message\":\"x\"}", "", ""},
            {"s400_Code_field", "400", "{\"Code\":\"FromCodeCap\",\"message\":\"x\"}", "", ""},
            {"s400_type_with_hdr_lower", "400", "{\"message\":\"x\"}", "X-Amzn-Query-Error", "LowerCase.Code;Sender"},
        };
        for (String[] c : cases) {
            status = Integer.parseInt(c[1]); body = c[2];
            respHeaders = c[3].isEmpty() ? Map.of() : Map.of(c[3], c[4]);
            delayMs = 0;
            captured.clear();
            System.out.println("CASE\t" + c[0] + "\t" + classify(client, queue, msg));
        }
        // 수신·삭제·가시성 응답
        String bodyHello = "hello 한글";
        String h = md5(bodyHello);
        String[][] rcases = {
            {"r_ok", "200", "{\"Messages\":[{\"MessageId\":\"a\",\"ReceiptHandle\":\"rh\",\"Body\":\"hello 한글\",\"MD5OfBody\":\"" + h + "\",\"Attributes\":{\"ApproximateReceiveCount\":\"2\"}}]}"},
            {"r_no_md5", "200", "{\"Messages\":[{\"MessageId\":\"a\",\"ReceiptHandle\":\"rh\",\"Body\":\"hello 한글\"}]}"},
            {"r_bad_md5", "200", "{\"Messages\":[{\"MessageId\":\"a\",\"ReceiptHandle\":\"rh\",\"Body\":\"hello 한글\",\"MD5OfBody\":\"00000000000000000000000000000000\"}]}"},
            {"r_empty_obj", "200", "{}"},
            {"r_empty_list", "200", "{\"Messages\":[]}"},
            {"r_empty_body", "200", ""},
            {"r_no_body_field", "200", "{\"Messages\":[{\"MessageId\":\"a\",\"ReceiptHandle\":\"rh\"}]}"},
            {"r_err", "400", "{\"__type\":\"com.amazonaws.sqs#QueueDoesNotExist\",\"message\":\"x\"}"},
        };
        for (String[] c : rcases) {
            status = Integer.parseInt(c[1]); body = c[2]; respHeaders = Map.of(); delayMs = 0;
            System.out.println("RCASE\t" + c[0] + "\t" + classifyReceive(client, queue));
        }
        String[][] dcases = {{"d_empty_200", "200", ""}, {"d_json_200", "200", "{}"}, {"d_err", "400", "{\"__type\":\"ReceiptHandleIsInvalid\"}"}, {"d_500", "500", ""}};
        for (String[] c : dcases) {
            status = Integer.parseInt(c[1]); body = c[2]; respHeaders = Map.of(); delayMs = 0;
            System.out.println("DCASE\t" + c[0] + "\t" + classifyDelete(client, queue) + "\t" + classifyVisibility(client, queue));
        }
        // 지연 6초: apiCallTimeout
        status = 200; body = "{\"MessageId\":\"m-1\",\"MD5OfMessageBody\":\"@@MD5@@\"}"; respHeaders = Map.of(); delayMs = 6500;
        long t0 = System.currentTimeMillis();
        String r = classify(client, queue, msg);
        System.out.println("CASE\ttimeout_6s\t" + r + "\telapsedMs=" + (System.currentTimeMillis() - t0) / 500 * 500);
        delayMs = 0;
        server.stop(0);
        // 연결 거부
        SqsClient dead = SqsClient.builder().endpointOverride(URI.create("http://127.0.0.1:1")).region(Region.AP_NORTHEAST_2)
                .credentialsProvider(StaticCredentialsProvider.create(AwsBasicCredentials.create("a", "b")))
                .overrideConfiguration(c -> c.apiCallTimeout(Duration.ofSeconds(5)).apiCallAttemptTimeout(Duration.ofSeconds(5))
                        .retryStrategy(StandardRetryStrategy.builder().maxAttempts(1).build())).build();
        System.out.println("CASE\tconn_refused\t" + classify(dead, queue, msg));
        client.close(); dead.close();
        System.exit(0);
    }

    static String classifyReceive(SqsClient client, String queue) {
        try {
            var r = client.receiveMessage(ReceiveMessageRequest.builder().queueUrl(queue).maxNumberOfMessages(1).waitTimeSeconds(20).visibilityTimeout(60).build());
            StringBuilder sb = new StringBuilder("OK\tcount=" + r.messages().size());
            for (var m : r.messages()) sb.append("\tid=").append(m.messageId()).append("\thandle=").append(m.receiptHandle()).append("\tbody=").append(m.body()).append("\tattrs=").append(m.attributesAsStrings());
            return sb.toString();
        } catch (SqsException e) {
            String code = e.awsErrorDetails() == null ? "<nodetails>" : String.valueOf(e.awsErrorDetails().errorCode());
            return "SQS\tstatus=" + e.statusCode() + "\tcode=" + code;
        } catch (RuntimeException e) {
            return "CLIENT\t" + e.getClass().getSimpleName();
        }
    }

    static String classifyDelete(SqsClient client, String queue) {
        try { client.deleteMessage(DeleteMessageRequest.builder().queueUrl(queue).receiptHandle("rh").build()); return "OK"; }
        catch (SqsException e) { return "SQS:" + e.statusCode() + ":" + (e.awsErrorDetails() == null ? "<nodetails>" : e.awsErrorDetails().errorCode()); }
        catch (RuntimeException e) { return "CLIENT:" + e.getClass().getSimpleName(); }
    }

    static String classifyVisibility(SqsClient client, String queue) {
        try { client.changeMessageVisibility(ChangeMessageVisibilityRequest.builder().queueUrl(queue).receiptHandle("rh").visibilityTimeout(60).build()); return "OK"; }
        catch (SqsException e) { return "SQS:" + e.statusCode() + ":" + (e.awsErrorDetails() == null ? "<nodetails>" : e.awsErrorDetails().errorCode()); }
        catch (RuntimeException e) { return "CLIENT:" + e.getClass().getSimpleName(); }
    }

    static String classify(SqsClient client, String queue, String msg) {
        try {
            String id = client.sendMessage(SendMessageRequest.builder().queueUrl(queue).messageBody(msg).build()).messageId();
            return "OK\t" + (id == null ? "<null>" : "[" + id + "]");
        } catch (SqsException e) {
            String code = e.awsErrorDetails() == null ? "<nodetails>" : String.valueOf(e.awsErrorDetails().errorCode());
            return "SQS\tstatus=" + e.statusCode() + "\tcode=" + code + "\tthrottling=" + e.isThrottlingException();
        } catch (software.amazon.awssdk.core.exception.SdkClientException e) {
            return "CLIENT\t" + e.getClass().getSimpleName() + "\t" + String.valueOf(e.getMessage()).replace("\n", " ");
        } catch (RuntimeException e) {
            return "OTHER\t" + e.getClass().getName() + "\t" + String.valueOf(e.getMessage()).replace("\n", " ");
        }
    }
}
