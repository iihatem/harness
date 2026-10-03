package task

import "testing"

func TestRun(t *testing.T) {
	if Double(4) != 8 || Run() != 42 {
		t.Fatal("unexpected result")
	}
}
