package epazote

import (
	"bytes"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"reflect"
	"testing"
)

// just to cover
func TestScanReturnFunc(t *testing.T) {
	s := new(Epazote)
	f := s.Scan("test")
	ft := reflect.TypeOf(f).Kind()
	if ft != reflect.Func {
		t.Error("Expecting func()")
	}
}

func TestScanSearchNonexistentRoot(t *testing.T) {
	log.SetOutput(ioutil.Discard)
	s := new(Epazote)
	err := s.search("nonexistent", false)
	if err == nil {
		t.Error("Expecting: lstat nonexistent: no such file or directory")
	}
}

func TestScanSearch(t *testing.T) {
	s := new(Epazote)
	err := s.search("test", false)
	if err != nil {
		t.Error(err)
	}
}

func TestScanParseScanErr(t *testing.T) {
	dir := "./"
	prefix := "test-scan1-"

	d, err := ioutil.TempDir(dir, prefix)

	if err != nil {
		fmt.Println(err)
	}

	defer os.RemoveAll(d)

	f := []byte(`epazote
    - bad`)

	err = ioutil.WriteFile(fmt.Sprintf("%s/epazote.yml", d), f, 0644)

	s := new(Epazote)
	err = s.search(d, false)
	if err == nil {
		t.Error(err)
	}
}

func TestScanParseScanSearchOk(t *testing.T) {
	var buf bytes.Buffer
	log.SetOutput(&buf)
	log.SetFlags(0)
	dir := "./"
	prefix := "test-scan2-"

	d, err := ioutil.TempDir(dir, prefix)

	if err != nil {
		fmt.Println(err)
	}

	defer os.RemoveAll(d)

	f := []byte(`
    service 1:
        url: http://about.epazote.io
        expect:
           body: "123"
`)

	err = ioutil.WriteFile(fmt.Sprintf("%s/epazote.yml", d), f, 0644)

	s := &Epazote{
		Services: make(map[string]*Service),
	}
	s.debug = true
	err = s.search(d, false)
	if err != nil {
		t.Error(err)
	}
	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
}

func TestScanParseScanSearchBadRegex(t *testing.T) {
	var buf bytes.Buffer
	log.SetOutput(&buf)
	log.SetFlags(0)
	dir := "./"
	prefix := "test-scan2-"

	d, err := ioutil.TempDir(dir, prefix)

	if err != nil {
		fmt.Println(err)
	}

	defer os.RemoveAll(d)

	f := []byte(`
    service 1:
        url: http://about.epazote.io
        expect:
           body: ?(),
`)

	err = ioutil.WriteFile(fmt.Sprintf("%s/epazote.yml", d), f, 0644)

	s := new(Epazote)
	s.search(d, false)
	if err != nil {
		t.Error(err)
	}

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
	sk := GetScheduler()
	expect(t, len(sk.Schedulers), 1)
}

func TestScanParseScanLast(t *testing.T) {
	var buf bytes.Buffer
	log.SetOutput(&buf)
	log.SetFlags(0)
	dir := "./"
	prefix := "test-scan2-"

	d, err := ioutil.TempDir(dir, prefix)

	if err != nil {
		fmt.Println(err)
	}

	defer os.RemoveAll(d)

	f := []byte(`
    service 1:
        url: http://about.epazote.io
        expect:
            status: 402
`)

	err = ioutil.WriteFile(fmt.Sprintf("%s/epazote.yml", d), f, 0644)
	s := make(Services)
	s["service 1"] = &Service{
		Name: "service 1",
		Expect: Expect{
			Status: 200,
			IfNot: Action{
				Notify: "yes",
			},
		},
		status: 3,
		action: &Action{Cmd: "matilde"},
	}

	ez := &Epazote{
		Services: s,
	}
	ez.search(d, false)
	if err != nil {
		t.Error(err)
	}

	sk := GetScheduler()

	expect(t, len(sk.Schedulers), 1)
	expect(t, 402, s["service 1"].Expect.Status)
	expect(t, 3, s["service 1"].status)
	expect(t, "matilde", s["service 1"].action.Cmd)
}
