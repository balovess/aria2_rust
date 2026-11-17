use super::MetalinkHttpParser;
use crate::http::header_processor::HttpResponseHead;

#[test]
fn historical_core_module_path_reaches_protocol_parser() {
    let response = HttpResponseHead::from_parts(
        "HTTP/1.1".to_string(),
        200,
        "OK".to_string(),
        vec![(
            "Link".to_string(),
            "<https://mirror.example/file>; rel=\"duplicate\"".to_string(),
        )],
    );

    let result = MetalinkHttpParser::parse_response(&response, &[]);
    assert_eq!(result.links.len(), 1);
    assert_eq!(result.links[0].uri, "https://mirror.example/file");
}
