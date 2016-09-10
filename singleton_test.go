package epazote

import (
	"testing"
)

func TestSingleton(t *testing.T) {

	instance1 := GetScheduler()
	instance2 := GetScheduler()

	if instance1 != instance2 {
		t.Errorf("Expect instance to equal, but not equal.\n")
	}

}
