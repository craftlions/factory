//! The guest's only way out: an HTTP proxy that reaches a fixed list of hosts
//! and, when the session allows it, the public internet. The guest has no
//! network device, so nothing can go around it.

use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

const MAX_HEAD: usize = 8 * 1024;

/// Host names a guest may open TLS connections to. An entry starting with a
/// dot also matches every subdomain.
#[derive(Debug, Default)]
pub struct AllowList(Vec<String>);

impl AllowList {
    pub fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        Self(
            hosts
                .into_iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
        )
    }

    pub fn allows(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.0.iter().any(|entry| match entry.strip_prefix('.') {
            Some(domain) => host == domain || host.ends_with(entry.as_str()),
            None => host == *entry,
        })
    }
}

/// Where a guest may go beyond its allow-list.
#[derive(Clone, Debug, Default)]
pub enum Egress {
    /// Nowhere: only allow-listed hosts, over TLS on port 443.
    #[default]
    Restricted,
    /// Any public host on these ports, or on every port when `None`.
    Public { ports: Option<Vec<u16>> },
}

impl Egress {
    fn permits(&self, port: u16) -> bool {
        match self {
            Egress::Restricted => false,
            Egress::Public { ports: None } => true,
            Egress::Public { ports: Some(ports) } => ports.contains(&port),
        }
    }
}

/// What one guest's proxy lets through. Private addresses never are.
#[derive(Debug)]
pub struct Policy {
    pub allow: AllowList,
    pub egress: Egress,
}

impl Policy {
    fn permits(&self, host: &str, port: u16) -> bool {
        (port == 443 && self.allow.allows(host)) || self.egress.permits(port)
    }
}

/// The host part of a URL such as a provider's API base.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// An allowed name must not lead into the host's own networks.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some())
        }
    }
}

/// Where the guest wants to go, from the head of its proxy request.
enum Target {
    /// `CONNECT host:port`: a tunnel, usually for TLS.
    Tunnel { host: String, port: u16 },
    /// A plain HTTP request in absolute form, `GET http://host/path`. `head`
    /// is the request rewritten for the origin server.
    Forward {
        host: String,
        port: u16,
        head: String,
    },
}

fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse().ok()?),
        _ => (authority, default_port),
    };
    let host = host.trim_matches(['[', ']']);
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

fn parse(head: &str) -> Option<Target> {
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split_whitespace();
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(target, 443)?;
        return Some(Target::Tunnel { host, port });
    }
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let (host, port) = split_authority(authority, 80)?;
    let path = if path.is_empty() { "/" } else { path };
    // One request per connection keeps every request checked on its own.
    let mut rewritten = format!("{method} {path} {version}\r\n");
    for line in lines.filter(|line| !line.is_empty()) {
        let name = line
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "connection" | "proxy-connection" | "proxy-authorization"
        ) {
            rewritten.push_str(line);
            rewritten.push_str("\r\n");
        }
    }
    rewritten.push_str("Connection: close\r\n\r\n");
    Some(Target::Forward {
        host,
        port,
        head: rewritten,
    })
}

async fn refuse(client: &mut (impl AsyncWrite + Unpin), status: &str) -> io::Result<()> {
    let response = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    client.write_all(response.as_bytes()).await
}

/// Serves one proxied connection. Refusals are reported through `notices`,
/// connections let through and how they ended through `trace`.
pub async fn serve(
    mut client: impl AsyncRead + AsyncWrite + Unpin,
    policy: Arc<Policy>,
    notices: mpsc::UnboundedSender<String>,
    trace: mpsc::UnboundedSender<String>,
) -> io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD || client.read(&mut byte).await? == 0 {
            return refuse(&mut client, "400 Bad Request").await;
        }
        head.push(byte[0]);
    }
    let Some(target) = parse(&String::from_utf8_lossy(&head)) else {
        let _ = notices
            .send("Blocked a request from the session that the proxy does not understand.".into());
        return refuse(&mut client, "405 Method Not Allowed").await;
    };
    let (host, port, forward) = match target {
        Target::Tunnel { host, port } => (host, port, None),
        Target::Forward { host, port, head } => (host, port, Some(head)),
    };
    let permitted = match &forward {
        // The allow-list is for TLS; plain HTTP needs public egress.
        None => policy.permits(&host, port),
        Some(_) => policy.egress.permits(port),
    };
    if !permitted {
        let _ = notices.send(format!(
            "Blocked a connection from the session to {host}:{port}."
        ));
        return refuse(&mut client, "403 Forbidden").await;
    }

    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .filter(|address| is_public(address.ip()))
        .collect();
    if addresses.is_empty() {
        let _ = notices.send(format!("Blocked {host}: it resolves to a private address."));
        return refuse(&mut client, "403 Forbidden").await;
    }
    let mut upstream = match TcpStream::connect(addresses.as_slice()).await {
        Ok(upstream) => upstream,
        Err(error) => {
            let _ = notices.send(format!("Could not reach {host}: {error}."));
            return refuse(&mut client, "502 Bad Gateway").await;
        }
    };
    let _ = trace.send(format!("connected to {host}:{port}"));
    match forward {
        Some(head) => upstream.write_all(head.as_bytes()).await?,
        None => {
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?
        }
    }
    let ended = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    let _ = trace.send(match &ended {
        Ok((sent, received)) => {
            format!("closed {host}:{port} after {sent} bytes out, {received} in")
        }
        Err(error) => format!("closed {host}:{port}: {error}"),
    });
    ended.map(|_| ())
}
