//! The transport rule for a credential sent over HTTP, shared by every client
//! that sends one: the hosted embedders (`bge_m3`, `embeddinggemma2`,
//! `elastic`, issue #21 and #233) and the `[recall]` Elasticsearch tier.
//!
//! A credential may go over `https`, or over plain `http` to a loopback host,
//! where it never leaves the machine. Any other plain `http` URL would put it
//! on the wire in clear text. A loopback `http` client also ignores the proxy
//! environment, since a proxy would carry the request, and the credential on
//! it, off the machine.
//!
//! Its own module so that the recall tier (feature `recall-elastic`, which
//! enables only `reqwest`) can use it without compiling the embedders. The
//! same goes for [`scrub`], which keeps a credential out of an error body or
//! reason before either is quoted, and for [`read_capped_body`], which bounds
//! how much of a response body is downloaded.

use reqwest::Url;

pub(crate) mod scrub;

/// Is `url`'s host loopback: `localhost`, `127.0.0.0/8`, `::1` or an
/// IPv4-mapped `127.0.0.0/8`? `url` has already normalised the host: IPv4 to
/// a dotted quad (`127.1`, `2130706433` and `0x7f.1` are `127.0.0.1`), IPv6
/// in brackets, domains lower-cased. The name `localhost` is trusted without
/// resolving it.
pub(crate) fn is_loopback_host(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        match host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
        {
            Ok(std::net::IpAddr::V4(a)) => a.is_loopback(),
            Ok(std::net::IpAddr::V6(a)) => {
                a.is_loopback() || a.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
            Err(_) => host == "localhost" || host == "localhost.",
        }
    })
}

/// Is `url` plain `http` to a loopback host? Such a client should ignore the
/// `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` environment when it carries a
/// credential.
pub(crate) fn is_loopback_http(url: &Url) -> bool {
    url.scheme() == "http" && is_loopback_host(url)
}

/// May a credential be sent to `url`? Only over `https`, or over plain `http`
/// to a loopback host.
pub(crate) fn may_carry_credential(url: &Url) -> bool {
    url.scheme() == "https" || is_loopback_http(url)
}

/// Why [`read_capped_body`] returned no body.
#[derive(Debug)]
pub(crate) enum BodyReadError {
    /// The transport failed mid-body: a reset, a stall past the request
    /// timeout (which covers the body), a connection closed early. The server
    /// said nothing about the request, so each caller classifies it as it
    /// classifies a request that got no answer at all (the embedders answer
    /// `EmbedError::Unavailable`, so the write keeps its durable intent).
    Cut(reqwest::Error),
    /// The body is longer than the cap (by `Content-Length`, or once read
    /// that far). The caller refuses the response without quoting any of it.
    TooLarge,
}

/// Read a response body of at most `cap` bytes, in chunks. A
/// `Content-Length` over the cap is refused before anything is read, and a
/// body without one stops at the first chunk that would pass it; the rest is
/// never downloaded, so a hostile or broken server cannot stream without end
/// into memory. Shared by the hosted embedders (their 2xx bodies) and the
/// `[recall]` tier (every body).
pub(crate) async fn read_capped_body(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, BodyReadError> {
    if resp
        .content_length()
        .is_some_and(|len| len > u64::try_from(cap).unwrap_or(u64::MAX))
    {
        return Err(BodyReadError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(BodyReadError::Cut)? {
        if chunk.len() > cap - body.len() {
            return Err(BodyReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test URL parses")
    }

    #[test]
    fn loopback_hosts_in_every_spelling_are_loopback() {
        for s in [
            "http://127.0.0.1:9200",
            "http://127.1",
            "http://2130706433",
            "http://0x7f.1",
            "http://127.255.255.254",
            "http://localhost:9200",
            "HTTP://LOCALHOST",
            "http://localhost.:9200",
            "http://[::1]:9200",
            "http://[::ffff:127.0.0.1]",
        ] {
            assert!(is_loopback_host(&url(s)), "{s}");
        }
        for s in [
            "http://10.0.0.5:9200",
            "http://192.168.1.2",
            "http://[::2]",
            "http://localhost.example.com",
            "http://my-cluster.es.example.com",
        ] {
            assert!(!is_loopback_host(&url(s)), "{s}");
        }
    }

    #[test]
    fn a_credential_rides_https_or_loopback_http_only() {
        for s in [
            "https://my-cluster.es.example.com",
            "https://127.0.0.1:9200",
            "http://127.0.0.1:9200",
            "http://localhost:9200",
            "http://[::1]:9200",
        ] {
            assert!(may_carry_credential(&url(s)), "{s}");
        }
        for s in [
            "http://my-cluster.es.example.com:9200",
            "http://10.0.0.5:9200",
            "ftp://127.0.0.1",
        ] {
            assert!(!may_carry_credential(&url(s)), "{s}");
        }
    }

    #[test]
    fn only_plain_http_to_loopback_is_loopback_http() {
        assert!(is_loopback_http(&url("http://127.0.0.1:9200")));
        assert!(!is_loopback_http(&url("https://127.0.0.1:9200")));
        assert!(!is_loopback_http(&url("http://my-cluster.es.example.com")));
    }
}
