//! 응답에 붙이는 내부 표지. 서버의 콘텐츠 협상 계층(`chalkak-server`의 `web::negotiate`)이 읽고 지운다.
//!
//! 원본에서는 오류 응답이 어떤 경로로 만들어졌는지에 따라 `Accept`를 JSON으로 받을 수 없을 때의 결과가 달라진다.
//! - Spring이 던진 예외(`DefaultHandlerExceptionResolver`가 처리): 같은 상태 코드로 컨테이너 오류 페이지를 돌려준다.
//! - 도메인 예외·처리되지 않은 예외: 처리기가 JSON을 쓰지 못하면 예외가 컨테이너까지 올라가 500이 된다.
//!
//! 응답을 만드는 쪽이 이 표지로 어느 경우인지 알린다. 표지는 클라이언트에게 나가지 않는다.

/// 오류 응답의 출처를 알리는 헤더 이름.
pub const ORIGIN: &str = "x-chalkak-origin";
/// Spring MVC가 던진 예외(400/404/405/415 등).
pub const SPRING: &str = "spring";
/// `BaseException` 계열과 처리되지 않은 예외.
pub const HANDLER: &str = "handler";
/// 컨테이너·보안 방화벽이 컨트롤러 앞에서 끊어 `/error`로 보낸 응답(Boot 오류 본문).
pub const BOOT: &str = "boot";
/// 액추에이터 응답(자체 미디어 타입 협상을 한다).
pub const ACTUATOR: &str = "actuator";
/// springdoc의 YAML 문서(`/v3/api-docs.yaml`). 핸들러 매핑이 `application/vnd.oai.openapi`만 만든다고 선언한다.
pub const DOCS_YAML: &str = "docs-yaml";
/// springdoc의 JSON 문서·설정(응답 타입이 `application/json`으로 고정).
pub const DOCS_JSON: &str = "docs-json";
/// HTML 정적 리소스에서 난 오류라는 표시. 이 헤더가 있으면 `Content-Disposition` 확장자 방어에서 `.html`이 안전하다.
pub const HTML_RESOURCE: &str = "x-chalkak-html-resource";
/// 요청 파라미터를 읽다 컨테이너가 예외를 던진 응답(`Request.getParameter`의 `IllegalStateException`).
/// 처리기가 JSON을 쓰면 500이지만, 쓰지 못하면 컨테이너가 파라미터 해석 실패로 둔 400이 남는다.
pub const PARAM: &str = "param";
/// [`PARAM`]과 같되 액추에이터 엔드포인트에서 난 것. 핸들러 매핑의 `produces` 검사가 먼저 돈다.
pub const ACTUATOR_PARAM: &str = "actuator-param";

/// 협상에 실패했을 때만 실제 헤더가 되는 값. 이름은 `<접두사><원래 헤더 이름>`이다.
pub const CARRY_PREFIX: &str = "x-chalkak-error-";
pub const CARRY_ALLOW: &str = "x-chalkak-error-allow";
pub const CARRY_ACCEPT: &str = "x-chalkak-error-accept";

/// 요청 본문 타입이 맞지 않을 때(415) 원본이 `Accept`에 싣는 목록.
pub const SUPPORTED_BODY_TYPES: &str = "application/json, application/yaml, application/xml;charset=UTF-8, text/xml;charset=UTF-8, application/*+json, application/*+xml;charset=UTF-8";
