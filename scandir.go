package epazote

import (
	"log"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

// Scan return func() to work with the scheduler
func (e *Epazote) Scan(dir string) func() {
	return func() {
		e.search(dir, true)
	}
}

// search walk through defined paths if check is true
// if will only update if modtime < refresh interval
func (e *Epazote) search(root string, check bool) error {
	if e.debug {
		log.Printf("Starting scan in: %s", root)
	}
	find := func(path string, f os.FileInfo, err error) error {
		if err != nil {
			return err
		}

		if strings.HasSuffix(f.Name(), "epazote.yml") {
			// only update if has been updated since last scan
			if check {
				interval := GetInterval(300, e.Config.Scan.Every)
				if int(time.Now().Sub(f.ModTime()).Seconds()) > interval {
					return nil
				}
			}

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

				// Add/Update existing services
				e.Lock()
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

				if e.debug {
					log.Printf(Green("Found epazote.yml in path: %s updating/adding service: %q"), path, k)
				}

				// schedule service
				e.RLock()
				if v.Disable {
					sk.Stop(k)
				} else {
					sk.AddScheduler(k, GetInterval(60, v.Every), e.Supervice(e.Services[k]))
				}
				e.RUnlock()
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
