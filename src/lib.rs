//!
//! A simple reverse proxy, to be used with [Hyper].
//!
//! The implementation ensures that [Hop-by-hop headers] are stripped correctly in both directions,
//! and adds the client's IP address to a comma-space-separated list of forwarding addresses in the
//! `X-Forwarded-For` header.
//!
//! The implementation is based on Go's [`httputil.ReverseProxy`].
//!
//! [Hyper]: http://hyper.rs/
//! [Hop-by-hop headers]: http://www.w3.org/Protocols/rfc2616/rfc2616-sec13.html
//! [`httputil.ReverseProxy`]: https://golang.org/pkg/net/http/httputil/#ReverseProxy
//!
//! # Example
//!
//! Add these dependencies to your `Cargo.toml` file.
//!
//! ```toml
//! [dependencies]
//! hyper-reverse-proxy = "0.5"
//! hyper = { version = "0.14", features = ["full"] }
//! tokio = { version = "1", features = ["full"] }
//! ```
//!
//! To enable support for connecting to downstream HTTPS servers, enable the `https` feature:
//!
//! ```toml
//! hyper-reverse-proxy = { version = "0.4", features = ["https"] }
//! ```
//!
//! The following example will set up a reverse proxy listening on `127.0.0.1:13900`,
//! and will proxy these calls:
//!
//! * `"/target/first"` will be proxied to `http://127.0.0.1:13901`
//!
//! * `"/target/second"` will be proxied to `http://127.0.0.1:13902`
//!
//! * All other URLs will be handled by `debug_request` function, that will display request information.
//!
//! ```rust,no_run
//! use hyper::server::conn::AddrStream;
//! use hyper::service::{make_service_fn, service_fn};
//! use hyper::{Body, Request, Response, Server, StatusCode};
//! use hyper_reverse_proxy::ReverseProxy;
//! use hyper_trust_dns::{RustlsHttpsConnector, TrustDnsResolver};
//! use std::net::IpAddr;
//! use std::{convert::Infallible, net::SocketAddr};
//!
//! lazy_static::lazy_static! {
//!     static ref  PROXY_CLIENT: ReverseProxy<RustlsHttpsConnector> = {
//!         ReverseProxy::new(
//!             hyper::Client::builder().build::<_, hyper::Body>(TrustDnsResolver::default().into_rustls_webpki_https_connector()),
//!         )
//!     };
//! }
//!
//! fn debug_request(req: &Request<Body>) -> Result<Response<Body>, Infallible> {
//!     let body_str = format!("{:?}", req);
//!     Ok(Response::new(Body::from(body_str)))
//! }
//!
//! async fn handle(client_ip: IpAddr, req: Request<Body>) -> Result<Response<Body>, Infallible> {
//!     if req.uri().path().starts_with("/target/first") {
//!         match PROXY_CLIENT.call(client_ip, "http://127.0.0.1:13901", req)
//!             .await
//!         {
//!             Ok(response) => {
//!                 Ok(response)
//!             },
//!             Err(_error) => {
//!                 Ok(Response::builder()
//!                 .status(StatusCode::INTERNAL_SERVER_ERROR)
//!                 .body(Body::empty())
//!                 .unwrap())},
//!         }
//!     } else if req.uri().path().starts_with("/target/second") {
//!         match PROXY_CLIENT.call(client_ip, "http://127.0.0.1:13902", req)
//!             .await
//!         {
//!             Ok(response) => Ok(response),
//!             Err(_error) => Ok(Response::builder()
//!                 .status(StatusCode::INTERNAL_SERVER_ERROR)
//!                 .body(Body::empty())
//!                 .unwrap()),
//!         }
//!     } else {
//!         debug_request(&req)
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let bind_addr = "127.0.0.1:8000";
//!     let addr: SocketAddr = bind_addr.parse().expect("Could not parse ip:port.");
//!
//!     let make_svc = make_service_fn(|conn: &AddrStream| {
//!         let remote_addr = conn.remote_addr().ip();
//!         async move { Ok::<_, Infallible>(service_fn(move |req| handle(remote_addr, req))) }
//!     });
//!
//!     let server = Server::bind(&addr).serve(make_svc);
//!
//!     println!("Running server on {:?}", addr);
//!
//!     if let Err(e) = server.await {
//!         eprintln!("server error: {}", e);
//!     }
//! }
//!
//! ```
#[macro_use]
extern crate tracing;

use hyper::header::{HeaderMap, HeaderName, HeaderValue, HOST};
use hyper::http::header::{InvalidHeaderValue, ToStrError};
use hyper::http::uri::InvalidUri;
use hyper::upgrade::OnUpgrade;
use hyper::{Body, Client, Error, Request, Response, StatusCode};
use lazy_static::lazy_static;
use std::net::IpAddr;
use tokio::io::copy_bidirectional;

lazy_static! {
    static ref TE_HEADER: HeaderName = HeaderName::from_static("te");
    static ref CONNECTION_HEADER: HeaderName = HeaderName::from_static("connection");
    static ref UPGRADE_HEADER: HeaderName = HeaderName::from_static("upgrade");
    static ref TRAILER_HEADER: HeaderName = HeaderName::from_static("trailer");
    static ref TRAILERS_HEADER: HeaderName = HeaderName::from_static("trailers");
    // A list of the headers, using hypers actual HeaderName comparison
    static ref HOP_HEADERS: [HeaderName; 9] = [
        CONNECTION_HEADER.clone(),
        TE_HEADER.clone(),
        TRAILER_HEADER.clone(),
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
        HeaderName::from_static("proxy-authenticate"),
        HeaderName::from_static("proxy-authorization"),
        HeaderName::from_static("transfer-encoding"),
        HeaderName::from_static("upgrade"),
    ];

    static ref X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
}

#[derive(Debug)]
pub enum ProxyError {
    InvalidUri(InvalidUri),
    HyperError(Error),
    ForwardHeaderError,
    UpgradeError(String),
}

impl From<Error> for ProxyError {
    fn from(err: Error) -> ProxyError {
        ProxyError::HyperError(err)
    }
}

impl From<InvalidUri> for ProxyError {
    fn from(err: InvalidUri) -> ProxyError {
        ProxyError::InvalidUri(err)
    }
}

impl From<ToStrError> for ProxyError {
    fn from(_err: ToStrError) -> ProxyError {
        ProxyError::ForwardHeaderError
    }
}

impl From<InvalidHeaderValue> for ProxyError {
    fn from(_err: InvalidHeaderValue) -> ProxyError {
        ProxyError::ForwardHeaderError
    }
}

fn remove_hop_headers(headers: &mut HeaderMap) {
    debug!("Removing hop headers");

    for header in &*HOP_HEADERS {
        headers.remove(header);
    }
}

fn get_upgrade_type(headers: &HeaderMap) -> Option<String> {
    #[allow(clippy::blocks_in_if_conditions)]
    if headers
        .get(&*CONNECTION_HEADER)
        .map(|value| {
            value
                .to_str()
                .unwrap()
                .split(',')
                .any(|e| e.trim() == *UPGRADE_HEADER)
        })
        .unwrap_or(false)
    {
        if let Some(upgrade_value) = headers.get(&*UPGRADE_HEADER) {
            debug!(
                "Found upgrade header with value: {}",
                upgrade_value.to_str().unwrap().to_owned()
            );

            return Some(upgrade_value.to_str().unwrap().to_owned());
        }
    }

    None
}

fn remove_connection_headers(headers: &mut HeaderMap) {
    if headers.get(&*CONNECTION_HEADER).is_some() {
        debug!("Removing connection headers");

        let value = headers.get(&*CONNECTION_HEADER).cloned().unwrap();

        for name in value.to_str().unwrap().split(',') {
            if !name.trim().is_empty() {
                headers.remove(name.trim());
            }
        }
    }
}

fn create_proxied_response<B>(mut response: Response<B>) -> Response<B> {
    info!("Creating proxied response");

    remove_hop_headers(response.headers_mut());
    remove_connection_headers(response.headers_mut());

    response
}

fn forward_uri<B>(forward_url: &str, req: &Request<B>) -> String {
    debug!("Building forward uri");

    let split_url = forward_url.split('?').collect::<Vec<&str>>();

    let mut base_url: &str = split_url.get(0).unwrap_or(&"");
    let forward_url_query: &str = split_url.get(1).unwrap_or(&"");

    let path2 = req.uri().path();

    if base_url.ends_with('/') {
        let mut path1_chars = base_url.chars();
        path1_chars.next_back();

        base_url = path1_chars.as_str();
    }

    let total_length = base_url.len()
        + path2.len()
        + 1
        + forward_url_query.len()
        + req.uri().query().map(|e| e.len()).unwrap_or(0);

    debug!("Creating url with capacity to {}", total_length);

    let mut url = String::with_capacity(total_length);

    url.push_str(base_url);
    url.push_str(path2);

    if !forward_url_query.is_empty() || req.uri().query().map(|e| !e.is_empty()).unwrap_or(false) {
        debug!("Adding query parts to url");
        url.push('?');
        url.push_str(forward_url_query);

        if forward_url_query.is_empty() {
            debug!("Using request query");

            url.push_str(req.uri().query().unwrap_or(""));
        } else {
            debug!("Merging request and forward_url query");

            let request_query_items = req.uri().query().unwrap_or("").split('&').map(|el| {
                let parts = el.split('=').collect::<Vec<&str>>();
                (parts[0], if parts.len() > 1 { parts[1] } else { "" })
            });

            let forward_query_items = forward_url_query
                .split('&')
                .map(|el| {
                    let parts = el.split('=').collect::<Vec<&str>>();
                    parts[0]
                })
                .collect::<Vec<_>>();

            for (key, value) in request_query_items {
                if !forward_query_items.iter().any(|e| e == &key) {
                    url.push('&');
                    url.push_str(key);
                    url.push('=');
                    url.push_str(value);
                }
            }

            if url.ends_with('&') {
                let mut parts = url.chars();
                parts.next_back();

                url = parts.as_str().to_string();
            }
        }
    }

    debug!("Built forwarding url from request: {}", url);

    url.parse().unwrap()
}

fn create_proxied_request<B>(
    client_ip: IpAddr,
    forward_url: &str,
    mut request: Request<B>,
    upgrade_type: Option<&String>,
) -> Result<Request<B>, ProxyError> {
    info!("Creating proxied request");

    let contains_te_trailers_value = request
        .headers()
        .get(&*TE_HEADER)
        .map(|value| {
            value
                .to_str()
                .unwrap()
                .split(',')
                .any(|e| e.trim() == *TRAILERS_HEADER)
        })
        .unwrap_or(false);

    let uri: hyper::Uri = forward_uri(forward_url, &request).parse()?;

    debug!("Setting headers of proxied request");

    // remove the original HOST header. It will be set by the client that sends the request: https://github.com/hyperium/hyper/blob/4fcfe1f4ba461209483dec960e36293459a1c60a/src/client/client.rs#L250
    let original_host = request.headers_mut().remove(HOST);

    *request.uri_mut() = uri;

    remove_hop_headers(request.headers_mut());
    remove_connection_headers(request.headers_mut());

    if contains_te_trailers_value {
        debug!("Setting up trailer headers");

        request
            .headers_mut()
            .insert(&*TE_HEADER, HeaderValue::from_static("trailers"));
    }

    if let Some(value) = upgrade_type {
        debug!("Repopulate upgrade headers");

        request
            .headers_mut()
            .insert(&*UPGRADE_HEADER, value.parse().unwrap());
        request
            .headers_mut()
            .insert(&*CONNECTION_HEADER, HeaderValue::from_static("UPGRADE"));
    }

    // Add forwarding information in the headers
    match request.headers_mut().entry(&*X_FORWARDED_FOR) {
        hyper::header::Entry::Vacant(entry) => {
            debug!("X-Fowraded-for header was vacant");
            entry.insert(client_ip.to_string().parse()?);
        }

        hyper::header::Entry::Occupied(entry) => {
            debug!("X-Fowraded-for header was occupied");
            let client_ip_str = client_ip.to_string();
            let mut addr =
                String::with_capacity(entry.get().as_bytes().len() + 2 + client_ip_str.len());

            addr.push_str(std::str::from_utf8(entry.get().as_bytes()).unwrap());
            addr.push(',');
            addr.push(' ');
            addr.push_str(&client_ip_str);
        }
    }

    debug!("Created proxied request");

    Ok(request)
}

pub async fn call<'a, T: hyper::client::connect::Connect + Clone + Send + Sync + 'static>(
    client_ip: IpAddr,
    forward_uri: &str,
    mut request: Request<Body>,
    client: &'a Client<T>,
) -> Result<Response<Body>, ProxyError> {
    info!(
        "Received proxy call from {} to {}, client: {}",
        request.uri().to_string(),
        forward_uri,
        client_ip
    );

    let request_upgrade_type = get_upgrade_type(request.headers());
    let request_upgraded = request.extensions_mut().remove::<OnUpgrade>();

    let proxied_request = create_proxied_request(
        client_ip,
        forward_uri,
        request,
        request_upgrade_type.as_ref(),
    )?;
    let mut response = client.request(proxied_request).await?;

    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let response_upgrade_type = get_upgrade_type(response.headers());

        if request_upgrade_type == response_upgrade_type {
            if let Some(request_upgraded) = request_upgraded {
                let mut response_upgraded = response
                    .extensions_mut()
                    .remove::<OnUpgrade>()
                    .expect("response does not have an upgrade extension")
                    .await?;

                debug!("Responding to a connection upgrade response");

                tokio::spawn(async move {
                    let mut request_upgraded =
                        request_upgraded.await.expect("failed to upgrade request");

                    copy_bidirectional(&mut response_upgraded, &mut request_upgraded)
                        .await
                        .expect("coping between upgraded connections failed");
                });

                Ok(response)
            } else {
                Err(ProxyError::UpgradeError(
                    "request does not have an upgrade extension".to_string(),
                ))
            }
        } else {
            Err(ProxyError::UpgradeError(format!(
                "backend tried to switch to protocol {:?} when {:?} was requested",
                response_upgrade_type, request_upgrade_type
            )))
        }
    } else {
        let proxied_response = create_proxied_response(response);

        debug!("Responding to call with response");
        Ok(proxied_response)
    }
}

pub struct ReverseProxy<T: hyper::client::connect::Connect + Clone + Send + Sync + 'static> {
    client: Client<T>,
}

impl<T: hyper::client::connect::Connect + Clone + Send + Sync + 'static> ReverseProxy<T> {
    pub fn new(client: Client<T>) -> Self {
        Self { client }
    }

    pub async fn call(
        &self,
        client_ip: IpAddr,
        forward_uri: &str,
        request: Request<Body>,
    ) -> Result<Response<Body>, ProxyError> {
        call::<T>(client_ip, forward_uri, request, &self.client).await
    }
}

#[cfg(feature = "__bench")]
pub mod benches {
    pub fn hop_headers() -> &'static [crate::HeaderName] {
        &*super::HOP_HEADERS
    }

    pub fn create_proxied_response<T>(response: crate::Response<T>) {
        super::create_proxied_response(response);
    }

    pub fn forward_uri<B>(forward_url: &str, req: &crate::Request<B>) {
        super::forward_uri(forward_url, req);
    }

    pub fn create_proxied_request<B>(
        client_ip: crate::IpAddr,
        forward_url: &str,
        request: crate::Request<B>,
        upgrade_type: Option<&String>,
    ) {
        super::create_proxied_request(client_ip, forward_url, request, upgrade_type).unwrap();
    }
}
