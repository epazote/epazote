package epazote

import (
	"sync"

	"github.com/epazote/scheduler"
)

var instance *scheduler.Scheduler
var once sync.Once

// GetScheduler return the scheduler
func GetScheduler() *scheduler.Scheduler {
	once.Do(func() {
		instance = scheduler.New()
	})
	return instance
}
