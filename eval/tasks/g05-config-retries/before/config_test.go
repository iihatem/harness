package task

import "testing"

func TestConfig(t *testing.T) {
	c := NewConfig("svc", 3)
	if c.Name != "svc" || c.Retries != 3 {
		t.Fatalf("unexpected config %+v", c)
	}
}
