package epazote

import (
	"log"
	"os"
	"regexp"
	"strings"
)

// Scheduler interface
type Scheduler interface {
	AddScheduler(string, int, func())
}

// Start Add services to scheduler
func (e *Epazote) Start(sk Scheduler, debug bool) {
	if debug {
		e.debug = true
	}

	for k, v := range e.Services {
		// Set service name
		v.Name = k

		// Status
		if v.Expect.Status < 1 {
			v.Expect.Status = 200
		}

		// rxBody
		if v.Expect.Body != "" {
			re := regexp.MustCompile(v.Expect.Body)
			v.Expect.body = re
		}

		// retry
		if v.RetryInterval == 0 {
			v.RetryInterval = 500
		}
		if v.RetryLimit == 0 {
			v.RetryLimit = 3
		}

		if v.Test.Test != "" {
			v.Test.Test = strings.TrimSpace(v.Test.Test)
		}

		if e.debug {
			if v.URL != "" {
				log.Printf(Green("Adding service: %s URL: %s"), v.Name, v.URL)
			} else {
				log.Printf(Green("Adding service: %s Test: %s"), v.Name, v.Test.Test)
			}
		}

		// schedule the service
		sk.AddScheduler(k, GetInterval(60, v.Every), e.Supervice(v))
	}

	if len(e.Config.Scan.Paths) > 0 {
		for _, v := range e.Config.Scan.Paths {
			sk.AddScheduler(v, GetInterval(300, e.Config.Scan.Every), e.Scan(v))
			// schedule the scan but also scan at the beginning
			e.search(v)
		}
	}

	log.Printf("Epazote %c   on %d services, scan paths: %s [pid: %d]", Icon(herb), len(e.Services), strings.Join(e.Config.Scan.Paths, ","), os.Getpid())
}
