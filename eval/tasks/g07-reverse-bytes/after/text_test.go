package task

import "testing"

func TestReverse(t *testing.T) {
	if Reverse("abc") != "cba" || Reverse("héllo") != "olléh" {
		t.Fatal("Reverse is wrong")
	}
}
