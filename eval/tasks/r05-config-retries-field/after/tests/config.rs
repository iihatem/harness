use task::Config;

#[test]
fn keeps_retries() {
    let c = Config::new("svc", 3);
    assert_eq!(c.name, "svc");
    assert_eq!(c.retries, 3);
}
