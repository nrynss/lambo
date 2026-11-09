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

/// One shared rendering of a reqwest transport failure, for every client that
/// compiles in this module (`embed-bge` and `recall-elastic`; see `lib.rs`).
/// It names what the failure says about retrying — a timeout
/// ([`reqwest::Error::is_timeout`]), a connection failure
/// ([`reqwest::Error::is_connect`]), or any other transport error — then the
/// reqwest message with the URL stripped (the [`reqwest::Error::without_url`]
/// semantics, from a borrow: the ` for url (...)` suffix is removed, and any
/// other rendering of the same URL with it, so no userinfo, query or
/// fragment is ever quoted; the caller's own host-bearing log line carries
/// the where), then the deduplicated source chain (`std::error::Error`
/// sources, first occurrence only), which is where the underlying cause (a
/// refused syscall, an elapsed deadline) is named. A reqwest error carries
/// no key and no response body, so neither can leak through here; callers
/// keep quoting bodies only through [`scrub::quotable_body`].
pub(crate) fn transport_error(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "connection failed"
    } else {
        "transport error"
    };
    let mut out = format!("{kind}: {}", stripped(e));
    // The sources carry the syscall-level cause ("Connection refused",
    // "operation timed out"). Deduped: reqwest repeats its own message as
    // the first source.
    let mut seen = vec![e.to_string(), stripped(e)];
    let mut causes = Vec::new();
    let mut next: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = next {
        next = err.source();
        if let Some(source) = next {
            let text = source.to_string();
            if !seen.contains(&text) {
                seen.push(text.clone());
                causes.push(text);
            }
        }
    }
    if !causes.is_empty() {
        out.push_str(&format!(" (caused by: {})", causes.join("; ")));
    }
    out
}

/// `e`'s message with its URL stripped, the [`reqwest::Error::without_url`]
/// semantics without taking ownership: reqwest renders the URL as a
/// ` for url (<url>)` suffix, which is removed, and any other rendering of
/// the same URL along with it.
fn stripped(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    if let Some(url) = e.url() {
        text = text.replace(&format!(" for url ({url})"), "");
        text = text.replace(url.as_str(), "(url not shown)");
    }
    text
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

    /// A delayed answer past the request timeout names the timeout and keeps
    /// the underlying cause: without the kind an operator cannot tell a slow
    /// cluster from a refused port.
    #[tokio::test]
    async fn transport_error_names_a_timeout_and_its_cause() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.path("/");
                then.status(200)
                    .delay(std::time::Duration::from_millis(400))
                    .body("{}");
            })
            .await;
        let err = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .unwrap()
            .get(server.base_url())
            .send()
            .await
            .expect_err("a 400 ms delay past a 50 ms timeout must fail");
        assert!(err.is_timeout(), "{err:?}");
        let text = transport_error(&err);
        assert!(text.contains("timed out"), "{text}");
        assert!(text.contains("caused by"), "{text}");
    }

    /// A refused connection (a closed port, well inside any timeout) is a
    /// connection failure, never a timeout: the two ask the operator for
    /// different fixes.
    #[tokio::test]
    async fn transport_error_keeps_a_refused_connection_distinct_from_a_timeout() {
        let err = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .expect_err("a closed port must fail");
        assert!(!err.is_timeout(), "{err:?}");
        let text = transport_error(&err);
        assert!(text.contains("connection failed"), "{text}");
        assert!(!text.contains("timed out"), "{text}");
    }

    /// A body the connection cuts off mid-read gets the same diagnostics as
    /// a request that got no answer: the kind and the cause, never the
    /// partial body.
    #[tokio::test]
    async fn transport_error_covers_a_body_cut_off_mid_read() {
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                    content-length: 100000\r\n\r\n"
            .to_string();
        let partial = br#"{"text_embedding":[{"embedding":[0.123456,"#.to_vec();
        let base = crate::test_util::serve_raw_http_once(head, vec![partial]).await;
        let mut resp = reqwest::Client::new()
            .get(base)
            .send()
            .await
            .expect("the head arrives");
        let mut cut = None;
        while let Ok(Some(_)) = resp.chunk().await.map_err(|e| {
            cut = Some(e);
        }) {}
        let text = transport_error(cut.as_ref().expect("the body is cut off"));
        assert!(
            text.contains("connection failed")
                || text.contains("transport error")
                || text.contains("timed out"),
            "{text}"
        );
        assert!(
            !text.contains("0.123456"),
            "the partial body is not quoted: {text}"
        );
    }

    /// Userinfo, query, fragment and any secret in them are never quoted: the
    /// URL is stripped before anything is rendered.
    #[tokio::test]
    async fn transport_error_never_quotes_url_userinfo_or_query() {
        let err = reqwest::Client::new()
            .get("http://fake-user:fake-xyzzy-secret@127.0.0.1:1/?api_key=fake-xyzzy-secret")
            .send()
            .await
            .expect_err("a closed port must fail");
        let text = transport_error(&err);
        assert!(!text.contains("fake-xyzzy"), "{text}");
        assert!(!text.contains("fake-user"), "{text}");
        assert!(!text.contains("api_key"), "{text}");
    }

    /// The source chain is deduplicated: reqwest repeats its own message as
    /// the first source, which must not appear twice.
    #[tokio::test]
    async fn transport_error_dedupes_the_source_chain() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.path("/");
                then.status(200)
                    .delay(std::time::Duration::from_millis(400))
                    .body("{}");
            })
            .await;
        let err = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .unwrap()
            .get(server.base_url())
            .send()
            .await
            .expect_err("a 400 ms delay past a 50 ms timeout must fail");
        let text = transport_error(&err);
        let head = text.split(" (caused by: ").next().unwrap_or(&text);
        if let Some(causes) = text.split(" (caused by: ").nth(1) {
            let causes = causes.strip_suffix(')').unwrap_or(causes);
            let mut seen = vec![head.to_string()];
            for cause in causes.split("; ") {
                assert!(!seen.contains(&cause.to_string()), "repeat: {text}");
                seen.push(cause.to_string());
            }
        }
    }
}
