package epazote

import (
	"testing"
	"time"
)

type fakeScheduler struct {
	services map[string]int
}

func (sk *fakeScheduler) AddScheduler(name string, interval time.Duration, f func()) {
	sk.services[name] = int(interval)
}

func (sk fakeScheduler) StopAll() {}

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

	expect(t, 3600, sk.services["/my/service/path"])
	expect(t, 30, sk.services["service 1"])
	expect(t, 60, sk.services["check pid"])
}

func TestStartNoServices(t *testing.T) {
	cfg, err := New("test/epazote-start-noservices.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.PathsOrServices()
	if err != nil {
		t.Error(err)
	}
	sk := &fakeScheduler{make(map[string]int)}
	cfg.Start(sk, true)
	expect(t, 0, len(cfg.Services))
}
