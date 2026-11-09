use std::time::Duration;

use netweir_core::{FetchErrorKind, FetchOptions, Fetcher, Profile};

fn fetcher() -> Fetcher {
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.timeout = Duration::from_secs(5);
    Fetcher::new(options).unwrap()
}

#[tokio::test]
async fn a_refused_connection_is_a_connect_error() {
    // A privileged port nothing listens on, which no test's server can be
    // given. (A port bound and let go was taken by another test's server
    // on CI, which reset the connection during the TLS handshake. A port
    // held bound but not listening isn't refused on macOS: the connection
    // waits out the timeout.)
    let port = 1;
    let err = fetcher()
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Connect, "{err}");
}

#[tokio::test]
async fn a_self_signed_certificate_is_a_tls_error() {
    let server = netweir_fingerprint::Server::start().await.unwrap();
    let err = fetcher()
        .get(&format!("https://localhost:{}/", server.port), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Tls, "{err}");
}

#[tokio::test]
async fn a_silent_server_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _held = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.timeout = Duration::from_millis(300);
    let err = Fetcher::new(options)
        .unwrap()
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Timeout, "{err}");
}

#[tokio::test]
async fn bad_input_is_invalid() {
    let f = fetcher();
    for url in ["not a url", "ftp://example.com/", ""] {
        let err = f.get(url, &[]).await.unwrap_err();
        assert_eq!(err.kind, FetchErrorKind::Invalid, "{url:?}: {err}");
    }
    let bad_header = [("bad header".to_string(), "x".to_string())];
    let err = f
        .get("https://example.com/", &bad_header)
        .await
        .unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::Invalid);

    // A scheme the client doesn't proxy with used to be accepted, and every
    // request then went out directly, past the proxy.
    for proxy in ["not a proxy", "bogus://127.0.0.1:9", "ftp://127.0.0.1:9"] {
        let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
        options.proxy = Some(proxy.into());
        let err = Fetcher::new(options).err().expect(proxy);
        assert_eq!(err.kind, FetchErrorKind::Invalid, "{proxy}: {err}");
    }
    for proxy in [
        "http://127.0.0.1:9",
        "HTTPS://u:p@127.0.0.1:9",
        "socks5://127.0.0.1:9",
        "socks5h://127.0.0.1:9",
    ] {
        let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
        options.proxy = Some(proxy.into());
        assert!(Fetcher::new(options).is_ok(), "{proxy}");
    }
}

/// Serves `head` and `body` once, as raw HTTP/1.1, and returns the URL.
async fn serve_once(head: String, body: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let _ = s.write_all(head.as_bytes()).await;
        let _ = s.write_all(&body).await;
        let _ = s.shutdown().await;
    });
    format!("http://127.0.0.1:{port}/")
}

fn capped(max: u64) -> Fetcher {
    let mut options = FetchOptions::new(Profile::named("chrome").unwrap());
    options.timeout = Duration::from_secs(5);
    options.max_body = Some(max);
    Fetcher::new(options).unwrap()
}

#[tokio::test]
async fn a_response_that_says_its_too_large_is_refused() {
    let head = "HTTP/1.1 200 OK\r\nContent-Length: 2000\r\nConnection: close\r\n\r\n".to_string();
    let url = serve_once(head, vec![b'x'; 2000]).await;
    let err = capped(1000).get(&url, &[]).await.unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::TooLarge, "{err}");
    assert!(err.message.contains("1000"), "{err}");
}

#[tokio::test]
async fn a_response_that_doesnt_say_is_cut_off_at_the_limit() {
    let head = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_string();
    let url = serve_once(head, vec![b'x'; 5000]).await;
    let err = capped(1000).get(&url, &[]).await.unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::TooLarge, "{err}");
}

#[tokio::test]
async fn the_limit_counts_what_a_compressed_body_unpacks_to() {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&vec![b'x'; 100_000]).unwrap();
    let small = gz.finish().unwrap();
    assert!(small.len() < 1000);
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        small.len()
    );
    let url = serve_once(head, small).await;
    let err = capped(10_000).get(&url, &[]).await.unwrap_err();
    assert_eq!(err.kind, FetchErrorKind::TooLarge, "{err}");
}

#[tokio::test]
async fn a_response_within_the_limit_arrives_whole() {
    let head = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_string();
    let url = serve_once(head, vec![b'x'; 1000]).await;
    let response = capped(1000).get(&url, &[]).await.unwrap();
    assert_eq!(response.body.len(), 1000);
}
