package epazote

import (
	"log"
	"os"
	"os/signal"
	"runtime"
	"syscall"
	"time"

	yaml "gopkg.in/yaml.v2"
)

// Block stop until signal received
func (e *Epazote) Block() {
	var m runtime.MemStats
	runtime.ReadMemStats(&m)
	start := time.Now().UTC()
	block := make(chan os.Signal)
	signal.Notify(block, syscall.SIGUSR1, syscall.SIGUSR2)
	for {
		signalType := <-block
		switch signalType {
		case syscall.SIGUSR1, syscall.SIGUSR2:
			// this creates a race condition due the map
			y, err := yaml.Marshal(&e)
			if err != nil {
				log.Printf("Error: %v", err)
			}
			l := `
    Gorutines: %d
    Alloc : %d
    Total Alloc: %d
    Sys: %d
    Lookups: %d
    Mallocs: %d
    Frees: %d
    Seconds in GC: %d
    Started on: %v
    Uptime: %v`
			log.Printf("Config dump:\n%s---"+Green(l),
				y,
				runtime.NumGoroutine(),
				m.Alloc,
				m.TotalAlloc,
				m.Sys,
				m.Lookups,
				m.Mallocs,
				m.Frees,
				m.PauseTotalNs/1000000000,
				start.Format(time.RFC3339),
				time.Since(start))
		default:
			signal.Stop(block)
			log.Printf("%q signal received.", signalType)
			sk := GetScheduler()
			sk.StopAll()
			log.Println("Exiting.")
			os.Exit(0)
		}
	}
}
