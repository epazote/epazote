package main

import (
	"flag"
	"fmt"
	"log"
	"os"

	ez "github.com/epazote/epazote"
)

var version, githash string

func main() {
	// f config file name
	var f = flag.String("f", "epazote.yml", "Epazote configuration file.")
	var c = flag.Bool("c", false, "Continue on errors.")
	var d = flag.Bool("d", false, "Debug mode.")
	var v = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))

	flag.Parse()

	if *v {
		if githash != "" {
			fmt.Printf("%s+%s\n", version, githash)
		} else {
			fmt.Printf("%s\n", version)
		}
		os.Exit(0)
	}

	if _, err := os.Stat(*f); os.IsNotExist(err) {
		fmt.Printf("Cannot read file: %s, use -h for more info.\n\n", *f)
		os.Exit(1)
	}

	cfg, err := ez.New(*f)
	if err != nil {
		log.Fatalln(err)
	}

	if cfg == nil {
		log.Fatalln("Check config file sintax.")
	}

	// scan check config and clean paths
	err = cfg.CheckPaths()
	if err != nil {
		log.Fatalln(err)
	}

	// verify URL, we can't supervice unreachable services
	err = cfg.VerifyUrls()
	if err != nil {
		if !*c {
			log.Fatalln(err)
		}
		log.Println(err)
	}

	// check that at least a path or service are set
	err = cfg.PathsOrServices()
	if err != nil {
		log.Fatalln(err)
	}

	// verifyEMAIL recipients & headers
	err = cfg.VerifyEmail()
	if err != nil {
		log.Fatalln(err)
	}

	// create a Scheduler
	sk := ez.GetScheduler()

	cfg.Start(sk, *d)

	// run forever until ctrl+c or kill signal
	cfg.Block()
}
