
pub mod tokiort;
use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response};

use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};

use tokiort::TokioIo;
type ClientBuilder = hyper::client::conn::http1::Builder;
type ServerBuilder = hyper::server::conn::http1::Builder;

use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info, instrument};
use tracing_subscriber::EnvFilter;

// To try this example:
// 1. cargo run --example http_proxy
// 2. config http_proxy in command line
//    $ export http_proxy=http://127.0.0.1:8100
//    $ export https_proxy=http://127.0.0.1:8100
// 3. send requests
//    $ curl -i https://www.some_domain.com/
#[derive(Deserialize, Clone)]
struct Route {
    host: String,    // 支持以 '.' 开头表示 suffix 匹配，如 ".example.com"
    backend: String, // 后端地址，可以是 "1.2.3.4:8080" 或 "backend.internal"（若无端口使用请求端口）
}

#[derive(Deserialize, Clone)]
struct Config {
    routes: Vec<Route>,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Initialize tracing subscriber; include file and line number in logs.
    tracing_subscriber::fmt()
        .with_file(true) // show source file
        .with_line_number(true) // show line number
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let addr = SocketAddr::from(([127, 0, 0, 1], 8100));
    info!(msg = "Proxy Start Listening", ?addr);
    // 读取并解析 proxy.toml（位于项目根）
    let cfg_str = std::fs::read_to_string("proxy.toml")?;
    let cfg: Config = toml::from_str(&cfg_str)?;
    let cfg = Arc::new(cfg);

    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let cfg_clone = cfg.clone();

        tokio::task::spawn(async move {
            if let Err(err) = ServerBuilder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                // 把配置捕获到每个连接的 service 中
                .serve_connection(io, service_fn(move |req| proxy(req, cfg_clone.clone())))
                .with_upgrades()
                .await
            {
                error!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

#[instrument(skip(cfg))]
async fn proxy(
    req: Request<hyper::body::Incoming>,
    cfg: Arc<Config>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    debug!("req: {:?}", req);

    // helper: find backend by host according to cfg rules
    fn find_backend<'a>(host: &str, cfg: &'a Config) -> Option<&'a str> {
        for r in &cfg.routes {
            if r.host.starts_with('.') {
                let suf = &r.host[1..];
                if host.ends_with(suf) {
                    return Some(r.backend.as_str());
                }
            } else if r.host == host {
                return Some(r.backend.as_str());
            }
        }
        None
    }

    if Method::CONNECT == req.method() {
        // CONNECT target like "host:port"
        if let Some(orig_addr) = host_addr(req.uri()) {
            // extract host part to match rules
            let host_part = orig_addr.split(':').next().unwrap_or(&orig_addr);
            let port_part = orig_addr.split(':').nth(1).map(|s| s.to_string());

            // determine backend address
            let backend_addr = if let Some(b) = find_backend(host_part, &cfg) {
                if b.contains(':') {
                    b.to_string()
                } else if let Some(p) = port_part {
                    format!("{}:{}", b, p)
                } else {
                    b.to_string()
                }
            } else {
                orig_addr.clone()
            };

            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(upgraded, backend_addr).await {
                            error!("server io error: {}", e);
                        };
                    }
                    Err(e) => error!("upgrade error: {}", e),
                }
            });

            Ok(Response::new(empty()))
        } else {
            error!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;

            Ok(resp)
        }
    } else {
        // normal HTTP request: use req.uri() host/port to decide backend
        let host = req.uri().host().expect("uri has no host");
        let port = req.uri().port_u16().unwrap_or(80);

        let target = if let Some(b) = find_backend(host, &cfg) {
            if b.contains(':') {
                b.to_string()
            } else {
                format!("{}:{}", b, port)
            }
        } else {
            format!("{}:{}", host, port)
        };

        let stream = TcpStream::connect(target)
            .await
            .map_err(|e| {
                error!("connect backend error: {}", e);
                e
            })
            .unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = ClientBuilder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                error!("Connection failed: {:?}", err);
            }
        });

        let resp = sender.send_request(req).await?;
        Ok(resp.map(|b| b.boxed()))
    }
}

#[instrument(skip(upgraded), level = "info")]
// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
async fn tunnel(upgraded: Upgraded, addr: String) -> anyhow::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    info!(
        "client wrote {} bytes and received {} bytes",
        from_client, from_server
    );

    Ok(())
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}
