package task

import "testing"

func TestIsPalindrome(t *testing.T) {
	if !IsPalindrome("racecar") || !IsPalindrome("") {
		t.Fatal("expected palindromes")
	}
	if IsPalindrome("go") {
		t.Fatal("go is not a palindrome")
	}
}
