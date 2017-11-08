package main

import (
	"flag"
	"fmt"
	"os"

	"github.com/epazote/epazote"
)

var version string

func main() {
	var (
		c = flag.Bool("c", false, "Continue on errors")
		d = flag.Bool("d", false, "Debug mode")
		f = flag.String("f", "epazote.yml", "Configuration `file.yml`")
		v = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
	)

	flag.Parse()

	if *v {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	if _, err := os.Stat(*f); os.IsNotExist(err) {
		fmt.Fprintf(os.Stderr, "Cannot read configuration file: %s, use -h for more info.\n", *f)
		os.Exit(1)
	}

	cfg, err := epazote.New(*f)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	if cfg == nil {
		fmt.Fprintln(os.Stderr, "Check config file sintax.")
		os.Exit(1)
	}

	// scan check config and clean paths
	if err = cfg.CheckPaths(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	// verify URL, we can't supervice unreachable services
	if err = cfg.VerifyUrls(); err != nil {
		if !*c {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Fprintln(os.Stderr, err)
	}

	// check that at least a path or service are set
	if err = cfg.PathsOrServices(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	// verifyEMAIL recipients & headers
	if err = cfg.VerifyEmail(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	// create a Scheduler
	sk := epazote.GetScheduler()

	cfg.Start(sk, *d)

	// run forever until ctrl+c or kill signal
	cfg.Block()
}
