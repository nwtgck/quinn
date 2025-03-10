#![allow(clippy::mutex_atomic, clippy::eval_order_dependence)]
#![type_length_limit = "2121396"]

use std::{
    env,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use futures::{future, StreamExt};
use hyper::body::HttpBody as _;
use lazy_static::lazy_static;
use quinn_h3::Settings;
use structopt::StructOpt;
use tracing::{debug, error, info, info_span, warn};
use tracing_futures::Instrument as _;

use quinn_h3::Body;

#[derive(StructOpt, Debug)]
#[structopt(name = "interop")]
struct Opt {
    host: Option<String>,
    #[structopt(short, long)]
    name: Option<String>,
    #[structopt(long)]
    port: Option<u16>,
    #[structopt(long)]
    retry_port: Option<u16>,
    #[structopt(long)]
    h3: bool,
    #[structopt(long)]
    hq: bool,
    #[structopt(long)]
    seq: bool,
    #[structopt(long)]
    siduck: bool,
    /// Enable key logging
    #[structopt(long = "keylog")]
    keylog: bool,
}

impl Opt {
    fn alpn(&self) -> Option<Alpn> {
        match (self.h3, self.hq, self.siduck) {
            (false, false, false) => None,
            (true, false, false) => Some(Alpn::H3),
            (false, true, false) => Some(Alpn::Hq),
            (false, false, true) => Some(Alpn::SiDuck),
            _ => panic!("conflicting protocol options"),
        }
    }
}

#[derive(Clone)]
enum Alpn {
    Hq,
    H3,
    HqH3,
    SiDuck,
}

impl From<&Alpn> for Vec<Vec<u8>> {
    fn from(alpn: &Alpn) -> Vec<Vec<u8>> {
        match alpn {
            Alpn::H3 => vec![quinn_h3::ALPN.into()],
            Alpn::Hq => vec![b"hq-29"[..].into()],
            Alpn::SiDuck => vec![b"siduck-00"[..].into()],
            Alpn::HqH3 => vec![b"hq-29"[..].into(), quinn_h3::ALPN.into()],
        }
    }
}

#[derive(Clone)]
struct Peer {
    name: String,
    host: String,
    port: u16,
    retry_port: u16,
    alpn: Alpn,
    sequential: bool,
}

impl Peer {
    fn new<T: Into<String>>(host: T) -> Self {
        let host_str = host.into();
        Self {
            name: host_str.clone(),
            host: host_str,
            port: 4433,
            retry_port: 4434,
            alpn: Alpn::HqH3,
            sequential: false,
        }
    }

    fn name<T: Into<String>>(mut self, name: T) -> Self {
        self.name = name.into();
        self
    }

    fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    fn retry_port(mut self, port: u16) -> Self {
        self.retry_port = port;
        self
    }

    fn h3(mut self) -> Self {
        self.alpn = Alpn::H3;
        self
    }

    fn hq(mut self) -> Self {
        self.alpn = Alpn::Hq;
        self
    }

    fn uri(&self, path: &str) -> http::Uri {
        http::Uri::builder()
            .scheme("https")
            .authority(self.host.as_str())
            .path_and_query(path)
            .build()
            .expect("invalid uri")
    }
}

lazy_static! {
    static ref PEERS: Vec<Peer> = vec![
        Peer::new("quant.eggert.org").name("quant").hq(),
        Peer::new("nghttp2.org").name("nghttp2").h3(),
        Peer::new("fb.mvfst.net").name("mvfst").h3(),
        Peer::new("test.privateoctopus.com").name("picoquic").h3(),
        Peer::new("quic.westus.cloudapp.azure.com")
            .name("msquic")
            .h3()
            .port(443),
        Peer::new("quic.westus.cloudapp.azure.com")
            .name("msquic-hq")
            .hq(),
        Peer::new("f5quic.com").name("f5"),
        Peer::new("quic.ogre.com").name("ATS"),
        Peer::new("quic.tech").name("quiche-http/0.9").hq(),
        Peer::new("quic.tech")
            .name("quiche")
            .h3()
            .port(8443)
            .retry_port(8444),
        Peer::new("http3-test.litespeedtech.com")
            .name("lsquic")
            .h3(),
        Peer::new("cloudflare-quic.com")
            .name("ngx_quic")
            .h3()
            .port(443),
        Peer::new("quic.aiortc.org").name("aioquic").h3().port(443),
        Peer::new("quic.rocks").name("gQuic"),
        Peer::new("mew.org").name("Haskell QUIC").h3(),
        Peer::new("quic.examp1e.net").name("quicly").hq(),
    ];
}

#[tokio::main]
async fn main() {
    let opt = Opt::from_args();
    let mut code = 0;
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let peers = if let Some(ref host) = opt.host {
        vec![Peer {
            name: host.clone(),
            host: host.clone(),
            port: opt.port.unwrap_or(4433),
            retry_port: opt.retry_port.unwrap_or(4434),
            sequential: opt.seq,
            alpn: opt.alpn().unwrap_or(Alpn::HqH3),
        }]
    } else if opt.name.is_some() {
        let name = opt.name.as_ref().unwrap();
        let mut peers: Vec<Peer> = PEERS[..]
            .iter()
            .filter(|p| &p.name == name)
            .cloned()
            .collect();

        peers.iter_mut().for_each(|mut x| {
            if let Some(alpn) = opt.alpn() {
                x.alpn = alpn;
            }
            if let Some(port) = opt.port {
                x.port = port;
            }
            if let Some(retry_port) = opt.retry_port {
                x.retry_port = retry_port;
            }
            x.sequential = opt.seq;
        });
        peers
    } else {
        Vec::from(&PEERS[..])
    };

    let keylog = opt.keylog;
    let results = future::join_all(peers.into_iter().map(|peer| async move {
        let span = info_span!("peer", name = peer.name.as_str());
        let result = run(&peer, keylog).instrument(span).await;
        (peer, result)
    }));
    for (peer, result) in results.await {
        match result {
            Ok(r) => println!("{}: {}", peer.name, r),
            Err(e) => {
                println!("ERROR: {}: {}", peer.name, e);
                code = 1;
            }
        }
    }
    ::std::process::exit(code);
}

async fn run(peer: &Peer, keylog: bool) -> Result<String> {
    let state = State::try_new(&peer, keylog)?;
    let result = match peer.alpn {
        Alpn::Hq => state.run_hq().instrument(info_span!("hq")).await?.format(),
        Alpn::SiDuck => state.run_siduck().instrument(info_span!("siduck")).await?,
        Alpn::H3 | Alpn::HqH3 => state.run_h3().instrument(info_span!("h3")).await?.format(),
    };
    Ok(result)
}

struct State {
    endpoint: quinn::Endpoint,
    client_config: quinn::ClientConfig,
    remote: SocketAddr,
    host: String,
    peer: Peer,
    h3_client: Option<quinn_h3::client::Client>,
}

impl State {
    async fn run_hq(self) -> Result<InteropResult> {
        // We run core on its own first to ensure the 0-RTT token can be used reliably
        let core = self.core_hq().instrument(info_span!("core")).await;
        if self.peer.sequential {
            Ok(build_result(
                core,
                self.key_update_hq()
                    .instrument(info_span!("key_update"))
                    .await,
                self.rebind_hq().instrument(info_span!("rebind")).await,
                self.retry_hq().instrument(info_span!("retry")).await,
                self.throughput_hq()
                    .instrument(info_span!("throughput"))
                    .await,
                None,
                None,
            ))
        } else {
            let (key_update, rebind, retry, throughput) = tokio::join!(
                self.key_update_hq().instrument(info_span!("key_update")),
                self.rebind_hq().instrument(info_span!("rebind")),
                self.retry_hq().instrument(info_span!("retry")),
                self.throughput_hq().instrument(info_span!("throughput"))
            );
            Ok(build_result(
                core, key_update, rebind, retry, throughput, None, None,
            ))
        }
    }

    async fn run_h3(self) -> Result<InteropResult> {
        // We run core on its own first to ensure the 0-RTT token can be used reliably
        let core = self.core_h3().instrument(info_span!("core")).await;
        if self.peer.sequential {
            Ok(build_result(
                core,
                self.key_update_h3()
                    .instrument(info_span!("key_update"))
                    .await,
                self.rebind_h3().instrument(info_span!("rebind")).await,
                self.retry_h3().instrument(info_span!("retry")).await,
                self.throughput_h3()
                    .instrument(info_span!("throughput"))
                    .await,
                Some(self.h3().instrument(info_span!("h3")).await),
                Some(
                    self.dynamic_encoding()
                        .instrument(info_span!("dynamic_encoding"))
                        .await,
                ),
            ))
        } else {
            let (key_update, rebind, retry, throughput, h3, dynamic_encoding) = tokio::join!(
                self.key_update_h3().instrument(info_span!("key_update")),
                self.rebind_h3().instrument(info_span!("rebind")),
                self.retry_h3().instrument(info_span!("retry")),
                self.throughput_h3().instrument(info_span!("throughput")),
                self.h3().instrument(info_span!("h3")),
                self.dynamic_encoding()
                    .instrument(info_span!("dynamic_encoding")),
            );
            Ok(build_result(
                core,
                key_update,
                rebind,
                retry,
                throughput,
                Some(h3),
                Some(dynamic_encoding),
            ))
        }
    }

    async fn run_siduck(self) -> Result<String> {
        let mut new_conn = self
            .endpoint
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("failed to connect: {}", e))?;
        for _ in 0..3 {
            new_conn
                .connection
                .send_datagram(b"quack"[..].into())
                .context("datagram send failed")?;
        }
        match new_conn.datagrams.next().await.unwrap() {
            Ok(bytes) if &bytes[..] == b"quack-ack" => {}
            Ok(_) => {
                bail!("incorrect response");
            }
            Err(e) => {
                bail!("unexpected error: {}", e);
            }
        }
        new_conn.connection.close(0u32.into(), [][..].into());
        self.endpoint.wait_idle().await;
        Ok("🦆".into())
    }

    fn try_new(peer: &Peer, keylog: bool) -> Result<Self> {
        let remote = format!("{}:{}", peer.host, peer.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;
        let host = if webpki::DNSNameRef::try_from_ascii_str(&peer.host).is_ok() {
            &peer.host
        } else {
            warn!("invalid hostname, using \"example.com\"");
            "example.com"
        };

        let mut tls_config = rustls::ClientConfig::new();
        tls_config.versions = vec![rustls::ProtocolVersion::TLSv1_3];
        tls_config.enable_early_data = true;
        tls_config
            .dangerous()
            .set_certificate_verifier(Arc::new(InteropVerifier(Arc::new(Mutex::new(false)))));
        tls_config.alpn_protocols = (&peer.alpn).into();
        if keylog {
            tls_config.key_log = Arc::new(rustls::KeyLogFile::new());
        }
        let mut transport = quinn::TransportConfig::default();
        transport.send_window(1024 * 1024 * 2);
        transport.receive_window(1024 * 1024 * 2).unwrap();
        transport
            .max_idle_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let client_config = quinn::ClientConfig {
            crypto: Arc::new(tls_config),
            transport: Arc::new(transport),
        };

        let mut endpoint = quinn::Endpoint::builder();
        endpoint.default_client_config(client_config.clone());

        let (endpoint, _) = endpoint.bind(&"[::]:0".parse().unwrap())?;

        let h3_client = match peer.alpn {
            Alpn::Hq => None,
            _ => {
                let mut h3_client = quinn_h3::client::Builder::default();
                h3_client.settings(Settings::new());
                Some(h3_client.endpoint(endpoint.clone()))
            }
        };
        Ok(State {
            h3_client,
            host: host.into(),
            peer: peer.clone(),
            client_config,
            endpoint,
            remote,
        })
    }

    async fn core_hq(&self) -> Result<InteropResult> {
        let mut result = InteropResult::default();
        let new_conn = self
            .endpoint
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("failed to connect: {}", e))?;
        result.handshake = true;
        let stream = new_conn
            .connection
            .open_bi()
            .await
            .map_err(|e| anyhow!("failed to open stream: {}", e))?;
        hq_get(stream, "/")
            .await
            .map_err(|e| anyhow!("simple request failed: {}", e))?;
        result.stream_data = true;
        new_conn.connection.close(0u32.into(), b"done");

        let saw_cert = Arc::new(Mutex::new(false));
        let quinn::ClientConfig {
            mut crypto,
            transport,
        } = self.client_config.clone();
        Arc::make_mut(&mut crypto)
            .dangerous()
            .set_certificate_verifier(Arc::new(InteropVerifier(saw_cert.clone())));

        let client_config = quinn::ClientConfig { transport, crypto };

        let conn = match self
            .endpoint
            .connect_with(client_config, &self.remote, &self.host)?
            .into_0rtt()
        {
            Ok((new_conn, _)) => {
                let stream = new_conn
                    .connection
                    .open_bi()
                    .await
                    .map_err(|e| anyhow!("failed to open 0-RTT stream: {}", e))?;
                hq_get(stream, "/")
                    .await
                    .map_err(|e| anyhow!("0-RTT request failed: {}", e))?;
                result.zero_rtt = true;
                new_conn.connection
            }
            Err(conn) => {
                info!("0-RTT unsupported");
                let new_conn = conn
                    .await
                    .map_err(|e| anyhow!("failed to connect: {}", e))?;
                new_conn.connection
            }
        };
        result.resumption = !*saw_cert.lock().unwrap();
        conn.close(0u32.into(), b"done");

        self.endpoint.wait_idle().await;

        result.close = true;

        Ok(result)
    }

    async fn key_update_hq(&self) -> Result<()> {
        let new_conn = self
            .endpoint
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("failed to connect: {}", e))?;
        let conn = new_conn.connection;
        // Make sure some traffic has gone both ways before the key update
        let stream = conn
            .open_bi()
            .await
            .map_err(|e| anyhow!("failed to open stream: {}", e))?;
        hq_get(stream, "/").await?;
        conn.force_key_update();
        let stream = conn
            .open_bi()
            .await
            .map_err(|e| anyhow!("failed to open stream: {}", e))?;
        hq_get(stream, "/").await?;
        conn.close(0u32.into(), b"done");
        Ok(())
    }

    async fn retry_hq(&self) -> Result<()> {
        let mut remote = self.remote;
        remote.set_port(self.peer.retry_port);

        let new_conn = self
            .endpoint
            .connect(&remote, &self.host)?
            .await
            .map_err(|e| anyhow!("failed to connect: {}", e))?;
        let stream = new_conn
            .connection
            .open_bi()
            .await
            .map_err(|e| anyhow!("failed to open stream: {}", e))?;
        hq_get(stream, "/").await?;
        new_conn.connection.close(0u32.into(), b"done");
        Ok(())
    }

    async fn rebind_hq(&self) -> Result<()> {
        let mut endpoint = quinn::Endpoint::builder();
        endpoint.default_client_config(self.client_config.clone());
        let (endpoint, _) = endpoint.bind(&"[::]:0".parse().unwrap())?;

        let new_conn = endpoint
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("failed to connect: {}", e))?;
        let socket = std::net::UdpSocket::bind("[::]:0").unwrap();
        endpoint.rebind(socket)?;
        let stream = new_conn
            .connection
            .open_bi()
            .await
            .map_err(|e| anyhow!("failed to open stream: {}", e))?;
        hq_get(stream, "/").await?;
        new_conn.connection.close(0u32.into(), b"done");
        Ok(())
    }

    async fn throughput_hq(&self) -> Result<()> {
        for size in [5_000_000, 10_000_000usize].iter() {
            let uri = self.peer.uri(&format!("/{}", size));
            let conn = self
                .endpoint
                .connect(&self.remote, &self.host)?
                .await
                .map_err(|e| anyhow!("failed to connect: {}", e))?
                .connection;

            let start = Instant::now();
            let stream = conn
                .open_bi()
                .await
                .map_err(|e| anyhow!("failed to open stream: {}", e))?;
            let body = hq_get(stream, &format!("/{}", size)).await?;
            if body.len() != *size {
                return Err(anyhow!("hq {} responded {} B", uri, body.len()));
            }
            let elapsed_hq = start.elapsed();
            conn.close(0x100u16.into(), b"NO_ERROR");

            let mut h2 = hyper::client::Builder::default();
            h2.http2_only(true);
            let mut http = hyper::client::connect::HttpConnector::new();
            http.enforce_http(false);
            let client = h2.build::<_, hyper::Body>(hyper_rustls::HttpsConnector::from((
                http,
                self.client_config.crypto.clone(),
            )));
            let response = client.get(self.peer.uri("/")).await?;
            let _ = hyper::body::to_bytes(response).await?;

            let start = Instant::now();
            let mut response = client.get(uri.clone()).await?;
            let mut total_len = 0usize;
            while let Some(b) = response.body_mut().data().await {
                total_len += b?.len()
            }
            if total_len != *size {
                return Err(anyhow!("h2 {} responded {} B", uri, total_len));
            }
            let elapsed_h2 = start.elapsed();

            let percentage = (elapsed_hq.as_nanos() as f64 - elapsed_h2.as_nanos() as f64)
                / elapsed_h2.as_nanos() as f64
                * 100.0;
            info!(
                size = size,
                h3 = elapsed_hq.as_millis() as usize,
                h2 = elapsed_h2.as_millis() as usize,
                "hq time {:+.2}% of h2's",
                percentage
            );
            if percentage > 10.0 {
                return Err(anyhow!("Throughput {} is {:+.2}% slower", size, percentage));
            }
        }
        Ok(())
    }

    async fn h3(&self) -> Result<()> {
        if let Alpn::Hq = self.peer.alpn {
            return Err(anyhow!("H3 not implemented on this peer"));
        }
        let conn = self
            .h3_client
            .as_ref()
            .unwrap()
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("h3 failed to connect: {}", e))?;

        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("h3 request failed: {}", e))?;
        conn.close();
        Ok(())
    }

    async fn throughput_h3(&self) -> Result<()> {
        if let Alpn::Hq = self.peer.alpn {
            return Err(anyhow!("H3 not implemented on this peer"));
        }

        for size in [5_000_000, 10_000_000usize].iter() {
            let uri = self.peer.uri(&format!("/{}", size));
            let conn = self
                .h3_client
                .as_ref()
                .unwrap()
                .connect(&self.remote, &self.host)?
                .await
                .map_err(|e| anyhow!("h3 failed to connect: {}", e))?;

            let start = Instant::now();
            h3_get(&conn, &uri).await.context("h3 request failed")?;
            let elapsed_h3 = start.elapsed();
            conn.close();

            let mut h2 = hyper::client::Builder::default();
            h2.http2_only(true);
            let mut http = hyper::client::connect::HttpConnector::new();
            http.enforce_http(false);
            let client = h2.build::<_, hyper::Body>(hyper_rustls::HttpsConnector::from((
                http,
                self.client_config.crypto.clone(),
            )));
            let response = client.get(self.peer.uri("/")).await?;
            let _ = hyper::body::to_bytes(response).await?;

            let start = Instant::now();
            let mut response = client.get(uri.clone()).await?;
            let mut total_len = 0usize;
            while let Some(b) = response.body_mut().data().await {
                total_len += b?.len()
            }
            if total_len != *size {
                return Err(anyhow!("h2 {} responded {} B", uri, total_len));
            }
            let elapsed_h2 = start.elapsed();

            let percentage = (elapsed_h3.as_nanos() as f64 - elapsed_h2.as_nanos() as f64)
                / elapsed_h2.as_nanos() as f64
                * 100.0;
            info!(
                size = size,
                h3 = elapsed_h3.as_millis() as usize,
                h2 = elapsed_h2.as_millis() as usize,
                "h3 time {:+.2}% of h2's",
                percentage
            );
            if percentage > 10.0 {
                return Err(anyhow!("Throughput {} is {:+.2}% slower", size, percentage));
            }
        }
        Ok(())
    }

    async fn core_h3(&self) -> Result<InteropResult> {
        let mut result = InteropResult::default();

        let conn = self
            .h3_client
            .as_ref()
            .unwrap()
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("h3 failed to connect: {}", e))?;
        result.handshake = true;
        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("simple request failed: {}", e))?;
        result.stream_data = true;
        conn.close();

        let saw_cert = Arc::new(Mutex::new(false));
        let quinn::ClientConfig {
            mut crypto,
            transport,
        } = self.client_config.clone();
        Arc::make_mut(&mut crypto)
            .dangerous()
            .set_certificate_verifier(Arc::new(InteropVerifier(saw_cert.clone())));
        let client_config = quinn::ClientConfig { transport, crypto };

        let conn = match self
            .h3_client
            .as_ref()
            .unwrap()
            .connect_with(client_config, &self.remote, &self.host)?
            .into_0rtt()
        {
            Ok((conn, _)) => {
                h3_get(&conn, &self.peer.uri("/"))
                    .await
                    .map_err(|e| anyhow!("0-RTT request failed: {}", e))?;
                result.zero_rtt = true;
                conn
            }
            Err(connecting) => {
                info!("0-RTT unsupported");
                connecting
                    .await
                    .map_err(|e| anyhow!("failed to connect: {}", e))?
            }
        };
        result.resumption = !*saw_cert.lock().unwrap();
        conn.close();

        self.endpoint.wait_idle().await;

        result.close = true;

        Ok(result)
    }

    async fn key_update_h3(&self) -> Result<()> {
        let conn = self
            .h3_client
            .as_ref()
            .unwrap()
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("h3 failed to connect: {}", e))?;
        // Make sure some traffic has gone both ways before the key update
        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("request failed before key update: {}", e))?;
        conn.force_key_update();
        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("request failed after key update: {}", e))?;
        conn.close();
        Ok(())
    }

    async fn retry_h3(&self) -> Result<()> {
        let mut remote = self.remote;
        remote.set_port(self.peer.retry_port);

        let conn = self
            .h3_client
            .as_ref()
            .unwrap()
            .connect(&remote, &self.host)?
            .await
            .map_err(|e| anyhow!("h3 failed to connect: {}", e))?;
        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("request failed on retry port: {}", e))?;
        conn.close();
        Ok(())
    }

    async fn rebind_h3(&self) -> Result<()> {
        let (endpoint, _) = quinn::Endpoint::builder().bind(&"[::]:0".parse().unwrap())?;

        let h3_client = quinn_h3::client::Builder::default().endpoint(self.endpoint.clone());
        let conn = h3_client
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("h3 failed to connect: {}", e))?;
        let socket = std::net::UdpSocket::bind("[::]:0").unwrap();
        endpoint.rebind(socket)?;
        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("request failed on retry port: {}", e))?;
        conn.close();
        Ok(())
    }

    async fn dynamic_encoding(&self) -> Result<()> {
        let conn = self
            .h3_client
            .as_ref()
            .unwrap()
            .connect(&self.remote, &self.host)?
            .await
            .map_err(|e| anyhow!("h3 failed to connect: {}", e))?;
        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("request failed before key update: {}", e))?;
        h3_get(&conn, &self.peer.uri("/"))
            .await
            .map_err(|e| anyhow!("request failed before key update: {}", e))?;
        if !conn.had_refs() {
            return Err(anyhow!("No dynamic table entry was referenced"));
        }
        conn.close();
        Ok(())
    }
}

#[derive(Default)]
struct InteropResult {
    handshake: bool,
    stream_data: bool,
    close: bool,
    resumption: bool,
    key_update: bool,
    rebinding: bool,
    zero_rtt: bool,
    retry: bool,
    throughput: bool,
    h3: bool,
    dynamic_encoding: bool,
}

impl InteropResult {
    fn format(&self) -> String {
        let mut string = String::with_capacity(10);
        if self.handshake {
            string.push_str("VH");
        }
        if self.stream_data {
            string.push('D');
        }
        if self.close {
            string.push('C');
        }
        if self.resumption {
            string.push('R');
        }
        if self.zero_rtt {
            string.push('Z');
        }
        if self.retry {
            string.push('S');
        }
        if self.rebinding {
            string.push('B');
        }
        if self.key_update {
            string.push('U');
        }
        if self.throughput {
            string.push('T');
        }
        if self.h3 {
            string.push('3');
        }
        if self.dynamic_encoding {
            string.push('d');
        }
        string
    }
}

#[allow(clippy::cognitive_complexity)]
fn build_result(
    core: Result<InteropResult>,
    key_update: Result<()>,
    rebind: Result<()>,
    retry: Result<()>,
    throughput: Result<()>,
    h3: Option<Result<()>>,
    dynamic_encoding: Option<Result<()>>,
) -> InteropResult {
    let mut result = core.unwrap_or_else(|e| {
        error!("core functionality failed: {}", e);
        InteropResult::default()
    });
    match key_update {
        Ok(_) => result.key_update = true,
        Err(e) => error!("key update failed: {}", e),
    };
    match rebind {
        Ok(_) => result.rebinding = true,
        Err(e) => error!("rebinding failed: {}", e),
    }
    match retry {
        Ok(_) => result.retry = true,
        Err(e) => error!("retry failed: {}", e),
    }
    match throughput {
        Ok(_) => result.throughput = true,
        Err(e) => error!("throughput failed: {}", e),
    }
    match h3 {
        Some(Ok(_)) => result.h3 = true,
        Some(Err(e)) => error!("retry failed: {}", e),
        None => (),
    }
    match dynamic_encoding {
        Some(Ok(_)) => result.dynamic_encoding = true,
        Some(Err(e)) => error!("dynamic encoding failed: {}", e),
        None => (),
    }
    result
}

async fn h3_get(conn: &quinn_h3::client::Connection, uri: &http::Uri) -> Result<usize> {
    let (request, response) = conn.send_request(http::Request::get(uri).body(Body::from(()))?);
    request.await?;

    let mut resp = response.await?;
    debug!("resp: {:?}", resp);
    let body = resp.body_mut().read_to_end().await?;

    Ok(body.len())
}

async fn hq_get(stream: (quinn::SendStream, quinn::RecvStream), path: &str) -> Result<Vec<u8>> {
    let (mut send, recv) = stream;
    send.write_all(&format!("GET {}\r\n", path).as_bytes())
        .await
        .map_err(|e| anyhow!("failed to send request: {}", e))?;
    send.finish()
        .await
        .map_err(|e| anyhow!("failed to shutdown stream: {}", e))?;
    let response = recv
        .read_to_end(usize::max_value())
        .await
        .map_err(|e| anyhow!("failed to read response: {}", e))?;
    Ok(response)
}

struct InteropVerifier(Arc<Mutex<bool>>);
impl rustls::ServerCertVerifier for InteropVerifier {
    fn verify_server_cert(
        &self,
        _roots: &rustls::RootCertStore,
        _presented_certs: &[rustls::Certificate],
        _dns_name: webpki::DNSNameRef,
        _ocsp_response: &[u8],
    ) -> std::result::Result<rustls::ServerCertVerified, rustls::TLSError> {
        *self.0.lock().unwrap() = true;
        Ok(rustls::ServerCertVerified::assertion())
    }
}
