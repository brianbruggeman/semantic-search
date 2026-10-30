package cache

import "testing"

func TestPutThenGet(t *testing.T) {
	store := New()
	store.Put("color", "teal")
	if value, ok := store.Get("color"); !ok || value != "teal" {
		t.Fatalf("got %q", value)
	}
}
