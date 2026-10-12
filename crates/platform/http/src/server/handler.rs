//! `Handler` 트레이트와 미들웨어 체인.

use std::sync::Arc;

use super::request::Request;
use super::response::Response;

/// 요청 하나를 응답 하나로 바꾼다. 여러 워커 스레드가 동시에 호출하므로 `Send + Sync`.
/// 핸들러가 패닉하면 서버가 500으로 응답하고 그 연결을 닫는다.
pub trait Handler: Send + Sync {
    fn handle(&self, req: Request) -> Response;
}

impl<F> Handler for F
where
    F: Fn(Request) -> Response + Send + Sync,
{
    fn handle(&self, req: Request) -> Response {
        self(req)
    }
}

impl Handler for Arc<dyn Handler> {
    fn handle(&self, req: Request) -> Response {
        (**self).handle(req)
    }
}

/// 요청을 가로채는 계층. `next`를 부르면 안쪽 계층(마지막에는 엔드포인트)이 실행된다.
/// `next`를 부르지 않고 응답을 직접 돌려 단락시킬 수도 있다.
pub trait Middleware: Send + Sync {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response;
}

impl<F> Middleware for F
where
    F: Fn(Request, &dyn Handler) -> Response + Send + Sync,
{
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        self(req, next)
    }
}

/// 엔드포인트 앞에 미들웨어를 쌓은 핸들러.
///
/// 먼저 추가한 계층이 가장 바깥이다(요청은 추가한 순서대로 지나가고 응답은 반대로 돌아온다).
/// 예: `Chain::new(router).layer(request_id).layer(security_headers)` 이면
/// 요청은 request_id → security_headers → router, 응답은 router → security_headers → request_id.
pub struct Chain {
    layers: Vec<Arc<dyn Middleware>>,
    endpoint: Arc<dyn Handler>,
}

impl Chain {
    pub fn new(endpoint: impl Handler + 'static) -> Self {
        Chain {
            layers: Vec::new(),
            endpoint: Arc::new(endpoint),
        }
    }

    pub fn layer(mut self, middleware: impl Middleware + 'static) -> Self {
        self.layers.push(Arc::new(middleware));
        self
    }
}

struct Next<'a> {
    chain: &'a Chain,
    index: usize,
}

impl Handler for Next<'_> {
    fn handle(&self, req: Request) -> Response {
        match self.chain.layers.get(self.index) {
            Some(layer) => layer.handle(
                req,
                &Next {
                    chain: self.chain,
                    index: self.index + 1,
                },
            ),
            None => self.chain.endpoint.handle(req),
        }
    }
}

impl Handler for Chain {
    fn handle(&self, req: Request) -> Response {
        Next {
            chain: self,
            index: 0,
        }
        .handle(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::method::Method;
    use std::sync::Mutex;

    fn req() -> Request {
        Request::new(Method::Get, "/")
    }

    #[test]
    fn closures_are_handlers() {
        let h = |r: Request| Response::new(200).with_body(r.path.into_bytes());
        assert_eq!(h.handle(req()).body, b"/");
    }

    #[test]
    fn layers_run_outermost_first_and_unwind_in_reverse() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let mk = |name: &'static str, log: Arc<Mutex<Vec<String>>>| {
            move |r: Request, next: &dyn Handler| {
                log.lock().unwrap().push(format!("{name} in"));
                let resp = next.handle(r);
                log.lock().unwrap().push(format!("{name} out"));
                resp.with_header("X-Seen", name)
            }
        };
        let l = log.clone();
        let endpoint = move |_r: Request| {
            l.lock().unwrap().push("endpoint".into());
            Response::new(200)
        };
        let chain = Chain::new(endpoint)
            .layer(mk("a", log.clone()))
            .layer(mk("b", log.clone()));
        let resp = chain.handle(req());
        assert_eq!(
            *log.lock().unwrap(),
            ["a in", "b in", "endpoint", "b out", "a out"]
        );
        let seen: Vec<_> = resp.headers.get_all("x-seen").collect();
        assert_eq!(seen, ["b", "a"]);
    }

    #[test]
    fn a_layer_can_short_circuit() {
        let chain = Chain::new(|_r: Request| -> Response { panic!("must not run") })
            .layer(|_r: Request, _next: &dyn Handler| Response::new(401));
        assert_eq!(chain.handle(req()).status, 401);
    }

    #[test]
    fn a_layer_can_modify_the_request() {
        let chain = Chain::new(|r: Request| Response::new(200).with_body(r.path.into_bytes()))
            .layer(|mut r: Request, next: &dyn Handler| {
                r.path = "/rewritten".into();
                next.handle(r)
            });
        assert_eq!(chain.handle(req()).body, b"/rewritten");
    }

    #[test]
    fn chain_without_layers_calls_the_endpoint() {
        let chain = Chain::new(|_r: Request| Response::new(204));
        assert_eq!(chain.handle(req()).status, 204);
    }
}
