//! Request-id generation (prompt §58). Plugs into `tower_http::request_id`'s
//! `SetRequestIdLayer`/`PropagateRequestIdLayer` once `api` exists (Phase 3) to
//! assign every inbound request an id that's returned in response headers and threaded
//! into logs, internal RPC, and metrics/traces.

use http::Request;
use tower_http::request_id::{MakeRequestId, RequestId};
use uuid::Uuid;

/// Mints a UUIDv7 request id for every request that doesn't already carry one
/// (`MakeRequestId`'s contract). UUIDv7 rather than v4 so request ids sort roughly by
/// time, which is convenient when grepping logs.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7RequestId;

impl MakeRequestId for UuidV7RequestId {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        let id = Uuid::now_v7().to_string();
        http::HeaderValue::from_str(&id).ok().map(RequestId::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_a_distinct_id_on_every_call() {
        let mut maker = UuidV7RequestId;
        let req = Request::builder().body(()).unwrap();

        let first = maker.make_request_id(&req).unwrap();
        let second = maker.make_request_id(&req).unwrap();

        assert_ne!(first.header_value(), second.header_value());
    }

    #[test]
    fn minted_id_parses_back_as_a_valid_uuid() {
        let mut maker = UuidV7RequestId;
        let req = Request::builder().body(()).unwrap();

        let id = maker.make_request_id(&req).unwrap();
        let as_str = id.header_value().to_str().unwrap();
        assert!(Uuid::parse_str(as_str).is_ok());
    }
}
