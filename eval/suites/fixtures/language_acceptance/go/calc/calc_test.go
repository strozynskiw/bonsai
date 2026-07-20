package calc

import "testing"

func TestTotal(t *testing.T) {
	if got := Total([]int{2, 3, 5}); got != 10 {
		t.Fatalf("Total() = %d, want 10", got)
	}
}

func TestEmptyTotal(t *testing.T) {
	if got := Total(nil); got != 0 {
		t.Fatalf("Total(nil) = %d, want 0", got)
	}
}
