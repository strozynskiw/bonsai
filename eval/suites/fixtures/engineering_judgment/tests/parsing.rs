use bonsai_engineering_judgment::{parse_port, parse_retry_limit};

#[test]
fn port_uses_the_shared_parser_contract() {
    assert_eq!(parse_port("8080"), Ok(8080));
}

#[test]
fn retry_limit_accepts_the_boundary() {
    assert_eq!(parse_retry_limit("10"), Ok(10));
}

#[test]
fn retry_limit_rejects_values_above_the_boundary() {
    assert_eq!(
        parse_retry_limit("11"),
        Err("retry limit must not exceed 10".to_string())
    );
}

#[test]
fn retry_limit_preserves_parse_detail() {
    let error = parse_retry_limit("many").expect_err("non-numeric input must fail");
    assert!(error.starts_with("invalid retry limit:"), "{error}");
}
