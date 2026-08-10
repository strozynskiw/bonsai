fn parse_bounded_u16(input: &str, field: &str, maximum: u16) -> Result<u16, String> {
    let value = input
        .parse::<u16>()
        .map_err(|error| format!("invalid {field}: {error}"))?;
    if value > maximum {
        return Err(format!("{field} must not exceed {maximum}"));
    }
    Ok(value)
}

pub fn parse_port(input: &str) -> Result<u16, String> {
    parse_bounded_u16(input, "port", u16::MAX)
}

pub fn parse_retry_limit(_input: &str) -> Result<u16, String> {
    todo!("follow the established parser boundary")
}
