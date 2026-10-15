//! Stop admission before draining requests, without cancelling their futures.

use actix_web::{
    body::{BodySize, EitherBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    middleware::Next,
    web::{Bytes, Data},
    Error, HttpMessage, HttpResponse,
};
use atomic_core::executor::{WorkGuard, WorkTracker};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::watch;

pub struct Shutdown {
    stopping: watch::Sender<bool>,
    work: WorkTracker,
    close_streams: watch::Sender<bool>,
    streams: WorkTracker,
}

pub(crate) struct AdmittedRequest;

impl Default for Shutdown {
    fn default() -> Self {
        Self {
            stopping: watch::channel(false).0,
            work: WorkTracker::default(),
            close_streams: watch::channel(false).0,
            streams: WorkTracker::default(),
        }
    }
}

impl Shutdown {
    pub fn admit(&self) -> Option<WorkGuard> {
        // Keep the watch read lock until accounting starts: stop cannot race
        // between the admission check and registration of this request/tick.
        let stopping = self.stopping.borrow();
        if *stopping {
            None
        } else {
            Some(self.work.start())
        }
    }

    pub fn begin(&self) {
        self.stopping.send_replace(true);
    }

    pub async fn stopped(&self) {
        let mut stopping = self.stopping.subscribe();
        stopping
            .wait_for(|value| *value)
            .await
            .expect("shutdown closed");
    }

    pub async fn wait_idle(&self) {
        self.work.wait_idle().await;
    }

    pub fn websocket(&self) -> WorkGuard {
        self.streams.start()
    }

    pub async fn streams_closed(&self) {
        self.close_streams
            .subscribe()
            .wait_for(|value| *value)
            .await
            .expect("stream stop closed");
    }

    /// Producers are finished; flush queued events before closing sockets.
    pub async fn close_streams(&self) {
        self.close_streams.send_replace(true);
        self.streams.wait_idle().await;
    }
}

pub async fn admission<B: MessageBody + 'static>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<EitherBody<DrainingBody<B>>>, Error> {
    let shutdown = req
        .app_data::<Data<Shutdown>>()
        .expect("shutdown state")
        .clone();
    let mut work = shutdown.admit();
    // Existing MCP sessions may need handshake/reply notifications to finish
    // admitted calls. The transport rejects new JSON-RPC requests without this
    // marker; protocol housekeeping itself does not keep shutdown alive.
    if work.is_some() {
        req.extensions_mut().insert(AdmittedRequest);
    } else if !(req.method() == actix_web::http::Method::POST
        && req.path().trim_end_matches('/') == "/mcp")
    {
        return Ok(req
            .into_response(HttpResponse::ServiceUnavailable().finish())
            .map_into_right_body());
    }
    let response = next.call(req).await?;
    // An upgraded WS body lives for the socket's lifetime, not one request.
    // Its actor and body EOF are drained separately after producers finish.
    if response.status() == actix_web::http::StatusCode::SWITCHING_PROTOCOLS {
        work = Some(shutdown.websocket());
    }
    Ok(response
        .map_body(|_, body| DrainingBody {
            body: Box::pin(body),
            work,
        })
        .map_into_left_body())
}

pub struct DrainingBody<B> {
    body: Pin<Box<B>>,
    work: Option<WorkGuard>,
}

impl<B: MessageBody> MessageBody for DrainingBody<B> {
    type Error = B::Error;

    fn size(&self) -> BodySize {
        self.body.size()
    }

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let result = self.body.as_mut().poll_next(cx);
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            self.work.take();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{middleware, test, web, App};
    use std::time::Duration;

    #[actix_web::test]
    async fn stop_rejects_new_requests_but_waits_for_existing_body() {
        let shutdown = Data::new(Shutdown::default());
        let app = test::init_service(
            App::new()
                .app_data(shutdown.clone())
                .wrap(middleware::from_fn(admission))
                .route(
                    "/",
                    web::get().to(|| async { HttpResponse::Ok().body("fictional result") }),
                ),
        )
        .await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
        shutdown.begin();
        shutdown.begin();
        assert!(
            // The former Actix cutoff must not end an admitted response.
            tokio::time::timeout(Duration::from_millis(2100), shutdown.wait_idle())
                .await
                .is_err()
        );
        let refused =
            test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
        assert_eq!(
            refused.status(),
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(test::read_body(response).await, "fictional result");
        shutdown.wait_idle().await;
    }
}
