package task

import "testing"

func TestParsePort(t *testing.T) {
	if n, err := ParsePort("8080"); err != nil || n != 8080 {
		t.Fatalf("got %d, %v", n, err)
	}
	if _, err := ParsePort("abc"); err == nil {
		t.Fatal("expected an error")
	}
}
