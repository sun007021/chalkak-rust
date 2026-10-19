import com.chalkak.backend.notification.service.PushMessage;
import java.util.LinkedHashMap;
import java.util.Map;
import tools.jackson.databind.DeserializationFeature;
import tools.jackson.databind.ObjectMapper;
import tools.jackson.databind.json.JsonMapper;

/** SQS 본문을 원본 PushMessage 로 읽을 때의 결과(레코드 또는 예외 이름)를 표로 출력한다. 생성: run_push_message.sh */
public class PushMessageFixture {
    static final String E = "0190f3a2-7b1c-7d2e-8f4a-123456789abc";

    static String body(String field, String value) {
        Map<String, String> m = new LinkedHashMap<>();
        m.put("eventId", "\"" + E + "\"");
        m.put("notificationId", "\"" + E.replace("abc", "abd") + "\"");
        m.put("userId", "\"" + E.replace("abc", "abe") + "\"");
        m.put("type", "\"POST_APPROVED\"");
        m.put("sourceType", "\"POST\"");
        m.put("sourceId", "\"" + E.replace("abc", "abf") + "\"");
        m.put("occurredAt", "\"2026-10-07T00:00:00Z\"");
        m.put("expiresAt", "\"2026-10-07T00:30:00Z\"");
        if (field != null) m.put(field, value);
        StringBuilder sb = new StringBuilder("{");
        m.forEach((k, v) -> sb.append(sb.length() > 1 ? "," : "").append('"').append(k).append("\":").append(v));
        return sb.append('}').toString();
    }

    static String esc(String s) {
        return s.replace("\\", "\\\\").replace("\n", "\\n").replace("\t", "\\t");
    }

    public static void main(String[] args) {
        ObjectMapper mapper = JsonMapper.builder().disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES).build();
        String ok = body(null, null);
        Map<String, String> cases = new LinkedHashMap<>();
        cases.put("valid", ok);
        cases.put("empty", "");
        cases.put("blank", "  \n");
        cases.put("invalid", "invalid");
        cases.put("open_brace", "{");
        cases.put("cut_value", "{\"eventId\":");
        cases.put("cut_string", "{\"eventId\":\"abc");
        cases.put("null", "null");
        cases.put("null_padded", "  null  ");
        cases.put("array_empty", "[]");
        cases.put("array_one", "[1]");
        cases.put("string", "\"x\"");
        cases.put("number", "123");
        cases.put("true", "true");
        cases.put("object_empty", "{}");
        cases.put("trailing_text", ok + " trailing");
        cases.put("trailing_object", ok + ok);
        cases.put("trailing_brace", ok + "}");
        cases.put("unknown_property", ok.replace("{", "{\"extra\":1,"));
        cases.put("duplicate_key", ok.replace("{", "{\"type\":\"TOPIC\","));
        cases.put("trailing_number", ok + " 1");
        cases.put("trailing_true", ok + " true");
        cases.put("trailing_null", ok + " null");
        cases.put("trailing_array", ok + " [");
        cases.put("trailing_string_open", ok + " \"abc");
        cases.put("trailing_whitespace", ok + "  \n\t ");
        cases.put("trailing_short_literal", ok + " tru");
        cases.put("trailing_bad_number", ok + " 01");
        cases.put("trailing_comma", ok + ",");
        cases.put("literal_suffix", "{\"a\":truex}");
        cases.put("leading_zero", "{\"a\":01}");
        cases.put("minus_only", "{\"a\":-}");
        cases.put("exponent_missing", "{\"a\":1e}");
        cases.put("nested_unknown", ok.replace("{", "{\"a\":{\"b\":[1,2,{\"c\":null}]},"));
        cases.put("unicode_escape", ok.replace("{", "{\"a\":\"\\u00e9\\ud83d\\ude00\","));
        cases.put("lone_surrogate", ok.replace("{", "{\"a\":\"\\ud800\","));
        cases.put("bad_escape", ok.replace("{", "{\"a\":\"\\q\","));
        cases.put("bad_unicode_escape", ok.replace("{", "{\"a\":\"\\u12g4\","));
        cases.put("missing_colon", "{\"a\" 1}");
        cases.put("missing_value_close", "{\"a\":1");
        cases.put("key_not_string", "{1:2}");
        cases.put("deep_1000", "[".repeat(1000) + "]".repeat(1000));
        for (int n : new int[]{1, 100, 498, 499, 500, 501, 1000}) {
            cases.put("deep_unknown_" + n, "{\"a\":" + "[".repeat(n) + "]".repeat(n) + "}");
        }
        cases.put("garbage_after_bad_field", "{\"eventId\":\"bad\",garbage");
        cases.put("garbage_after_array_field", "{\"eventId\":[1,garbage");
        cases.put("unterminated_after_enum", "{\"type\":\"FOO\",\"x\":");
        cases.put("deep_1001", "[".repeat(1001) + "]".repeat(1001));
        cases.put("bom", "\uFEFF" + ok);
        cases.put("comment", "/* c */" + ok);
        cases.put("single_quotes", ok.replace('"', '\''));
        cases.put("unquoted_name", "{eventId:\"" + E + "\"}");
        cases.put("trailing_comma", ok.replace("}", ",}"));
        cases.put("nan", body("occurredAt", "NaN"));
        cases.put("control_char", body("type", "\"POST\u0001\""));
        for (String field : new String[]{"eventId", "notificationId", "userId", "sourceId"}) {
            for (String[] v : new String[][]{{"bad", "\"x\""}, {"empty", "\"\""}, {"blank", "\" \""}, {"number", "123"}, {"object", "{}"},
                    {"array", "[]"}, {"bool", "true"}, {"null", "null"}, {"upper", "\"" + E.toUpperCase() + "\""},
                    {"nodash", "\"" + E.replace("-", "") + "\""}, {"braced", "\"{" + E + "}\""}, {"urn", "\"urn:uuid:" + E + "\""},
                    {"short", "\"" + E.substring(1) + "\""}, {"base64_24", "\"AAAAAAAAAAAAAAAAAAAAAA==\""}, {"padded", "\" " + E + "\""}, {"float", "1.5"}}) {
                cases.put(field + "_" + v[0], body(field, v[1]));
            }
        }
        for (String field : new String[]{"type", "sourceType"}) {
            for (String[] v : new String[][]{{"unknown", "\"FOO\""}, {"empty", "\"\""}, {"blank", "\" \""}, {"zero", "0"}, {"one", "1"}, {"big", "9"}, {"negative", "-1"},
                    {"float", "1.5"}, {"lower", "\"post_approved\""}, {"lower_source", "\"post\""}, {"bool", "true"}, {"object", "{}"}, {"array", "[]"},
                    {"null", "null"}, {"numstr", "\"0\""}, {"topic", "\"TOPIC\""}, {"notice", "\"NOTICE\""}, {"padded", "\" POST\""}}) {
                cases.put(field + "_" + v[0], body(field, v[1]));
            }
        }
        for (String field : new String[]{"occurredAt", "expiresAt"}) {
            for (String[] v : new String[][]{{"fraction", "\"2026-10-07T00:00:00.123456789Z\""}, {"offset", "\"2026-10-07T09:00:00+09:00\""},
                    {"offset_nocolon", "\"2026-10-07T09:00:00+0900\""}, {"no_zone", "\"2026-10-07T00:00:00\""}, {"date_only", "\"2026-10-07\""},
                    {"empty", "\"\""}, {"blank", "\" \""}, {"epoch_int", "1791331200"}, {"epoch_float", "1791331200.5"}, {"epoch_big", "99999999999999999999"},
                    {"epoch_string", "\"1791331200\""}, {"bool", "true"}, {"object", "{}"}, {"array", "[]"}, {"null", "null"}, {"lower_z", "\"2026-10-07T00:00:00z\""},
                    {"lower_t", "\"2026-10-07t00:00:00Z\""}, {"year_plus", "\"+12026-10-07T00:00:00Z\""}, {"hour24", "\"2026-10-07T24:00:00Z\""},
                    {"sec60", "\"2026-10-07T00:00:60Z\""}, {"sec60_end", "\"2026-10-07T23:59:60Z\""}, {"feb30", "\"2026-02-30T00:00:00Z\""}, {"no_seconds", "\"2026-10-07T00:00Z\""},
                    {"padded", "\" 2026-10-07T00:00:00Z\""}, {"min_year", "\"-0001-01-01T00:00:00Z\""}, {"very_early", "\"-1000000000-01-01T00:00:00Z\""},
                    {"max", "\"+1000000000-12-31T23:59:59.999999999Z\""}, {"over_max", "\"+1000000001-01-01T00:00:00Z\""}, {"comma_fraction", "\"2026-10-07T00:00:00,5Z\""},
                    {"epoch_negative", "-1"}, {"epoch_huge_float", "1e30"}, {"epoch_nanos_int", "1791331200000000000"}}) {
                cases.put(field + "_" + v[0], body(field, v[1]));
            }
        }
        cases.forEach((label, text) -> {
            String result;
            try {
                PushMessage m = mapper.readValue(text, PushMessage.class);
                result = "OK " + m;
            } catch (Exception e) {
                result = "EXC " + e.getClass().getSimpleName();
            }
            System.out.println(label + "\t" + esc(text) + "\t" + result);
        });
    }
}
