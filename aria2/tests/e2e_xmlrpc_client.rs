//! Process-level XML-RPC compatibility coverage for existing aria2 clients.

mod support;

use std::time::Duration;

use support::RunningAria2;

const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

fn xml_call(method: &str, params: &[String]) -> String {
    let params = params
        .iter()
        .map(|value| format!("<param><value><string>{value}</string></value></param>"))
        .collect::<String>();
    format!(
        "<?xml version=\"1.0\"?><methodCall><methodName>{method}</methodName><params>{params}</params></methodCall>"
    )
}

/// XML-RPC clients authenticate with the same leading `token:` parameter as
/// JSON-RPC clients.  Verify the live server's method response shape, string
/// stat values, and shutdown behavior without going through Rust handlers.
#[test]
fn e2e_xmlrpc_client_authenticates_and_reads_aria2_response_shapes() {
    let secret = "xmlrpc-process-secret";
    let token = format!("token:{secret}");
    let mut aria2 = RunningAria2::start_rpc(&[format!("--rpc-secret={secret}")]);

    let version = aria2.post(
        "/rpc",
        "text/xml",
        xml_call("aria2.getVersion", std::slice::from_ref(&token)).as_bytes(),
    );
    assert_eq!(version.status, 200);
    assert!(
        version
            .headers
            .to_ascii_lowercase()
            .contains("content-type: text/xml"),
        "XML-RPC response must preserve aria2's text/xml content type: {}",
        version.headers
    );
    let version_body = std::str::from_utf8(&version.body).expect("XML-RPC response must be UTF-8");
    assert!(version_body.contains("<methodResponse>"));
    assert!(
        version_body.contains("<name>version</name>"),
        "getVersion must return aria2's version struct: {version_body}"
    );

    let global_stat = aria2.post(
        "/rpc",
        "text/xml",
        xml_call("aria2.getGlobalStat", std::slice::from_ref(&token)).as_bytes(),
    );
    assert_eq!(global_stat.status, 200);
    let global_stat_body =
        std::str::from_utf8(&global_stat.body).expect("XML-RPC response must be UTF-8");
    let download_speed_member = global_stat_body
        .split("<member>")
        .find(|member| member.contains("<name>downloadSpeed</name>"))
        .expect("getGlobalStat must return a downloadSpeed member");
    assert!(
        download_speed_member.contains("<value><string>0</string></value>"),
        "getGlobalStat must preserve aria2's string downloadSpeed value: {download_speed_member}"
    );

    let shutdown = aria2.post(
        "/rpc",
        "text/xml",
        xml_call("aria2.shutdown", &[token]).as_bytes(),
    );
    assert_eq!(shutdown.status, 200);
    let shutdown_body =
        std::str::from_utf8(&shutdown.body).expect("XML-RPC response must be UTF-8");
    assert!(
        shutdown_body.contains("<string>OK."),
        "shutdown must return aria2's successful result: {shutdown_body}"
    );
    assert!(aria2.wait_for_exit(PROCESS_EXIT_TIMEOUT).success());
}
