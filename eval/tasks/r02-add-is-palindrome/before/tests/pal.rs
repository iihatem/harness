use task::is_palindrome;

#[test]
fn palindromes() {
    assert!(is_palindrome("racecar"));
    assert!(is_palindrome(""));
    assert!(!is_palindrome("rust"));
}
