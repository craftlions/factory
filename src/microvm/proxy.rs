//! The guest's only way out: an HTTP CONNECT proxy that reaches a fixed list
//! of hosts. The guest has no network device, so nothing can go around it.

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

fn parse_connect(head: &str) -> Option<(String, u16)> {
    let mut parts = head.lines().next()?.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let (host, port) = parts.next()?.rsplit_once(':')?;
    Some((host.trim_matches(['[', ']']).to_owned(), port.parse().ok()?))
}

async fn refuse(client: &mut (impl AsyncWrite + Unpin), status: &str) -> io::Result<()> {
    let response = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    client.write_all(response.as_bytes()).await
}

/// Serves one proxied connection. Refusals are reported through `notices`.
pub async fn serve(
    mut client: impl AsyncRead + AsyncWrite + Unpin,
    allow: Arc<AllowList>,
    notices: mpsc::UnboundedSender<String>,
) -> io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD || client.read(&mut byte).await? == 0 {
            return refuse(&mut client, "400 Bad Request").await;
        }
        head.push(byte[0]);
    }
    let Some((host, port)) = parse_connect(&String::from_utf8_lossy(&head)) else {
        let _ = notices.send("Blocked a plain HTTP request from the session.".into());
        return refuse(&mut client, "405 Method Not Allowed").await;
    };
    if port != 443 || !allow.allows(&host) {
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
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_matches_exact_hosts_and_dotted_suffixes() {
        let allow = AllowList::new([
            "OpenRouter.ai".to_owned(),
            ".githubusercontent.com".to_owned(),
        ]);
        assert!(allow.allows("openrouter.ai"));
        assert!(!allow.allows("api.openrouter.ai"));
        assert!(!allow.allows("evilopenrouter.ai"));
        assert!(allow.allows("objects.githubusercontent.com"));
        assert!(allow.allows("githubusercontent.com"));
        assert!(!allow.allows("evilgithubusercontent.com"));
    }

    #[test]
    fn hosts_are_taken_from_urls() {
        assert_eq!(
            host_of("https://openrouter.ai/api/v1").as_deref(),
            Some("openrouter.ai")
        );
        assert_eq!(
            host_of("https://user@Example.com:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn connect_requests_are_parsed() {
        assert_eq!(
            parse_connect("CONNECT openrouter.ai:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("openrouter.ai".into(), 443))
        );
        assert_eq!(
            parse_connect("GET http://example.com/ HTTP/1.1\r\n\r\n"),
            None
        );
    }

    #[test]
    fn private_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    /// Not a test: serves the proxy on TCP for a while, to try a guest image by
    /// hand against it. `FACTORY_TEST_PROXY=<port>:<seconds>:<host,host>`.
    #[tokio::test]
    #[ignore]
    async fn serve_on_tcp_for_manual_experiments() {
        let Ok(config) = std::env::var("FACTORY_TEST_PROXY") else {
            return;
        };
        let mut parts = config.splitn(3, ':');
        let port: u16 = parts.next().unwrap().parse().unwrap();
        let seconds: u64 = parts.next().unwrap().parse().unwrap();
        let hosts = parts
            .next()
            .unwrap_or("")
            .split(',')
            .filter(|h| !h.is_empty())
            .map(str::to_owned);
        let allow = Arc::new(AllowList::new(hosts));
        let (notices, mut seen) = mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, allow.clone(), notices.clone()));
            }
        });
        let _ = tokio::time::timeout(std::time::Duration::from_secs(seconds), async {
            while let Some(notice) = seen.recv().await {
                println!("NOTICE {notice}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_host_off_the_list_is_refused_and_reported() {
        let (mut guest, proxy_side) = tokio::io::duplex(1024);
        let (notices, mut seen) = mpsc::unbounded_channel();
        let allow = Arc::new(AllowList::new(["openrouter.ai".to_owned()]));
        let served = tokio::spawn(serve(proxy_side, allow, notices));
        guest
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        guest.read_to_string(&mut response).await.unwrap();
        served.await.unwrap().unwrap();
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(seen.recv().await.unwrap().contains("example.com:443"));
    }
}
