use task::sum_to;

#[test]
fn adds_up_to_and_including_n() {
    assert_eq!(sum_to(4), 10);
    assert_eq!(sum_to(1), 1);
    assert_eq!(sum_to(0), 0);
}
