use task::clamp;

#[test]
fn clamps_to_the_bounds() {
    assert_eq!(clamp(5, 0, 10), 5);
    assert_eq!(clamp(-3, 0, 10), 0);
    assert_eq!(clamp(42, 0, 10), 10);
}
