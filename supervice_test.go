package epazote

import (
	"encoding/json"
	"fmt"
	"io/ioutil"
	"log"
	"net/http"
	"net/http/httptest"
	"os"
	"regexp"
	"sync"
	"testing"
)

type Wanted struct {
	Name    string
	Exit    int
	Status  int
	Output  string
	Because string
	Retries int
	Test    string
}

func TestSupervice(t *testing.T) {
	var wg sync.WaitGroup
	wa := Wanted{}
	type Return struct {
		StatusCode int
		Body       string
		Header     map[string]string
		redirect   bool
	}
	rs := &Return{}
	checkEnd := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, rs.Body)
	}))
	defer checkEnd.Close()
	checkServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		if rs.redirect {
			http.Redirect(w, r, checkEnd.URL, http.StatusFound)
		} else {
			if rs.Header != nil {
				for k, v := range rs.Header {
					w.Header().Set(k, v)
				}
			}
			w.WriteHeader(rs.StatusCode)
			fmt.Fprintln(w, rs.Body)
		}
	}))
	defer checkServer.Close()
	logServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		decoder := json.NewDecoder(r.Body)
		var i Wanted
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		expect(t, wa.Name, i.Name)
		expect(t, wa.Exit, i.Exit)
		expect(t, wa.Status, i.Status)
		expect(t, wa.Output, i.Output)
		expect(t, wa.Because, i.Because)
		expect(t, wa.Retries, i.Retries)
		expect(t, wa.Test, i.Test)
		wg.Done()
	}))
	defer logServer.Close()
	var testTable = []struct {
		s      Services
		r      *Return
		expect Wanted
	}{
		{map[string]*Service{"s 1": {
			Name: "s 1",
			Test: Test{
				Test: "test 3 -gt 2",
			},
			Log: logServer.URL,
		}},
			nil,
			Wanted{"s 1", 0, 0, "", "Test cmd: ", 0, "test 3 -gt 2"},
		},
		{map[string]*Service{"s 1": {
			Name: "s 2",
			Test: Test{
				Test: "test 3 -gt 5",
			},
			Log: logServer.URL,
		}},
			nil,
			Wanted{"s 2", 1, 0, "No defined cmd", "Test cmd: exit status 1", 0, "test 3 -gt 5"},
		},
		{map[string]*Service{"s 1": {
			Name: "s 3",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 201,
			},
		}},
			&Return{http.StatusCreated, "body", nil, false},
			Wanted{"s 3", 0, 201, "", "Status: 201", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 4 regex match",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Body: "(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}",
				body: regexp.MustCompile(`(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}`),
			},
		}},
			&Return{http.StatusOK, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex", nil, false},
			Wanted{"s 4 regex match", 0, 200, "", "Body regex match: 0BC20225-2E72-4646-9202-8467972199E1", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 5 regex no match",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Body: "(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}",
				body: regexp.MustCompile(`[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}`),
			},
		}},
			&Return{http.StatusOK, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex", nil, false},
			Wanted{"s 5 regex no match", 1, 200, "No defined cmd", "Body no regex match: [a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 6 bad url",
			URL:  "http://",
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				IfNot: Action{
					Cmd: "test 1 -gt 2",
				},
			},
		}},
			nil,
			Wanted{"s 6 bad url", 1, 0, "exit status 1", "GET: http: no Host in request URL", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 7 bad url no cmd output",
			URL:  "http://",
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				IfNot: Action{
					Cmd: "test 3 -gt 2",
				},
			},
		}},
			nil,
			Wanted{"s 7 bad url no cmd output", 1, 0, "", "GET: http: no Host in request URL", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 8 match 502",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				IfNot:  Action{},
			},
			IfStatus: map[int]Action{
				501: {},
				502: {},
				503: {},
			},
			IfHeader: map[string]Action{
				"x-amqp-kapputt": {Notify: "yes"},
			},
		}},
			&Return{http.StatusBadGateway, "", nil, false},
			Wanted{"s 8 match 502", 1, 502, "No defined cmd", "Status: 502", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 9 ifstatus no match",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				IfNot:  Action{},
			},
			IfStatus: map[int]Action{
				501: {},
				502: {},
				503: {},
			},
			IfHeader: map[string]Action{
				"x-amqp-kapputt": {Notify: "yes"},
				"x-db-kapputt":   {},
			},
		}},
			&Return{http.StatusHTTPVersionNotSupported, "", nil, false},
			Wanted{"s 9 ifstatus no match", 1, 505, "No defined cmd", "Status: 505", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 10 ifHeader match",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				IfNot:  Action{},
			},
			IfStatus: map[int]Action{
				501: {},
				503: {},
			},
			IfHeader: map[string]Action{
				"x-amqp-kapputt": {Notify: "yes"},
				"x-db-kapputt":   {Cmd: "test 1 -gt 2"},
			},
		}},
			&Return{http.StatusOK, "", map[string]string{"x-db-kapputt": "si si si"}, false},
			Wanted{"s 10 ifHeader match", 1, 200, "exit status 1", "Header: x-db-kapputt", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 11 status 202",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 202,
				IfNot:  Action{},
			},
			IfStatus: map[int]Action{
				501: {},
				503: {},
			},
			IfHeader: map[string]Action{
				"x-amqp-kapputt": {Notify: "yes"},
				"x-db-kapputt":   {Cmd: "test 1 -gt 2"},
			},
		}},
			&Return{http.StatusAccepted, "", nil, false},
			Wanted{"s 11 status 202", 0, 202, "", "Status: 202", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 12 missing header",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				Header: map[string]string{
					"test":  "xxx",
					"X-Abc": "xyz",
				},
				IfNot: Action{},
			},
			IfStatus: map[int]Action{
				501: {},
				503: {},
			},
			IfHeader: map[string]Action{
				"x-amqp-kapputt": {Notify: "yes"},
				"x-db-kapputt":   {Cmd: "test 1 -gt 2"},
			},
		}},
			&Return{http.StatusOK, "", map[string]string{"X-Abc": "xyz"}, false},
			Wanted{"s 12 missing header", 1, 200, "No defined cmd", "Header: test: xxx", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 13 matching header",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				Header: map[string]string{
					"X-Abc": "xyz",
				},
				IfNot: Action{},
			},
			IfStatus: map[int]Action{
				501: {},
				503: {},
			},
			IfHeader: map[string]Action{
				"x-amqp-kapputt": {Notify: "yes"},
				"x-db-kapputt":   {Cmd: "test 1 -gt 2"},
			},
		}},
			&Return{http.StatusOK, "", map[string]string{"X-Abc": "xyz"}, false},
			Wanted{"s 13 matching header", 0, 200, "", "Status: 200", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 14 matching header prefix",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 200,
				Header: map[string]string{
					"content-type": "application/json",
				},
				IfNot: Action{},
			},
		}},
			&Return{http.StatusOK, "", map[string]string{"content-type": "application/json; charset=UTF-8"}, false},
			Wanted{"s 14 matching header prefix", 0, 200, "", "Status: 200", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 15 302",
			URL:  checkServer.URL,
			Log:  logServer.URL,
			Expect: Expect{
				Status: 302,
				IfNot:  Action{},
			},
			IfStatus: map[int]Action{
				200: {},
			},
		}},
			&Return{http.StatusFound, "", map[string]string{"content-type": "application/json; charset=UTF-8"}, false},
			Wanted{"s 15 302", 0, 302, "", "Status: 302", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name:   "s 16 follow",
			URL:    checkServer.URL,
			Follow: true,
			Log:    logServer.URL,
			Expect: Expect{
				Status: 200,
				Body:   "(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}",
				body:   regexp.MustCompile(`(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}`),
			},
			IfStatus: map[int]Action{
				302: {},
			},
		}},
			&Return{0, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex", nil, true},
			Wanted{"s 16 follow", 0, 200, "", "Body regex match: 0BC20225-2E72-4646-9202-8467972199E1", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name: "s 17 redirect",
			URL:  checkServer.URL,
			Log:  logServer.URL,
		}},
			&Return{0, "", nil, true},
			Wanted{"s 17 redirect", 1, 302, "No defined cmd", "Status: 302", 0, ""},
		},
		{map[string]*Service{"s 1": {
			Name:      "s 18 readlimit",
			URL:       checkServer.URL,
			Log:       logServer.URL,
			ReadLimit: 5,
			Follow:    true,
			Expect:    Expect{Status: 200},
		}},
			&Return{0, "0123", nil, true},
			Wanted{"s 18 readlimit", 0, 200, "", "Status: 200", 0, ""},
		},
	}
	for _, tt := range testTable {
		wa = tt.expect
		rs = tt.r
		e := &Epazote{
			Services: tt.s,
		}
		wg.Add(1)
		e.Supervice(tt.s["s 1"])()
		wg.Wait()
	}
}

func TestSuperviceSkipCmd(t *testing.T) {
	var wg sync.WaitGroup
	checkServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex")
	}))
	defer checkServer.Close()
	logServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		decoder := json.NewDecoder(r.Body)
		var i Wanted
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		wg.Done()
	}))
	defer logServer.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:   "s 1",
		Follow: true,
		Log:    logServer.URL,
		URL:    checkServer.URL,
		Expect: Expect{
			Status: 201,
			IfNot: Action{
				Notify: "yes",
			},
		},
	}
	ez := &Epazote{
		Services: s,
		debug:    true,
	}
	wg.Add(2)
	ez.Supervice(s["s 1"])()
	ez.Supervice(s["s 1"])()
	wg.Wait()
	expect(t, 2, s["s 1"].status)
	s["s 1"].status = 0
	s["s 1"].Expect.Status = 200
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
	expect(t, 0, s["s 1"].status)
}

func TestSuperviceCount1000(t *testing.T) {
	var wg sync.WaitGroup
	checkServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex")
	}))
	defer checkServer.Close()
	logServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		decoder := json.NewDecoder(r.Body)
		var i Wanted
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		wg.Done()
	}))
	defer logServer.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:   "s 1",
		URL:    checkServer.URL,
		Follow: true,
		Log:    logServer.URL,
		Expect: Expect{
			Status: 201,
			IfNot: Action{
				Notify: "yes",
			},
		},
	}
	ez := &Epazote{
		Services: s,
		debug:    true,
	}
	wg.Add(1000)
	for i := 0; i < 1000; i++ {
		ez.Supervice(s["s 1"])()
	}
	wg.Wait()
	expect(t, 1000, s["s 1"].status)
}

func TestSuperviceRetrie(t *testing.T) {
	var wg sync.WaitGroup
	var server *httptest.Server
	var counter int
	var h http.HandlerFunc = func(w http.ResponseWriter, r *http.Request) {
		if counter <= 1 {
			server.CloseClientConnections()
		}
		w.Header().Set("X-Abc", "xyz")
		fmt.Fprintln(w, "Hello, molcajete.org")
		counter++
	}
	server = httptest.NewServer(h)
	defer server.Close()

	logServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		decoder := json.NewDecoder(r.Body)
		var i Wanted
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		expect(t, 0, i.Exit)
		expect(t, "Body regex match: molcajete", i.Because)
		expect(t, 2, i.Retries)
		expect(t, 200, i.Status)
		wg.Done()
	}))
	defer logServer.Close()
	s := make(Services)
	re := regexp.MustCompile(`molcajete`)
	s["s 1"] = &Service{
		Name:       "s 1",
		URL:        server.URL,
		RetryLimit: 3,
		ReadLimit:  17,
		Log:        logServer.URL,
		Expect: Expect{
			Status: 200,
			Header: map[string]string{
				"X-Abc": "xyz",
			},
			Body: "molcajete",
			body: re,
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
	// 1 try, 2 tries
	expect(t, 2, s["s 1"].retryCount)
	expect(t, 3, counter)
}

func TestSuperviceRetrieLimit(t *testing.T) {
	var wg sync.WaitGroup
	var server *httptest.Server
	var counter int
	var h http.HandlerFunc = func(w http.ResponseWriter, r *http.Request) {
		if counter <= 10 {
			server.CloseClientConnections()
		}
		fmt.Fprintln(w, "Hello")
		counter++
	}
	server = httptest.NewServer(h)
	defer server.Close()

	logServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		decoder := json.NewDecoder(r.Body)
		var i Wanted
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		expect(t, 1, i.Exit)
		expect(t, 4, i.Retries)
		expect(t, 0, i.Status)
		wg.Done()
	}))
	defer logServer.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:          "s 1",
		URL:           server.URL,
		RetryLimit:    5,
		RetryInterval: 1,
		Log:           logServer.URL,
		Expect: Expect{
			Status: 200,
		},
	}
	ez := &Epazote{Services: s}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
	expect(t, 4, s["s 1"].retryCount)
}

func TestSuperviceRetrieLimit0(t *testing.T) {
	var wg sync.WaitGroup
	var server *httptest.Server
	var counter int
	var h http.HandlerFunc = func(w http.ResponseWriter, r *http.Request) {
		if counter > 0 {
			server.CloseClientConnections()
		}
		fmt.Fprintln(w, "Hello")
		counter++
	}
	server = httptest.NewServer(h)
	defer server.Close()
	logServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		decoder := json.NewDecoder(r.Body)
		var i Wanted
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		expect(t, 0, i.Exit)
		expect(t, 0, i.Retries)
		expect(t, 200, i.Status)
		wg.Done()
	}))
	defer logServer.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:          "s 1",
		URL:           server.URL,
		RetryLimit:    0,
		RetryInterval: 1,
		ReadLimit:     1024,
		Log:           logServer.URL,
		Expect:        Expect{Status: 200},
	}
	ez := &Epazote{Services: s}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
	expect(t, 0, s["s 1"].retryCount)
}

func TestSuperviceReadLimit(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestSuperviceReadLimit")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	log.SetOutput(tmpfile)
	log.SetFlags(0)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "0123456789")
	}))
	defer server.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:      "s 1",
		URL:       server.URL,
		ReadLimit: 5,
		Expect: Expect{
			Status: 200,
		},
	}
	ez := &Epazote{Services: s}
	ez.debug = true
	ez.Supervice(s["s 1"])()
	rc := s["s 1"].retryCount
	if rc != 0 {
		t.Errorf("Expecting retryCount = 0 got: %d", rc)
	}
	b, _ := ioutil.ReadFile(tmpfile.Name())
	re := regexp.MustCompile("(?m)[\r\n]+^01234$")
	expect(t, true, re.Match(b))
}
