use task::Stack;

#[test]
fn peek_does_not_remove() {
    let mut s = Stack::new();
    assert_eq!(s.peek(), None);
    s.push(1);
    s.push(2);
    assert_eq!(s.peek(), Some(&2));
    assert_eq!(s.pop(), Some(2));
}
