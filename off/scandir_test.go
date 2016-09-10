package epazote

import (
	"fmt"
	"io/ioutil"
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
	} else {
		f()
	}
}

func TestScanSearchNonexistentRoot(t *testing.T) {
	s := new(Epazote)
	err := s.search("nonexistent")
	if err == nil {
		t.Error("Expecting: lstat nonexistent: no such file or directory")
	}
}

func TestScanSearch(t *testing.T) {
	s := new(Epazote)
	err := s.search("test")
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
	err = s.search(d)
	if err == nil {
		t.Error(err)
	}
}

func TestScanParseScanSearchOk(t *testing.T) {
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
	err = s.search(d)
	if err != nil {
		t.Error(err)
	}
	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
}

func TestScanParseScanSearchBadRegex(t *testing.T) {
	buf.Reset()
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
	s.search(d)
	if err != nil {
		t.Error(err)
	}

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
	sk := GetScheduler()

	if len(sk.Schedulers) != 1 {
		t.Error("Expecting 1")
	}

	buf.Reset()
}

func TestScanParseScanLast(t *testing.T) {
	buf.Reset()
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
	ez.search(d)
	if err != nil {
		t.Error(err)
	}

	sk := GetScheduler()

	if len(sk.Schedulers) != 1 {
		t.Error("Expecting 1")
	}

	if s["service 1"].Expect.Status != 402 {
		t.Errorf("Expecting status 402 got: %v", s["service 1"].Expect.Status)
	}

	if s["service 1"].status != 3 {
		t.Errorf("Expecting status 3 got: %v", s["service 1"].status)
	}

	if s["service 1"].action.Cmd != "matilde" {
		t.Errorf("Expecting Cmd = matilde got: %v", s["service 1"].action.Cmd)
	}

	buf.Reset()
}
