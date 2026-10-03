package task

import "testing"

func TestSum(t *testing.T) {
	if got := Sum(4); got != 10 {
		t.Fatalf("Sum(4) = %d, want 10", got)
	}
	if got := Sum(0); got != 0 {
		t.Fatalf("Sum(0) = %d, want 0", got)
	}
}
