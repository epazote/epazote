package epazote

import (
	"gopkg.in/yaml.v2"
	"log"
	"os"
	"os/signal"
	"runtime"
	"syscall"
	"time"
)

func (self *Epazote) Block() {
	// stop until signal received
	start := time.Now().UTC()

	// loop forever
	block := make(chan os.Signal)

	signal.Notify(block, os.Interrupt, os.Kill, syscall.SIGTERM, syscall.SIGUSR1, syscall.SIGUSR2)

	for {
		signalType := <-block
		switch signalType {
		case syscall.SIGUSR1, syscall.SIGUSR2:
			y, err := yaml.Marshal(&self)
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

			runtime.NumGoroutine()
			s := new(runtime.MemStats)
			runtime.ReadMemStats(s)

			log.Printf("Config dump:\n%s---"+Green(l), y, runtime.NumGoroutine(), s.Alloc, s.TotalAlloc, s.Sys, s.Lookups, s.Mallocs, s.Frees, s.PauseTotalNs/1000000000, start.Format(time.RFC3339), time.Since(start))

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
