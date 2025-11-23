use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use hyper::{
    body::HttpBody,
    client::HttpConnector,
    server::conn::AddrIncoming,
    service::{make_service_fn, service_fn},
    upgrade::Upgraded,
    Body, Client, Method, Request, Response, StatusCode,
};
use serde::Deserialize;
use tokio::{io::{copy_bidirectional, AsyncWriteExt}, net::TcpStream, sync::Semaphore};

#[derive(Deserialize)]
struct Config {
    bind: String,
    // future fields: acl, auth, etc.
    // allowed_hosts: Option<Vec<String>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // read config
    let cfg_str = std::fs::read_to_string("proxy.toml")?;
    let cfg: Config = toml::from_str(&cfg_str)?;
    let bind_addr: SocketAddr = cfg.bind.parse()?;

    let client = Client::builder().build::<_, Body>(HttpConnector::new());
    let client = Arc::new(client);

    // limit concurrent tunnels slightly (optional)
    let sem = Arc::new(Semaphore::new(1000));

    let make_svc = make_service_fn(move |_conn| {
        let client = client.clone();
        let sem = sem.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let client = client.clone();
                let sem = sem.clone();
                proxy_handler(req, client, sem)
            }))
        }
    });

    let server = hyper::Server::bind(&bind_addr).serve(make_svc);
    println!("Proxy listening on http://{}", bind_addr);
    server.await?;
    Ok(())
}

async fn proxy_handler(
    mut req: Request<Body>,
    client: Arc<Client<HttpConnector>>,
    sem: Arc<Semaphore>,
) -> Result<Response<Body>, Infallible> {
    // Handle CONNECT (tunnel)
    if req.method() == Method::CONNECT {
        // Typical CONNECT target is "host:port" in the authority or in uri path
        let target = match req.uri().authority() {
            Some(a) => a.as_str().to_string(),
            None => req.uri().path().to_string(),
        };

        // Acquire permit for tunnels
        let _permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                return Ok(Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .body(Body::from("Too many tunnels"))
                    .unwrap())
            }
        };

        // Send OK to client and then upgrade the connection
        let on_upgrade = async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    if let Err(e) = tunnel(upgraded, target).await {
                        eprintln!("tunnel error: {}", e);
                    }
                }
                Err(e) => eprintln!("upgrade error: {}", e),
            }
        };

        // spawn the upgrade handling
        tokio::spawn(on_upgrade);

        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap();
        return Ok(resp);
    }

    // Normal HTTP request: forward using hyper::Client
    // Clean proxy-specific headers
    if let Some(_) = req.headers_mut().remove("proxy-authorization") {
        // keep simple: drop proxy auth header
    }
    // remove hop-by-hop headers that should not be forwarded
    let _ = req.headers_mut().remove("proxy-connection");
    let _ = req.headers_mut().remove("connection");
    // Ensure URI is absolute; if not, try to rebuild from Host header
    let uri_string = if req.uri().scheme().is_some() && req.uri().authority().is_some() {
        req.uri().to_string()
    } else if let Some(host) = req.headers().get("host") {
        let scheme = "http";
        format!("{}://{}{}", scheme, host.to_str().unwrap_or(""), req.uri())
    } else {
        req.uri().to_string()
    };

    let mut forward_req = Request::builder()
        .method(req.method())
        .uri(uri_string)
        .body(Body::wrap_stream(req.into_body().map(|chunk| chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)) )))
        .unwrap();

    // copy headers
    *forward_req.headers_mut() = req.headers().clone();

    match client.request(forward_req).await {
        Ok(mut resp) => {
            // Return response back to client directly
            let mut builder = Response::builder()
                .status(resp.status());
            *builder.headers_mut().unwrap() = resp.headers().clone();
            let body = Body::wrap_stream(resp.body_mut().data().map(|opt| async {
                match opt {
                    Some(Ok(b)) => Ok(b),
                    Some(Err(e)) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                    None => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof")),
                }
            }).into_stream());
            // Simpler: return the original response (works because hyper types align)
            Ok(resp)
        }
        Err(e) => {
            eprintln!("forward error: {}", e);
            let resp = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!("Upstream error: {}", e)))
                .unwrap();
            Ok(resp)
        }
    }
}

async fn tunnel(mut upgraded: Upgraded, target: String) -> anyhow::Result<()> {
    // connect to target
    let mut server = TcpStream::connect(&target).await?;
    // perform bidirectional copy
    let (mut ri, mut wi) = tokio::io::split(upgraded);
    let (mut rs, mut ws) = server.split();

    let client_to_server = tokio::spawn(async move {
        tokio::io::copy(&mut ri, &mut ws).await
    });
    let server_to_client = tokio::spawn(async move {
        tokio::io::copy(&mut rs, &mut wi).await
    });

    let _ = tokio::try_join!(client_to_server, server_to_client);
    // try to shutdown both sides
    let _ = wi.shutdown().await;
    let _ = ws.shutdown().await;
    Ok(())
}
