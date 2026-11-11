use super::parse_content_disposition;

#[test]
fn historical_core_module_path_reaches_protocol_parser() {
    let result = parse_content_disposition("attachment; filename=report.txt");
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(result.filename.as_deref(), Some("report.txt"));
}
