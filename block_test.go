package epazote

import (
	"io/ioutil"
	"log"
	"os"
	"regexp"
	"syscall"
	"testing"
	"time"
)

func TestBlock(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestBlock")
	defer os.Remove(tmpfile.Name())
	if err != nil {
		t.Error(err)
	}
	log.SetOutput(tmpfile)
	log.SetFlags(0)
	e := &Epazote{}
	go e.Block()

	select {
	case <-time.After(1 * time.Second):
		syscall.Kill(syscall.Getpid(), syscall.SIGUSR1)
		time.Sleep(time.Second)
		b, _ := ioutil.ReadFile(tmpfile.Name())
		re := regexp.MustCompile(`Gorutines.*`)
		expect(t, true, re.Match(b))
	}
}
