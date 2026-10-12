//! HTTP 계층. 원본에서 서블릿 컨테이너, 보안 필터 체인, `DispatcherServlet`이 맡던 일을 미들웨어로 옮겼다.
//!
//! 요청이 지나는 순서(바깥에서 안쪽):
//! 1. [`tomcat`]: 컨테이너가 필터 앞에서 처리하는 것(`TRACE`).
//! 2. [`access`]: requestId와 접근 로그(`RequestIdFilter`).
//! 3. [`callback_limit`]: 이미지 처리 콜백 본문 크기 제한(`CallbackBodySizeLimitFilter`).
//! 4. [`firewall`]: 위험한 경로·메서드 거절(`StrictHttpFirewall`).
//! 5. [`security_headers`]: 보안 응답 헤더.
//! 6. [`cors`]: 관리자 API의 CORS.
//! 7. [`bearer`]: Bearer 토큰 인증.
//! 8. [`authorize`]: 경로별 인가.
//! 9. [`dispatch`]: Actuator, 오류 페이지, 애플리케이션 라우터.

pub mod access;
pub mod actuator;
pub mod authorize;
pub mod bearer;
pub mod boot;
pub mod callback_limit;
pub mod cors;
pub mod dispatch;
pub mod firewall;
pub mod security_headers;
pub mod tomcat;
