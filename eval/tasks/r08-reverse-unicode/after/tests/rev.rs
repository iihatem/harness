use task::reverse;

#[test]
fn reverses_text() {
    assert_eq!(reverse("abc"), "cba");
    assert_eq!(reverse("héllo"), "olléh");
}
