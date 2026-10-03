pub fn reverse(s: &str) -> String {
    String::from_utf8(s.bytes().rev().collect()).unwrap()
}
