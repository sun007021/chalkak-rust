//! 보안 단계를 통과한 요청을 받는 마지막 핸들러. 원본에서는 `DispatcherServlet`과 컨테이너 오류 디스패치에 해당한다.
//!
//! - `/error`: 컨테이너 오류 페이지.
//! - `/actuator/**`: Actuator 엔드포인트(맡지 않는 경로는 아래 라우터로).
//! - 관리자가 아닌 경로의 CORS 사전 요청(OPTIONS + `Origin` + `Access-Control-Request-Method`): 헤더 없는 빈 200.
//!   (관리자 경로의 사전 요청은 앞의 CORS 계층이 답한다.)
//! - 그 밖: 애플리케이션 라우터(404·405 응답 포함).

use chalkak_http::{Handler, Method, Request, Response, Router};

use super::actuator::{Actuator, is_actuator_path};

pub struct Dispatcher {
    router: Router,
    actuator: Actuator,
}

impl Dispatcher {
    pub fn new(router: Router, actuator: Actuator) -> Self {
        Dispatcher { router, actuator }
    }
}

fn is_preflight(req: &Request) -> bool {
    req.method == Method::Options
        && req.headers.contains("origin")
        && req.headers.contains("access-control-request-method")
}

impl Handler for Dispatcher {
    fn handle(&self, req: Request) -> Response {
        if req.path == "/error" {
            return self.actuator.error_page();
        }
        if is_actuator_path(&req.path)
            && let Some(response) = self.actuator.handle(&req)
        {
            return response;
        }
        if is_preflight(&req) {
            return Response::new(200);
        }
        self.router.handle(req)
    }
}
