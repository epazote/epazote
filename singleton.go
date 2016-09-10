package epazote

import (
	sk "github.com/nbari/epazote/scheduler"
	"sync"
)

var instance *sk.Scheduler
var once sync.Once

func GetScheduler() *sk.Scheduler {
	once.Do(func() {
		instance = sk.New()
	})
	return instance
}
