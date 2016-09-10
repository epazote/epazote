package epazote

import (
	//	"fmt"
	"testing"
)

type fakeScheduler struct {
	services map[string]int
}

func (self *fakeScheduler) AddScheduler(name string, interval int, f func()) {
	self.services[name] = interval
}

func (self fakeScheduler) StopAll() {}

func TestStart(t *testing.T) {
	cfg, err := New("test/epazote-start.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.PathsOrServices()
	if err != nil {
		t.Error(err)
	}
	sk := &fakeScheduler{make(map[string]int)}
	cfg.Start(sk, true)

	if sk.services["/my/service/path"] != 3600 {
		t.Errorf("Expecting 3600 got: %v", sk.services["/my/services/path"])
	}
	if sk.services["service 1"] != 30 {
		t.Errorf("Expecting 30 got: %v", sk.services["service 1"])
	}
	if sk.services["check pid"] != 60 {
		t.Errorf("Expecting 60 got: %v", sk.services["check pid"])
	}

}
