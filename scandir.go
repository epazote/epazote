package epazote

import (
	"log"
	"os"
	"path/filepath"
	"regexp"
	"time"
)

// Scan return func() to work with the scheduler
func (e *Epazote) Scan(dir string) func() {
	return func() {
		e.search(dir)
	}
}

// search walk through defined paths
func (e *Epazote) search(root string) error {
	if e.debug {
		log.Printf("Starting scan in: %s", root)
	}
	find := func(path string, f os.FileInfo, err error) error {
		if err != nil {
			return err
		}

		if f.Name() == "epazote.yml" {
			srv, err := ParseScan(path)
			if err != nil {
				return err
			}

			// get a Scheduler
			sk := GetScheduler()

			for k, v := range srv {
				if !IsURL(v.URL) {
					log.Printf("[%s] %s - Verify URL: %q", Red(path), k, v.URL)
					continue
				}

				// Set service name
				v.Name = k

				// Status
				if v.Expect.Status < 1 {
					v.Expect.Status = 200
				}

				// rxBody
				if v.Expect.Body != "" {
					re, err := regexp.Compile(v.Expect.Body)
					if err != nil {
						log.Printf("[%s] %s - Verify Body: %q - %q", Red(path), k, v.Expect.Body, err)
						continue
					}
					v.Expect.body = re
				}

				// retry
				if v.RetryInterval == 0 {
					v.RetryInterval = 500
				}
				if v.RetryLimit == 0 {
					v.RetryLimit = 3
				}

				e.Lock()
				// Add/Update existing services
				if _, ok := e.Services[k]; !ok {
					e.Services[k] = v
				} else {
					lastStatus := e.Services[k].status
					lastAction := e.Services[k].action
					e.Services[k] = v
					e.Services[k].status = lastStatus
					e.Services[k].action = lastAction
				}
				e.Unlock()

				// race condition
				time.Sleep(3 * time.Second)

				if e.debug {
					log.Printf(Green("Found epazote.yml in path: %s updating/adding service: %q"), path, k)
				}

				// schedule service
				sk.AddScheduler(k, GetInterval(60, v.Every), e.Supervice(e.Services[k]))
			}
		}
		return nil
	}

	// Walk over root using find func
	err := filepath.Walk(root, find)
	if err != nil {
		return err
	}

	return nil
}
