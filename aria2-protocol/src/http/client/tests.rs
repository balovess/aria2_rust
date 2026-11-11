use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn default_options_do_not_advertise_gzip() {
    assert!(!HttpClientOptions::default().accept_gzip);
}

#[test]
fn fallible_header_builder_reports_invalid_input() {
    let client = HttpClient::default_client().unwrap();

    let name_error = client
        .get("http://example.test")
        .try_header("invalid header name", "value");
    assert!(matches!(
        name_error,
        Err(HttpRequestBuilderError::InvalidName(_))
    ));

    let value_error = client
        .get("http://example.test")
        .try_header("X-Test", "invalid\nvalue");
    assert!(matches!(
        value_error,
        Err(HttpRequestBuilderError::InvalidValue(_))
    ));
}

#[tokio::test]
async fn chainable_header_reports_invalid_input_without_panicking() {
    let client = HttpClient::default_client().unwrap();
    let result = client
        .get("http://example.test")
        .header("invalid header name", "value")
        .send()
        .await;

    assert!(matches!(
        result,
        Err(error) if error.contains("invalid header name")
    ));
}

#[test]
fn test_should_follow_301() {
    let action = RedirectHandler::should_follow_redirect(301, "GET", 0, 5);
    assert!(action.is_some());

    let no_action = RedirectHandler::should_follow_redirect(301, "GET", 5, 5);
    assert!(no_action.is_none());
}

#[test]
fn test_should_follow_300() {
    let action = RedirectHandler::should_follow_redirect(300, "GET", 0, 5);
    assert!(matches!(action, Some(RedirectAction::FollowKeepMethod)));
}

#[test]
fn test_should_not_follow_200() {
    let action = RedirectHandler::should_follow_redirect(200, "GET", 0, 5);
    assert!(action.is_none());
}

#[test]
fn test_resolve_absolute_redirect() {
    let url = RedirectHandler::resolve_redirect_url(
        "http://example.com/page",
        "http://other.example.com/new",
    )
    .unwrap();
    assert_eq!(url, "http://other.example.com/new");
}

#[test]
fn test_resolve_relative_redirect() {
    let url =
        RedirectHandler::resolve_redirect_url("http://example.com/old/path", "/new/path").unwrap();
    assert_eq!(url, "http://example.com/new/path");
}

#[test]
fn test_resolve_relative_path_redirect() {
    let url =
        RedirectHandler::resolve_redirect_url("http://example.com/old/page.html", "new-page.html")
            .unwrap();
    assert_eq!(url, "http://example.com/old/new-page.html");
}

#[test]
fn test_302_changes_to_get() {
    let action = RedirectHandler::should_follow_redirect(302, "POST", 0, 5);
    assert!(action.is_some());
    if let Some(action) = action {
        assert!(matches!(action, RedirectAction::FollowChangeToGet));
    }
}

#[test]
fn test_307_keeps_method() {
    let action = RedirectHandler::should_follow_redirect(307, "POST", 0, 5);
    assert!(action.is_some());
    if let Some(action) = action {
        assert!(matches!(action, RedirectAction::FollowKeepMethod));
    }
}

#[tokio::test]
async fn execute_stream_exposes_metadata_and_incremental_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        let _ = connection.read(&mut request).await.unwrap();
        connection
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nX-Test: stream\r\nConnection: close\r\n\r\nhello world",
                )
                .await
                .unwrap();
        connection.shutdown().await.unwrap();
    });

    let client = HttpClient::default_client().unwrap();
    let mut response = client
        .get(format!("http://{address}/payload"))
        .send_stream()
        .await
        .unwrap();
    assert_eq!(response.status_code(), 200);
    assert_eq!(response.status_text(), "OK");
    assert_eq!(response.header("x-test"), Some(&"stream".to_string()));

    let mut body = Vec::new();
    let mut chunks = 0;
    while let Some(chunk) = response.next_chunk().await {
        chunks += 1;
        body.extend_from_slice(&chunk.unwrap());
    }
    assert!(chunks > 0);
    assert_eq!(body, b"hello world");
    server.await.unwrap();
}

#[tokio::test]
async fn execute_stream_propagates_truncated_body_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        let _ = connection.read(&mut request).await.unwrap();
        connection
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort")
            .await
            .unwrap();
        connection.shutdown().await.unwrap();
    });

    let client = HttpClient::default_client().unwrap();
    let mut response = client
        .execute_stream(HttpRequest::get(format!("http://{address}/truncated")))
        .await
        .unwrap();
    let mut saw_error = false;
    while let Some(chunk) = response.next_chunk().await {
        if chunk.is_err() {
            saw_error = true;
            break;
        }
    }
    assert!(
        saw_error,
        "premature EOF must not look like a successful body"
    );
    server.await.unwrap();
}
