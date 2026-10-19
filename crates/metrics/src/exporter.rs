//! Bounded HTTP/1 Prometheus exporter. Node health/readiness belong to RPC.
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    body::Incoming, header::CONTENT_TYPE, server::conn::http1, service::service_fn, Method,
    Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use prometheus::{Encoder, TextEncoder};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore};
use tracing::{info, warn};

/// Serve metrics only; a metrics listener is not evidence of node readiness.
pub async fn start_metrics_exporter(addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .context("binding Prometheus metrics listener")?;
    let permits = Arc::new(Semaphore::new(64));
    info!(%addr, "Prometheus exporter listening at /metrics");
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            // Dropping the socket bounds simultaneous exporter work.
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            let mut http = http1::Builder::new();
            http.timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_buf_size(16 * 1024);
            if let Err(error) = http
                .serve_connection(TokioIo::new(stream), service_fn(metrics_handler))
                .await
            {
                tracing::debug!(%error, "Metrics connection closed");
            }
        });
    }
}

async fn metrics_handler(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let mut response = Response::new(Full::new(Bytes::new()));
    if req.uri().path() != "/metrics" {
        *response.status_mut() = StatusCode::NOT_FOUND;
        return Ok(response);
    }
    if req.method() != Method::GET {
        *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
        return Ok(response);
    }
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    match encoder.encode(&prometheus::gather(), &mut buffer) {
        Ok(()) => {
            *response.body_mut() = Full::new(Bytes::from(buffer));
            response.headers_mut().insert(
                CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/plain; version=0.0.4"),
            );
        }
        Err(error) => {
            warn!(%error, "Failed to encode metrics");
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    Ok(response)
}
