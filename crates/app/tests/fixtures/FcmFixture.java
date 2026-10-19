import com.chalkak.backend.notification.domain.NotificationSourceType;
import com.chalkak.backend.notification.domain.NotificationType;
import com.chalkak.backend.notification.infrastructure.infra.FcmDevicePushSender;
import com.chalkak.backend.notification.service.DevicePushRequest;
import com.chalkak.backend.notification.service.DevicePushResult;
import com.chalkak.backend.notification.service.PushMessage;
import com.google.api.client.http.javanet.NetHttpTransport;
import com.google.auth.oauth2.ServiceAccountCredentials;
import com.google.firebase.FirebaseApp;
import com.google.firebase.FirebaseOptions;
import com.google.firebase.messaging.FirebaseMessaging;
import com.sun.net.httpserver.HttpServer;
import java.io.ByteArrayInputStream;
import java.net.HttpURLConnection;
import java.net.InetSocketAddress;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.time.ZoneId;
import java.time.ZoneOffset;
import java.util.*;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * 원본 `FcmDevicePushSender`와 Firebase Admin SDK 9.11.0이 FCM HTTP v1 요청을 어떻게 만들고 응답을 어떻게 분류하는지 기록한다.
 * Rust 구현(`notification/fcm`)은 이 출력(expected_fcm.tsv)과 같은 요청 바이트·같은 분류를 내야 한다.
 * 사용: run_fcm.sh [backend-jar] [jdk-bin-dir] [private-key.pem] > expected_fcm.tsv
 */
public class FcmFixture {
    static final Instant NOW = Instant.parse("2026-10-07T00:00:00Z");
    static final List<String> fcmRequests = new CopyOnWriteArrayList<>();
    static final List<String> tokenRequests = new CopyOnWriteArrayList<>();
    static final AtomicInteger fcmCalls = new AtomicInteger();
    static final AtomicInteger tokenCalls = new AtomicInteger();
    static volatile int fcmStatus = 200;
    static volatile String fcmBody = "{\"name\":\"projects/test/messages/accepted\"}";
    static volatile Map<String, List<String>> fcmHeaders = Map.of();
    static volatile int acceptAfter = Integer.MAX_VALUE;
    static volatile long fcmDelayMs = 0;
    static volatile int tokenStatus = 200;
    static volatile String tokenBody = "{\"access_token\":\"fixture-access-token\",\"expires_in\":3600,\"token_type\":\"Bearer\"}";
    static volatile String currentInput = "";
    static volatile int portHolder;
    static HttpServer server;

    static String describe(com.sun.net.httpserver.HttpExchange ex, byte[] body) {
        StringBuilder sb = new StringBuilder();
        sb.append(ex.getRequestMethod()).append(' ').append(ex.getRequestURI());
        new TreeMap<>(ex.getRequestHeaders()).forEach((k, v) -> {
            String name = k.toLowerCase();
            if (name.equals("host") || name.equals("user-agent") || name.equals("connection")) return;
            sb.append("\\n").append(name).append(": ").append(String.join(",", v));
        });
        sb.append("\\nBODY ").append(new String(body, StandardCharsets.UTF_8).replace("\n", "\\n"));
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        String pem = Files.readString(Path.of(args[0]));
        server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.setExecutor(java.util.concurrent.Executors.newCachedThreadPool());
        server.createContext("/token", ex -> {
            byte[] req = ex.getRequestBody().readAllBytes();
            tokenCalls.incrementAndGet();
            tokenRequests.add(describe(ex, req));
            byte[] out = tokenBody.getBytes(StandardCharsets.UTF_8);
            ex.getResponseHeaders().set("Content-Type", "application/json");
            ex.sendResponseHeaders(tokenStatus, out.length);
            ex.getResponseBody().write(out);
            ex.close();
        });
        server.createContext("/", ex -> {
            byte[] req = ex.getRequestBody().readAllBytes();
            int attempt = fcmCalls.incrementAndGet();
            fcmRequests.add(describe(ex, req));
            if (fcmDelayMs > 0) {
                try { Thread.sleep(fcmDelayMs); } catch (InterruptedException e) { }
            }
            int status = attempt >= acceptAfter ? 200 : fcmStatus;
            String body = attempt >= acceptAfter ? "{\"name\":\"projects/test/messages/accepted\"}" : fcmBody;
            byte[] out = body.getBytes(StandardCharsets.UTF_8);
            fcmHeaders.forEach((k, v) -> v.forEach(value -> ex.getResponseHeaders().add(k, value)));
            if (!ex.getResponseHeaders().containsKey("Content-Type")) ex.getResponseHeaders().set("Content-Type", "application/json; charset=UTF-8");
            ex.sendResponseHeaders(status, out.length == 0 ? -1 : out.length);
            if (out.length > 0) ex.getResponseBody().write(out);
            ex.close();
        });
        server.start();
        int port = server.getAddress().getPort();
        String keyJson = "{\"type\":\"service_account\",\"project_id\":\"test\",\"private_key_id\":\"fixture-key-id\","
                + "\"private_key\":" + jsonString(pem) + ",\"client_email\":\"worker@test.iam.gserviceaccount.com\","
                + "\"client_id\":\"1234567890\",\"token_uri\":\"http://127.0.0.1:" + port + "/token\"}";

        // ---- 1. 요청 바이트 ----
        UUID event = UUID.fromString("0190f3a2-7b1c-7d2e-8f4a-123456789abc");
        UUID notification = UUID.fromString("0190f3a2-7b1c-7d2e-8f4a-123456789abd");
        UUID user = UUID.fromString("0190f3a2-7b1c-7d2e-8f4a-123456789abe");
        UUID source = UUID.fromString("0190f3a2-7b1c-7d2e-8f4a-123456789abf");
        Instant expires = NOW.plusSeconds(1800);
        List<Object[]> requests = List.of(
                new Object[]{"approved", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.POST_APPROVED, NotificationSourceType.POST, source, NOW, expires), "fcm-token-1", "게시물이 승인되었습니다.", "내 사진이 피드에 공개되었습니다.", expires)},
                new Object[]{"rejected", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.POST_REJECTED, NotificationSourceType.POST, source, NOW, expires), "fcm-token-2", "게시물이 반려되었습니다.", "반려 사유를 확인해 주세요.", expires)},
                new Object[]{"topic", new DevicePushRequest(new PushMessage(event, null, user, NotificationType.TOPIC, NotificationSourceType.TOPIC, source, NOW, expires), "fcm-token-3", "오늘의 주제", "빛과 그림자", expires)},
                new Object[]{"notice", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.NOTICE, NotificationSourceType.NOTICE, source, NOW, expires), "fcm-token-4", "서비스 점검 안내", "10월 20일 02:00부터 점검합니다.", expires)},
                new Object[]{"fraction", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.POST_APPROVED, NotificationSourceType.POST, source, NOW, expires), "fcm-token-5", "t", "b", NOW.plusMillis(1800123))},
                new Object[]{"fraction_micro", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.POST_APPROVED, NotificationSourceType.POST, source, NOW, expires), "fcm-token-5", "t", "b", NOW.plusNanos(1_500_999_999L))},
                new Object[]{"subsecond", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.POST_APPROVED, NotificationSourceType.POST, source, NOW, expires), "fcm-token-5", "t", "b", NOW.plusMillis(500))},
                new Object[]{"escapes", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.POST_APPROVED, NotificationSourceType.POST, source, NOW, expires), "tok\"en\\x", "제목 \"따옴표\" \\ / <b>&='", "줄\n바꿈\t탭\u0001\u001f\u007f   😀 é", expires)}
        );
        // 문자 전수 조사: 제어 문자·경계 문자가 JSON 에서 어떻게 이스케이프되는지 고정한다(0x00-0x7F, 경계 부근 문자, 비BMP).
        StringBuilder sweep = new StringBuilder();
        for (int c = 0; c < 0x80; c++) sweep.append((char) c);
        for (int c : new int[]{0x80, 0x9F, 0xA0, 0xAD, 0x2028, 0x2029, 0xD7FF, 0xE000, 0xFFFD, 0xFFFE, 0xFFFF}) sweep.append((char) c);
        sweep.appendCodePoint(0x10000).appendCodePoint(0x10FFFF);
        requests = new ArrayList<>(requests);
        requests.add(new Object[]{"sweep", new DevicePushRequest(new PushMessage(event, notification, user, NotificationType.POST_APPROVED, NotificationSourceType.POST, source, NOW, expires), "tok", "s" + sweep, "b", expires)});
        FirebaseApp app = newApp(keyJson, port, 10000);
        DevicePushResult last = null;
        FcmDevicePushSender sender = new FcmDevicePushSender(FirebaseMessaging.getInstance(app), Clock.fixed(NOW, ZoneOffset.UTC));
        reset();
        for (Object[] item : requests) {
            fcmRequests.clear();
            DevicePushResult result = sender.send((DevicePushRequest) item[1]);
            System.out.println("REQUEST\t" + item[0] + "\t" + result.status() + "\t" + fcmRequests.get(0));
        }
        // 토큰 교환 요청: 첫 발송에서만 일어나고 이후에는 캐시를 쓴다.
        System.out.println("TOKEN_CALLS_AFTER_" + requests.size() + "_SENDS\t" + tokenCalls.get());
        for (String t : tokenRequests) System.out.println("TOKEN_REQUEST\t" + normalizeToken(t));
        app.delete();

        // ---- 2. 응답 분류 ----
        String ok = "{\"name\":\"projects/test/messages/accepted\"}";
        String[][] cases = {
                // name, status, body, header name, header value(s joined by |)
                {"ok", "200", ok, "", ""},
                {"ok_empty_object", "200", "{}", "", ""},
                {"ok_not_json", "200", "not json", "", ""},
                {"ok_empty_body", "200", "", "", ""},
                {"unregistered", "404", err(404, "NOT_FOUND", "UNREGISTERED"), "", ""},
                {"unregistered_status_missing", "404", "{\"error\":{\"code\":404,\"message\":\"gone\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"unregistered_with_400", "400", err(400, "NOT_FOUND", "UNREGISTERED"), "", ""},
                {"not_found_no_details", "404", "{\"error\":{\"code\":404,\"status\":\"NOT_FOUND\",\"message\":\"x\"}}", "", ""},
                {"invalid_argument", "400", err(400, "INVALID_ARGUMENT", "INVALID_ARGUMENT"), "", ""},
                {"invalid_argument_unspecified", "400", err(400, "INVALID_ARGUMENT", "UNSPECIFIED_ERROR"), "", ""},
                {"sender_mismatch", "403", err(403, "PERMISSION_DENIED", "SENDER_ID_MISMATCH"), "", ""},
                {"third_party_auth", "401", err(401, "UNAUTHENTICATED", "THIRD_PARTY_AUTH_ERROR"), "", ""},
                {"apns_auth", "401", err(401, "UNAUTHENTICATED", "APNS_AUTH_ERROR"), "", ""},
                {"quota", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "", ""},
                {"quota_no_header_status_only", "429", "{\"error\":{\"code\":429,\"status\":\"RESOURCE_EXHAUSTED\"}}", "", ""},
                {"internal", "500", err(500, "INTERNAL", "INTERNAL"), "", ""},
                {"internal_empty_body", "500", "", "", ""},
                {"internal_text_body", "500", "oops", "", ""},
                {"bad_gateway", "502", "bad gateway", "", ""},
                {"gateway_timeout", "504", "", "", ""},
                {"teapot", "418", "", "", ""},
                {"conflict", "409", err(409, "ABORTED", "UNSPECIFIED_ERROR"), "", ""},
                {"precondition", "412", err(412, "FAILED_PRECONDITION", "UNSPECIFIED_ERROR"), "", ""},
                {"unauthenticated_plain", "401", "{\"error\":{\"code\":401,\"status\":\"UNAUTHENTICATED\"}}", "", ""},
                {"status_unknown_string", "400", err(400, "FOO_BAR", "INVALID_ARGUMENT"), "", ""},
                {"status_lowercase", "400", err(400, "invalid_argument", "INVALID_ARGUMENT"), "", ""},
                {"error_is_string", "400", "{\"error\":\"nope\"}", "", ""},
                {"error_is_array", "400", "{\"error\":[1,2]}", "", ""},
                {"details_not_list", "400", "{\"error\":{\"status\":\"INVALID_ARGUMENT\",\"details\":{\"errorCode\":\"UNREGISTERED\"}}}", "", ""},
                {"details_second_entry", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"@type\":\"type.googleapis.com/google.rpc.BadRequest\"},{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"details_wrong_type", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"@type\":\"type.googleapis.com/google.rpc.Other\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"error_code_unknown_value", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"NEW_CODE\"}]}}", "", ""},
                {"unavailable_detail_on_400", "400", err(400, "INVALID_ARGUMENT", "UNAVAILABLE"), "", ""},
                {"internal_detail_on_400", "400", err(400, "INVALID_ARGUMENT", "INTERNAL"), "", ""},
                {"quota_detail_on_400", "400", err(400, "INVALID_ARGUMENT", "QUOTA_EXCEEDED"), "", ""},
                {"unavailable_status_on_400", "400", err(400, "UNAVAILABLE", "UNSPECIFIED_ERROR"), "", ""},
                {"deadline_status_on_400", "400", err(400, "DEADLINE_EXCEEDED", "UNSPECIFIED_ERROR"), "", ""},
                {"retry_after_120", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "120"},
                {"retry_after_plus", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "+75"},
                {"retry_after_negative", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "-1"},
                {"retry_after_59", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "59"},
                {"retry_after_60", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "60"},
                {"retry_after_61", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "61"},
                {"retry_after_huge", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "99999999999"},
                {"retry_after_overflow", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "99999999999999999999"},
                {"retry_after_date", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:02:00 GMT"},
                {"retry_after_date_wrong_weekday", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Mon, 07 Oct 2026 00:02:00 GMT"},
                {"retry_after_date_no_weekday", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "07 Oct 2026 00:02:00 GMT"},
                {"retry_after_date_no_seconds", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:03 GMT"},
                {"retry_after_date_offset", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 09:05:00 +0900"},
                {"retry_after_date_one_digit_day", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 7 Oct 2026 00:04:00 GMT"},
                {"retry_after_date_lowercase", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "wed, 07 oct 2026 00:05:00 gmt"},
                {"retry_after_date_past", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Tue, 06 Oct 2026 00:00:00 GMT"},
                {"retry_after_date_ut", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:06:00 UT"},
                {"retry_after_date_z", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:06:00 Z"},
                {"retry_after_date_obs_zone", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:06:00 EST"},
                {"retry_after_date_fraction", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:06:00.5 GMT"},
                {"retry_after_date_two_digit_year", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 26 00:07:00 GMT"},
                {"retry_after_date_long_month", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 October 2026 00:07:00 GMT"},
                {"retry_after_date_sec60", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:07:60 GMT"},
                {"retry_after_date_hour24", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 24:00:00 GMT"},
                {"retry_after_date_feb30", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Mon, 30 Feb 2026 00:08:00 GMT"},
                {"retry_after_invalid", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "invalid-header"},
                {"retry_after_empty", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", ""},
                {"retry_after_padded", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", " 90 "},
                {"retry_after_float", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "90.5"},
                {"retry_after_arabic_digits", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "١٢٣"},
                {"retry_after_two_headers", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "100|200"},
                {"retry_after_on_500", "500", err(500, "INTERNAL", "INTERNAL"), "Retry-After", "300"},
                {"retry_after_on_400", "400", err(400, "INVALID_ARGUMENT", "INVALID_ARGUMENT"), "Retry-After", "300"},
                {"retry_after_date_nov31", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "31 Nov 2026 00:08:00 GMT"},
                {"retry_after_date_nov31_mon", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Mon, 31 Nov 2026 00:08:00 GMT"},
                {"retry_after_date_nov31_tue", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Tue, 31 Nov 2026 00:08:00 GMT"},
                {"retry_after_date_feb29_nonleap", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "29 Feb 2027 00:00:00 GMT"},
                {"retry_after_date_day32", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "32 Nov 2026 00:00:00 GMT"},
                {"retry_after_date_day0", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "00 Nov 2026 00:00:00 GMT"},
                {"retry_after_date_offset_1900", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 19:00:00 +1900"},
                {"retry_after_date_offset_1800", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Thu, 08 Oct 2026 18:05:00 +1800"},
                {"retry_after_date_offset_min60", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:05:00 +0060"},
                {"retry_after_date_offset_zero", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:05:00 +0000"},
                {"retry_after_date_offset_colon", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 09:05:00 +09:00"},
                {"retry_after_date_hour24_sec", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 24:00:01 GMT"},
                {"retry_after_date_hour24_min", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 24:01 GMT"},
                {"retry_after_date_hour24_nosec", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 24:00 GMT"},
                {"retry_after_date_double_space", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026  00:05:00 GMT"},
                {"retry_after_date_double_space_zone", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:05:00  GMT"},
                {"retry_after_date_no_space_after_comma", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed,07 Oct 2026 00:05:00 GMT"},
                {"retry_after_date_space_before_comma", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed , 07 Oct 2026 00:05:00 GMT"},
                {"retry_after_date_long_weekday", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wednesday, 07 Oct 2026 00:05:00 GMT"},
                {"retry_after_date_one_digit_hour", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 0:05:00 GMT"},
                {"retry_after_date_one_digit_min", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:5:00 GMT"},
                {"retry_after_date_one_digit_sec", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:05:0 GMT"},
                {"retry_after_date_year_5", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 12026 00:05:00 GMT"},
                {"retry_after_date_trailing_text", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:05:00 GMT x"},
                {"retry_after_date_weekday_only_comma", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:05:00GMT"},
                {"retry_after_date_min60", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Wed, 07 Oct 2026 00:60:00 GMT"},
                {"retry_after_date_future_year", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Fri, 07 Oct 2033 00:00:00 GMT"},
                {"retry_after_date_dec", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "Thu, 31 Dec 2026 00:00:00 GMT"},
                {"retry_after_digits_with_underscore", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "1_000"},
                {"retry_after_hex", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "0x50"},
                {"retry_after_zero_prefixed", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "0090"},
                {"retry_after_minus_zero", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "-0"},
                {"retry_after_only_plus", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "+"},
                // HTTP 상태만 있고 본문이 없을 때의 분류
                {"status_only_400", "400", "", "", ""},
                {"status_only_403", "403", "", "", ""},
                {"status_only_404", "404", "", "", ""},
                {"status_only_408", "408", "", "", ""},
                {"status_only_409", "409", "", "", ""},
                {"status_only_410", "410", "", "", ""},
                {"status_only_412", "412", "", "", ""},
                {"status_only_429", "429", "", "", ""},
                {"status_only_500", "500", "", "", ""},
                {"status_only_501", "501", "", "", ""},
                {"status_only_507", "507", "", "", ""},
                {"status_only_300", "300", "", "", ""},
                {"status_only_204", "204", "", "", ""},
                {"error_status_other_than_http", "500", err(500, "NOT_FOUND", "UNAVAILABLE"), "", ""},
                {"error_status_ok_string", "400", err(400, "OK", "INVALID_ARGUMENT"), "", ""},
                {"error_status_null", "404", "{\"error\":{\"status\":null,\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"error_status_empty", "404", "{\"error\":{\"status\":\"\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"no_error_object", "404", "{}", "", ""},
                {"error_code_lowercase", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"unregistered\"}]}}", "", ""},
                {"error_code_not_string", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":5}]}}", "", ""},
                {"detail_apns_auth_error", "404", err(404, "NOT_FOUND", "APNS_AUTH_ERROR"), "", ""},
                {"detail_quota_on_404", "404", err(404, "NOT_FOUND", "QUOTA_EXCEEDED"), "", ""},
                {"detail_unregistered_on_500", "500", err(500, "INTERNAL", "UNREGISTERED"), "", ""},
                {"detail_unregistered_on_503_status_field", "400", err(400, "UNAVAILABLE", "UNREGISTERED"), "", ""},
                {"detail_first_wins", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"SENDER_ID_MISMATCH\"},{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"detail_unknown_then_known", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"NEW_CODE\"},{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"detail_without_type", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[{\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"detail_is_null_entry", "404", "{\"error\":{\"status\":\"NOT_FOUND\",\"details\":[null,{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}", "", ""},
                {"body_is_json_array", "404", "[1]", "", ""},
                {"body_is_json_string", "404", "\"x\"", "", ""},
                {"body_trailing_garbage", "404", err(404, "NOT_FOUND", "UNREGISTERED") + " trailing", "", ""},
                {"ok_array_body", "200", "[1]", "", ""},
                {"ok_string_body", "200", "\"x\"", "", ""},
                {"ok_204", "204", "", "", ""},
        };
        for (String[] c : cases) {
            reset();
            fcmStatus = Integer.parseInt(c[1]);
            fcmBody = c[2];
            if (!c[3].isEmpty()) fcmHeaders = Map.of(c[3], Arrays.asList(c[4].split("\\|", -1)));
            currentInput = input(c[1], c[3], c[4], "", c[2]);
            runCase("CASE\t" + c[0], keyJson, port, 10000, null);
        }

        // ---- 3. 503 재시도: SDK는 503만 재시도한다(최대 4회). Retry-After가 60초 이하면 그만큼 기다리고 넘으면 재시도하지 않는다. ----
        String unavailable = err(503, "UNAVAILABLE", "UNAVAILABLE");
        String[][] retryCases = {
                {"503_then_ok", "503", unavailable, "", "", "2"},
                {"503_retry_after_1_then_ok", "503", unavailable, "Retry-After", "1", "2"},
                {"503_retry_after_61", "503", unavailable, "Retry-After", "61", "99"},
                {"503_retry_after_0", "503", unavailable, "Retry-After", "0", "2"},
                {"503_retry_after_invalid", "503", unavailable, "Retry-After", "later", "2"},
                {"503_retry_after_date_past", "503", unavailable, "Retry-After", "Tue, 06 Oct 2026 00:00:00 GMT", "2"},
                {"503_always_retry_after_1", "503", unavailable, "Retry-After", "1", "99"},
                {"503_always", "503", unavailable, "", "", "99"},
                {"500_no_retry", "500", err(500, "INTERNAL", "INTERNAL"), "", "", "2"},
                {"429_no_retry", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "", "", "2"},
                // SDK 는 시스템 시계를 쓰므로 날짜 헤더는 지금 기준 상대 값으로 만든다.
                {"503_retry_after_date_in_2s", "503", unavailable, "Retry-After", "@DATE+3", "2"},
                {"503_retry_after_date_in_100s", "503", unavailable, "Retry-After", "@DATE+100", "99"},
                {"503_retry_after_plus_1", "503", unavailable, "Retry-After", "+1", "2"},
                {"503_retry_after_padded", "503", unavailable, "Retry-After", " 1 ", "2"},
                {"503_retry_after_float", "503", unavailable, "Retry-After", "1.5", "2"},
                {"503_retry_after_negative", "503", unavailable, "Retry-After", "-5", "2"},
                {"503_retry_after_two_headers", "503", unavailable, "Retry-After", "1|200", "2"},
                {"503_plain_body", "503", "", "", "", "2"},
                {"503_detail_unregistered", "503", err(503, "UNAVAILABLE", "UNREGISTERED"), "", "", "2"},
                {"503_retry_after_60_boundary_only_first", "503", unavailable, "Retry-After", "61", "2"},
                {"500_retry_after_1", "500", err(500, "INTERNAL", "INTERNAL"), "Retry-After", "1", "2"},
                {"429_retry_after_1", "429", err(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED"), "Retry-After", "1", "2"},
        };
        for (String[] c : retryCases) {
            reset();
            fcmStatus = Integer.parseInt(c[1]);
            fcmBody = c[2];
            acceptAfter = Integer.parseInt(c[5]);
            if (!c[3].isEmpty()) {
                String value = c[4];
                if (value.startsWith("@DATE+")) value = httpDate(Instant.now().plusSeconds(Long.parseLong(value.substring(6))));
                fcmHeaders = Map.of(c[3], Arrays.asList(value.split("\\|", -1)));
            }
            currentInput = input(c[1], c[3], c[4], c[5], c[2]);
            runCase("RETRY\t" + c[0], keyJson, port, 10000, null);
        }

        // ---- 4. 401: HttpCredentialsAdapter가 토큰을 새로 받아 한 번 더 보내는지 ----
        for (String[] c : new String[][]{{"401_always", "99", "", ""}, {"401_then_ok", "2", "", ""},
                {"401_bearer_realm_then_ok", "2", "WWW-Authenticate", "Bearer realm=\"x\""},
                {"401_bearer_invalid_token_then_ok", "2", "WWW-Authenticate", "Bearer error=\"invalid_token\""},
                {"401_bearer_invalid_token_spaced_then_ok", "2", "WWW-Authenticate", "Bearer  error = invalid_token"},
                {"401_bearer_insufficient_then_ok", "2", "WWW-Authenticate", "Bearer error=\"insufficient_scope\""},
                {"401_basic_then_ok", "2", "WWW-Authenticate", "Basic realm=\"x\""},
                {"401_lowercase_bearer_invalid_token_then_ok", "2", "WWW-Authenticate", "bearer error=\"invalid_token\""}}) {
            reset();
            fcmStatus = 401;
            fcmBody = err(401, "UNAUTHENTICATED", "THIRD_PARTY_AUTH_ERROR");
            acceptAfter = Integer.parseInt(c[1]);
            if (!c[2].isEmpty()) fcmHeaders = Map.of(c[2], List.of(c[3]));
            currentInput = input("401", c[2], c[3], c[1], fcmBody);
            runCase("AUTH\t" + c[0], keyJson, port, 10000, null);
        }
        // 401 본문의 status·세부 코드가 분류에 쓰이는지(재시도하지 않는 경로와 재시도하는 경로 모두)
        reset();
        fcmStatus = 401;
        fcmBody = "{\"error\":{\"code\":401,\"status\":\"PERMISSION_DENIED\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}";
        fcmHeaders = Map.of("WWW-Authenticate", List.of("Bearer realm=\"x\""));
        currentInput = input("401", "WWW-Authenticate", "Bearer realm=\"x\"", "", fcmBody);
        runCase("AUTH\t401_bearer_realm_body_permission_denied", keyJson, port, 10000, null);
        for (String[] c : new String[][]{
                {"401_body_permission_denied", "{\"error\":{\"code\":401,\"status\":\"PERMISSION_DENIED\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"UNREGISTERED\"}]}}"},
                {"401_empty_body", ""}, {"401_text_body", "denied"}}) {
            reset();
            fcmStatus = 401;
            fcmBody = c[1];
            currentInput = input("401", "", "", "", fcmBody);
            runCase("AUTH\t" + c[0], keyJson, port, 10000, null);
        }

        // ---- 5. 전송 오류 ----
        reset();
        fcmDelayMs = 3000;
        currentInput = "IN\tdelay=3000ms\ttimeout=1000ms";
        runCase("IO\tread_timeout", keyJson, port, 1000, null);
        reset();
        currentInput = "IN\tconnection refused";
        runCase("IO\tconnection_refused", keyJson, port, 10000, "http://127.0.0.1:1");
        reset();
        currentInput = "IN\tunknown host";
        runCase("IO\tunknown_host", keyJson, port, 10000, "http://no-such-host.invalid:80");

        // ---- 6. 토큰 교환 실패 ----
        for (String[] c : new String[][]{
                {"token_400_invalid_grant", "400", "{\"error\":\"invalid_grant\",\"error_description\":\"Invalid JWT Signature.\"}"},
                {"token_401", "401", "{\"error\":\"unauthorized_client\"}"},
                {"token_500", "500", "{\"error\":\"backend_error\"}"},
                {"token_429", "429", "{\"error\":\"rate\"}"},
                {"token_503", "503", "{\"error\":\"unavailable\"}"},
                {"token_200_not_json", "200", "<html>"},
                {"token_200_no_token", "200", "{\"expires_in\":3600}"},
                {"token_200_no_expiry", "200", "{\"access_token\":\"fixture-access-token\",\"token_type\":\"Bearer\"}"},
                {"token_403", "403", "{\"error\":\"forbidden\"}"},
                {"token_404", "404", "{}"},
                {"token_408", "408", "{}"},
                {"token_501", "501", "{}"},
                {"token_502", "502", "{}"},
                {"token_504", "504", "{}"},
                {"token_200_expires_string", "200", "{\"access_token\":\"fixture-access-token\",\"expires_in\":\"3600\"}"},
                {"token_200_expires_float", "200", "{\"access_token\":\"fixture-access-token\",\"expires_in\":3600.5}"},
                {"token_200_expires_bad_string", "200", "{\"access_token\":\"fixture-access-token\",\"expires_in\":\"soon\"}"},
                {"token_200_token_not_string", "200", "{\"access_token\":5,\"expires_in\":3600}"},
                {"token_200_empty_token", "200", "{\"access_token\":\"\",\"expires_in\":3600}"},
                {"token_200_zero_expiry", "200", "{\"access_token\":\"fixture-access-token\",\"expires_in\":0}"},
                {"token_200_short_expiry", "200", "{\"access_token\":\"fixture-access-token\",\"expires_in\":200}"},
        }) {
            reset();
            tokenStatus = Integer.parseInt(c[1]);
            tokenBody = c[2];
            currentInput = input(c[1], "", "", "", c[2]);
            runCase("TOKEN\t" + c[0], keyJson, port, 10000, null);
        }
        // 토큰 캐시: 같은 앱으로 두 번 보낼 때 토큰을 다시 받는 시점(만료까지 남은 시간)
        for (int expiresIn : new int[]{60, 200, 225, 226, 240, 300, 301, 400, 3600}) {
            reset();
            tokenBody = "{\"access_token\":\"fixture-access-token\",\"expires_in\":" + expiresIn + "}";
            FirebaseApp cacheApp = newApp(keyJson, port, 10000);
            FcmDevicePushSender cacheSender = new FcmDevicePushSender(FirebaseMessaging.getInstance(cacheApp), Clock.fixed(NOW, ZoneOffset.UTC));
            DevicePushRequest cacheRequest = new DevicePushRequest(
                    new PushMessage(UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), NotificationType.POST_APPROVED, NotificationSourceType.POST, UUID.randomUUID(), NOW, NOW.plusSeconds(1800)),
                    "fcm-token", "제목", "본문", NOW.plusSeconds(1800));
            cacheSender.send(cacheRequest);
            cacheSender.send(cacheRequest);
            System.out.println("CACHE\texpires_in=" + expiresIn + "\ttokenCalls=" + tokenCalls.get() + "\tfcmCalls=" + fcmCalls.get());
            cacheApp.delete();
        }
        // 토큰 서버에 연결할 수 없을 때
        reset();
        currentInput = "IN\ttoken server refuses connection";
        String deadKey = keyJson.replace("http://127.0.0.1:" + port + "/token", "http://127.0.0.1:1/token");
        runCase("TOKEN\ttoken_connection_refused", deadKey, port, 10000, null);
        System.exit(0);
    }

    static void reset() {
        fcmRequests.clear();
        tokenRequests.clear();
        fcmCalls.set(0);
        tokenCalls.set(0);
        fcmStatus = 200;
        fcmBody = "{\"name\":\"projects/test/messages/accepted\"}";
        fcmHeaders = Map.of();
        acceptAfter = Integer.MAX_VALUE;
        fcmDelayMs = 0;
        currentInput = "";
        tokenStatus = 200;
        tokenBody = "{\"access_token\":\"fixture-access-token\",\"expires_in\":3600,\"token_type\":\"Bearer\"}";
    }

    static void runCase(String label, String keyJson, int port, int timeoutMs, String redirectTo) throws Exception {
        FirebaseApp app = redirectTo == null ? newApp(keyJson, port, timeoutMs) : newAppTo(keyJson, port, timeoutMs, redirectTo);
        FcmDevicePushSender sender = new FcmDevicePushSender(FirebaseMessaging.getInstance(app), Clock.fixed(NOW, ZoneOffset.UTC));
        DevicePushRequest request = new DevicePushRequest(
                new PushMessage(UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(), NotificationType.POST_APPROVED, NotificationSourceType.POST, UUID.randomUUID(), NOW, NOW.plusSeconds(1800)),
                "fcm-token", "제목", "본문", NOW.plusSeconds(1800));
        long started = System.nanoTime();
        String outcome;
        try {
            DevicePushResult result = sender.send(request);
            outcome = "status=" + result.status() + " errorCode=" + result.errorCode() + " retryAfter=" + result.retryAfter().toSeconds();
        } catch (RuntimeException e) {
            outcome = "THROWN " + e.getClass().getSimpleName();
        }
        long elapsedMs = (System.nanoTime() - started) / 1_000_000;
        // 소요 시간은 초 단위로 어림해 남긴다(대기 시간이 구현마다 조금씩 어긋나므로 정확한 값은 비교하지 않는다).
        System.out.println(label + "\t" + outcome + "\tfcmCalls=" + fcmCalls.get() + "\ttokenCalls=" + tokenCalls.get() + "\tseconds=" + Math.round(elapsedMs / 1000.0) + "\t" + currentInput);
        app.delete();
    }

    static FirebaseApp newApp(String keyJson, int port, int timeoutMs) throws Exception {
        return newAppTo(keyJson, port, timeoutMs, "http://127.0.0.1:" + port);
    }

    static FirebaseApp newAppTo(String keyJson, int port, int timeoutMs, String target) throws Exception {
        NetHttpTransport transport = new NetHttpTransport.Builder()
                .setConnectionFactory(url -> (HttpURLConnection) URI.create(url.toString().replace(
                        "https://fcm.googleapis.com", target)).toURL().openConnection())
                .build();
        return FirebaseApp.initializeApp(FirebaseOptions.builder()
                .setCredentials(ServiceAccountCredentials.fromStream(new ByteArrayInputStream(keyJson.getBytes(StandardCharsets.UTF_8))))
                .setProjectId("test")
                .setConnectTimeout(timeoutMs).setReadTimeout(timeoutMs).setWriteTimeout(timeoutMs)
                .setHttpTransport(transport).build(), UUID.randomUUID().toString());
    }

    /** 표(TSV) 한 칸에 담도록 역슬래시·개행·탭을 이스케이프한다. */
    static String esc(String text) {
        return text.replace("\\", "\\\\").replace("\n", "\\n").replace("\t", "\\t");
    }

    static String input(String status, String headerName, String headerValue, String acceptAfter, String body) {
        return "IN\t" + status + "\t" + headerName + "\t" + headerValue + "\t" + acceptAfter + "\t" + esc(body);
    }

    static String httpDate(Instant at) {
        return java.time.format.DateTimeFormatter.RFC_1123_DATE_TIME.format(at.atOffset(ZoneOffset.UTC));
    }

    static String err(int code, String status, String detail) {
        return "{\"error\":{\"code\":" + code + ",\"status\":\"" + status + "\",\"message\":\"failure\",\"details\":[{\"@type\":\"type.googleapis.com/google.firebase.fcm.v1.FcmError\",\"errorCode\":\"" + detail + "\"}]}}";
    }

    static String jsonString(String s) {
        return "\"" + s.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n") + "\"";
    }

    /** 토큰 요청의 assertion(JWT)을 풀어 헤더·클레임을 보인다. iat/exp는 서로의 차이로 바꾼다. */
    static String normalizeToken(String request) {
        int at = request.indexOf("assertion=");
        if (at < 0) return request;
        String assertion = request.substring(at + "assertion=".length());
        String[] parts = assertion.split("\\.");
        String header = new String(Base64.getUrlDecoder().decode(parts[0]), StandardCharsets.UTF_8);
        String payload = new String(Base64.getUrlDecoder().decode(parts[1]), StandardCharsets.UTF_8);
        java.util.regex.Matcher iat = java.util.regex.Pattern.compile("\"iat\":(\\d+)").matcher(payload);
        java.util.regex.Matcher exp = java.util.regex.Pattern.compile("\"exp\":(\\d+)").matcher(payload);
        String delta = "";
        if (iat.find() && exp.find()) delta = " exp-iat=" + (Long.parseLong(exp.group(1)) - Long.parseLong(iat.group(1)));
        payload = payload.replaceAll("\"iat\":\\d+", "\"iat\":N").replaceAll("\"exp\":\\d+", "\"exp\":N");
        return request.substring(0, at) + "assertion HEADER=" + header + " PAYLOAD=" + payload + delta + " SIGNATURE_BYTES=" + Base64.getUrlDecoder().decode(parts[2]).length;
    }
}
