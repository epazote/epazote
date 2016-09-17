package epazote

import (
	"bytes"
	"fmt"
	"io/ioutil"
	"log"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestReportNotifyHTTPDefault(t *testing.T) {
	var wg sync.WaitGroup
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expect(t, "GET", r.Method)
		expect(t, "list", r.FormValue("param"))
		wg.Done()
	}))
	defer server.Close()
	a := &Action{
		HTTP: []HTTP{
			{
				URL: fmt.Sprintf("%s/?param=list", server.URL),
			},
		},
	}
	e := &Epazote{}
	wg.Add(1)
	e.Report(nil, &Service{}, a, nil, 1, 200, "because", "output")
	wg.Wait()
}

func TestReportNotifyHTTPEmptyURL(t *testing.T) {
	a := &Action{
		HTTP: []HTTP{{}},
	}
	e := &Epazote{}
	s := &Service{}
	e.Report(nil, s, a, nil, 1, 200, "because", "output")
	expect(t, s.status, 1)
}

func TestReportNotifyHTTPExitCode0(t *testing.T) {
	var wg sync.WaitGroup
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expect(t, "GET", r.Method)
		expect(t, "0", r.FormValue("exit"))
		expect(t, "200", r.FormValue("status"))
		wg.Done()
	}))
	defer server.Close()
	a := &Action{
		HTTP: []HTTP{
			{
				URL: fmt.Sprintf("%s/?exit=_exit_&status=_status_", server.URL),
			},
			{
				URL: fmt.Sprintf("%s/?exit=_exit_", server.URL),
			},
		},
	}
	e := &Epazote{}
	e.debug = true
	wg.Add(1)
	e.Report(nil, &Service{}, a, nil, 0, 200, "because", "output")
	wg.Wait()
}

func TestReportNotifyHTTPExitCode1(t *testing.T) {
	var wg sync.WaitGroup
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expect(t, "GET", r.Method)
		expect(t, "1", r.FormValue("exit"))
		wg.Done()
	}))
	defer server.Close()
	a := &Action{
		HTTP: []HTTP{
			{
				URL: fmt.Sprintf("%s/?exit=0", server.URL),
			},
			{
				URL: fmt.Sprintf("%s/?exit=1", server.URL),
			},
		},
	}
	e := &Epazote{}
	e.debug = true
	wg.Add(1)
	e.Report(nil, &Service{}, a, nil, 1, 200, "because", "output")
	wg.Wait()
}

func TestReportNotifyHTTPPost(t *testing.T) {
	var wg sync.WaitGroup
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expect(t, "POST", r.Method)
		expect(t, "application/x-www-form-urlencoded", r.Header.Get("Content-Type"))

		body, _ := ioutil.ReadAll(r.Body)
		values, _ := url.ParseQuery(string(body))

		var testTable = []struct {
			key, val string
		}{
			{"room_id", "10"},
			{"from", "Alerts"},
			{"message", "A new user signed up"},
			{"status", "200"},
		}

		for _, tt := range testTable {
			got := values[tt.key]
			if got[0] != tt.val {
				t.Errorf("Expecting %s got: %s", got[0], tt.val)
			}
		}
		wg.Done()
	}))
	defer server.Close()
	a := &Action{
		HTTP: []HTTP{
			{
				URL:    server.URL,
				Method: "post",
				Data:   "room_id=10&from=Alerts&message=A+new+user+signed+up&status=_status_",
				Header: map[string]string{
					"Content-Type": "application/x-www-form-urlencoded",
				},
			},
		},
	}
	e := &Epazote{}
	e.debug = true
	wg.Add(1)
	e.Report(nil, &Service{}, a, nil, 1, 200, "because", "output")
	wg.Wait()
}

func TestReportNotifyLogCookies(t *testing.T) {
	var wg sync.WaitGroup
	var buf bytes.Buffer
	log.SetOutput(&buf)
	log.SetFlags(0)
	checkServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expiration := time.Now().Add(30 * 24 * time.Hour)
		cookie := http.Cookie{Name: "galleta", Value: "maria", Expires: expiration}
		http.SetCookie(w, &cookie)
		wg.Done()
	}))
	defer checkServer.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  checkServer.URL,
	}
	e := &Epazote{
		Services: s,
	}
	e.debug = true
	wg.Add(1)
	e.Supervice(s["s 1"])()
	wg.Wait()
	re := regexp.MustCompile(`Set-Cookie.*`)
	match := re.FindString(buf.String())
	expect(t, true, strings.HasPrefix(match, "Set-Cookie\x1b[0;00m: [galleta=maria;"))
}

func TestReportNotifyThresholdHealthydUsing1HTTP(t *testing.T) {
	var wg sync.WaitGroup
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expect(t, "GET", r.Method)
		expect(t, "3", r.FormValue("exit"))
		wg.Done()
	}))
	defer server.Close()
	s := &Service{
		Name: "s 1",
		Threshold: Threshold{
			Healthy:   2,
			Unhealthy: 2,
		},
	}
	e := &Epazote{}
	a := &Action{
		HTTP: []HTTP{
			{
				URL: fmt.Sprintf("%s/?exit=3", server.URL),
			},
		},
	}
	// ignore if exitCode == 0
	e.Report(nil, s, a, nil, 0, 200, "because", "output")
	e.Report(nil, s, a, nil, 0, 200, "because", "output")
	e.Report(nil, s, a, nil, 0, 200, "because", "output")
	// only use call the HTTP on error
	e.Report(nil, s, a, nil, 1, 200, "because", "output")
	wg.Add(1)
	e.Report(nil, s, a, nil, 1, 200, "because", "output")
	wg.Wait()
	expect(t, s.status, 2)
}

func TestReportNotifyThresholdUnhealthydUsing1HTTP(t *testing.T) {
	var wg sync.WaitGroup
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expect(t, "GET", r.Method)
		expect(t, "3", r.FormValue("exit"))
		wg.Done()
	}))
	defer server.Close()
	service := &Service{
		Name: "s 1",
		Threshold: Threshold{
			Healthy:   2,
			Unhealthy: 2,
		},
	}
	e := &Epazote{}
	a := &Action{
		HTTP: []HTTP{
			{
				URL: fmt.Sprintf("%s/?exit=3", server.URL),
			},
		},
	}
	e.Report(nil, service, a, nil, 1, 200, "because", "output")
	wg.Add(1)
	e.Report(nil, service, a, nil, 1, 200, "because", "output")
	wg.Wait()
}

func TestReportNotifyThresholdXhealthyUsing2HTTP(t *testing.T) {
	var wg sync.WaitGroup
	type Return struct {
		exitCode, httpStatus int
		because, output      string
	}
	type Expect struct {
		ua, method, exit string
	}
	ex := Expect{}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, ex.ua, r.Header.Get("User-agent"))
		expect(t, ex.method, r.Method)
		expect(t, ex.exit, r.FormValue("exit"))
		wg.Done()
	}))
	defer server.Close()
	var testTable = []struct {
		r      Return
		expect Expect
	}{
		// Healthy exitCode = 0, Unhealthy exitCode = 1
		{
			Return{0, 200, "because", "output"},
			Expect{"epazote", "GET", "0"},
		},
		{
			Return{1, 200, "because", "output"},
			Expect{"epazote", "GET", "1"},
		},
	}
	for _, tt := range testTable {
		ex = tt.expect
		e := &Epazote{}
		service := &Service{
			Name: "s 1",
			Threshold: Threshold{
				Healthy:   2,
				Unhealthy: 2,
			},
		}
		a := &Action{
			HTTP: []HTTP{
				{
					URL: fmt.Sprintf("%s/?exit=0", server.URL),
				},
				{
					URL: fmt.Sprintf("%s/?exit=1", server.URL),
				},
			},
		}
		e.Report(nil, service, a, nil, tt.r.exitCode, tt.r.httpStatus, tt.r.because, tt.r.output)
		wg.Add(1)
		e.Report(nil, service, a, nil, tt.r.exitCode, tt.r.httpStatus, tt.r.because, tt.r.output)
		wg.Wait()
	}
}

func TestLog(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestLog")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	log.SetOutput(tmpfile)
	log.SetFlags(0)
	e := &Epazote{}
	e.debug = true
	s := &Service{Log: "--http--"}
	e.Log(s, []byte("hello"))
	b, _ := ioutil.ReadFile(tmpfile.Name())
	re := regexp.MustCompile("unsupported protocol scheme")
	expect(t, true, re.Match(b))
}
