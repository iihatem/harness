use task::parse_port;

#[test]
fn parses_and_rejects() {
    assert_eq!(parse_port(" 8080 "), Ok(8080));
    assert!(parse_port("abc").is_err());
}
