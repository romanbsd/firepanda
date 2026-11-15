//! Shared browser-impersonated HTTP client for non-browser requests: the `fetch`
//! tool and search-engine page loads. Requests run under an absolute deadline
//! and a caller-supplied body cap.
//!
//! primp negotiates HTTP/2 over TLS via ALPN (Chrome's HTTP/2 window settings)
//! and falls back to HTTP/1.1; cleartext requests use HTTP/1.1. One process-wide
//! client keeps pooled connections (90 s idle), `TCP_NODELAY`, and compression
//! support, so repeated fetches reuse connections instead of reconnecting.
//! Referer is off. Redirects stop at 10 hops and refuse a hop `normalize_parsed`
//! would reject, before that hop is sent.
//!
//! With `--no-private-network`, every request (including each redirect hop)
//! refuses non-public destinations: IP-literal hosts are checked before the
//! request is sent, and hostnames resolve only to public addresses, so a name
//! that points at loopback, a private range, or link-local metadata fails
//! instead of connecting.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

/// Client backstop. Per-request deadlines are always stricter.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// Set once at startup, before the first request builds the client.
static BLOCK_PRIVATE: OnceLock<bool> = OnceLock::new();

/// Refuse private, loopback, link-local, and other non-public destinations
/// for every non-browser request. Call before the first request.
pub fn block_private_network() {
    let _ = BLOCK_PRIVATE.set(true);
}

fn blocking_private() -> bool {
    BLOCK_PRIVATE.get().copied().unwrap_or(false)
}

/// One process-wide client: connection pooling and the browser fingerprint are
/// shared by every request.
pub fn client() -> Result<&'static primp::Client, String> {
    static CLIENT: OnceLock<Result<primp::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let builder = primp::Client::builder()
                .impersonate(primp::Impersonate::ChromeV146)
                .timeout(CLIENT_TIMEOUT)
                .referer(false)
                .redirect(primp::redirect::Policy::custom(safe_redirect));
            let builder = if blocking_private() {
                builder.dns_resolver(PublicOnly)
            } else {
                builder
            };
            builder
                .build()
                .map_err(|error| format!("HTTP client failed: {}", error.without_url()))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// A GET through the shared client that refuses a non-public IP-literal
/// destination before anything is sent. Hostnames are covered by the resolver.
pub fn get(url: &str) -> Result<primp::RequestBuilder, String> {
    let parsed = url::Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
    check_destination(&parsed)?;
    Ok(client()?.get(url))
}

/// Follow a redirect only when the next hop is a URL Firepanda would accept,
/// and stop after the default 10 hops. A custom policy has to enforce the
/// loop itself.
fn safe_redirect(attempt: primp::redirect::Attempt<'_>) -> primp::redirect::Action {
    if let Err(message) = crate::tools::normalize_parsed(attempt.url().clone()) {
        return attempt.error(message);
    }
    if let Err(message) = check_destination(attempt.url()) {
        return attempt.error(message);
    }
    primp::redirect::Policy::limited(10).redirect(attempt)
}

fn private_message(ip: IpAddr) -> String {
    format!("Access to private/internal IP address {ip} is blocked")
}

/// IP-literal hosts never reach the resolver, so check them here.
fn check_destination(url: &url::Url) -> Result<(), String> {
    if !blocking_private() {
        return Ok(());
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => return Ok(()),
    };
    if is_public(ip) {
        Ok(())
    } else {
        Err(private_message(ip))
    }
}

/// System resolver that drops non-public answers. A name with only non-public
/// answers fails; mixed answers connect only to the public ones, so DNS
/// rebinding cannot steer a connection inward.
struct PublicOnly;

impl primp::dns::Resolve for PublicOnly {
    fn resolve(&self, name: primp::dns::Name) -> primp::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let first = resolved.first().map(SocketAddr::ip);
            let public: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|address| is_public(address.ip()))
                .collect();
            if public.is_empty() {
                let message =
                    first.map_or_else(|| format!("No addresses for {host}"), private_message);
                return Err(message.into());
            }
            Ok(Box::new(public.into_iter()) as primp::dns::Addrs)
        })
    }
}

/// Globally routable unicast only. IPv4-mapped IPv6 is judged as IPv4.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map_or_else(|| is_public_v6(ip), is_public_v4),
    }
}

const fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0 // "this network"
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // includes the 169.254.169.254 metadata service
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || (a == 100 && (b & 0xc0) == 64) // 100.64.0.0/10 shared address space
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24 protocol assignments
        || (a == 198 && (b & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || s[..6] == [0; 6] // ::/96 IPv4-compatible (deprecated)
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (s[0] & 0xffc0) == 0xfec0 // fec0::/10 site-local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64 reaches embedded IPv4
        || s[0] == 0x2002) // 6to4 embeds IPv4
}

/// Read a body up to `max_bytes`, failing as soon as the deadline expires.
/// `label` prefixes error messages ("Fetch", "Search").
pub async fn read_limited(
    mut response: primp::Response,
    max_bytes: usize,
    deadline: tokio::time::Instant,
    label: &str,
) -> Result<Vec<u8>, String> {
    let declared = response.content_length();
    if declared.is_some_and(|length| length > u64::try_from(max_bytes).unwrap_or(u64::MAX)) {
        return Err(too_large(label, max_bytes));
    }
    let expected = usize::try_from(declared.unwrap_or(0))
        .unwrap_or(max_bytes)
        .min(max_bytes);
    let mut body = Vec::with_capacity(expected);
    loop {
        let chunk = tokio::time::timeout_at(deadline, response.chunk())
            .await
            .map_err(|_| format!("{label} request timed out"))?
            .map_err(|error| format!("{label} response failed: {}", error.without_url()))?;
        let Some(chunk) = chunk else {
            break;
        };
        if chunk.len() > max_bytes - body.len() {
            return Err(too_large(label, max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn too_large(label: &str, max_bytes: usize) -> String {
    format!("{label} response exceeds {} MiB", max_bytes / (1024 * 1024))
}

/// UTF-8 is the normal case; a lossy decode only matters for malformed bodies.
pub fn text_or_lossy(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(address: &str) -> bool {
        is_public(address.parse().expect("valid IP"))
    }

    #[test]
    fn non_public_addresses_are_refused() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "::",
            "::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "2001:db8::1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
            "ff02::1",
        ] {
            assert!(!public(address), "{address} must be refused");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.128.0.1",
            "172.32.0.1",
            "::ffff:8.8.8.8",
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
        ] {
            assert!(public(address), "{address} must be allowed");
        }
    }
}
