pub fn parse_port(s: &str) -> u16 {
    s.trim().parse().unwrap()
}
