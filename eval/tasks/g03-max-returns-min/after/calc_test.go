package task

import "testing"

func TestMax(t *testing.T) {
	if Max(2, 7) != 7 || Max(7, 2) != 7 || Max(3, 3) != 3 {
		t.Fatal("Max is wrong")
	}
}
