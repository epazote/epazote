package epazote

import (
	"sync"

	sk "github.com/epazote/scheduler"
)

var instance *sk.Scheduler
var once sync.Once

// GetScheduler return the scheduler
func GetScheduler() *sk.Scheduler {
	once.Do(func() {
		instance = sk.New()
	})
	return instance
}
