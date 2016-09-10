package epazote

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"regexp"
	"sync"
	"testing"
)

func TestSuperviceTestOk(t *testing.T) {
	var wg sync.WaitGroup
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if i["name"] != "s 1" {
			t.Errorf("Expecting  %q, got: %q", "s 1", i["name"])
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Test cmd: " {
				t.Errorf("Expecting: %q, got: %q", "Test cmd: ", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check test
		if tt, ok := i["test"]; ok {
			if tt != "test 3 -gt 2" {
				t.Errorf("Expecting: %q, got: %q", "Test cmd: ", tt)
			}
		} else {
			t.Errorf("key not found: %q", "test")
		}
		// check url
		if o, ok := i["url"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		if i["status"].(float64) != 0 {
			t.Errorf("Expecting status: %d got: %v", 0, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		Test: Test{
			Test: "test 3 -gt 2",
		},
		Log: log_s.URL,
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceTestNotOk(t *testing.T) {
	var wg sync.WaitGroup
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Test cmd: exit status 1" {
				t.Errorf("Expecting: %q, got: %q", "Test cmd: exit status 1", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 1 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check test
		if tt, ok := i["test"]; ok {
			if tt != "test 3 -gt 5" {
				t.Errorf("Expecting: %q, got: %q", "Test cmd: ", tt)
			}
		} else {
			t.Errorf("key not found: %q", "test")
		}
		// check url
		if o, ok := i["url"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		// check output
		if o, ok := i["output"]; ok {
			e := "No defined cmd"
			if o != e {
				t.Errorf("Expecting %q, got %q", e, o)
			}
		} else {
			t.Errorf("key not found: %q", "output")
		}
		if i["status"].(float64) != 0 {
			t.Errorf("Expecting status: %d got: %v", 0, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		Test: Test{
			Test: "test 3 -gt 5",
		},
		Log: log_s.URL,
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceStatusCreated(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.WriteHeader(http.StatusCreated)
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 201" {
				t.Errorf("Expecting: %q, got: %q", "Status: 201", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		if i["status"].(float64) != 201 {
			t.Errorf("Expecting status: %d got: %v", 201, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 201,
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceBodyMatch(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		fmt.Fprintln(w, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			e := "Body regex match: 0BC20225-2E72-4646-9202-8467972199E1"
			if b != e {
				t.Errorf("Expecting: %q, got: %q", e, b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	re := regexp.MustCompile(`(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}`)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Body: "(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}",
			body: re,
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceBodyNoMatch(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		fmt.Fprintln(w, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			e := "Body no regex match: [a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}"
			if b != e {
				t.Errorf("Expecting: %q, got: %q", e, b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 1 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check output
		if o, ok := i["output"]; ok {
			e := "No defined cmd"
			if o != e {
				t.Errorf("Expecting %q, got %q", e, o)
			}
		} else {
			t.Errorf("key not found: %q", "output")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	re := regexp.MustCompile(`[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}`)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Body: "[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}",
			body: re,
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceNoGet(t *testing.T) {
	var wg sync.WaitGroup
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if i["name"] != "s 1" {
			t.Errorf("Expecting  %q, got: %q", "s 1", i["name"])
		}
		// check because
		e := "GET: http: no Host in request URL"
		if i["because"] != e {
			t.Errorf("Expecting: %q, got: %q", e, i["because"])
		}
		// check exit
		if i["exit"].(float64) != 1 {
			t.Errorf("Expecting: 1 got: %v", i["exit"].(float64))
		}
		// check output
		e = "exit status 1"
		if i["output"] != e {
			t.Errorf("Expecting %q, got %v", e, i["oputput"])
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		if i["status"].(float64) != 0 {
			t.Errorf("Expecting status: %d got: %v", 0, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  "http://",
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			IfNot: Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceNoGetStatus0(t *testing.T) {
	var wg sync.WaitGroup
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			e := "GET: http: no Host in request URL"
			if b != e {
				t.Errorf("Expecting: %q, got: %q", e, b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 1 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check output
		if o, ok := i["output"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		if i["status"].(float64) != 0 {
			t.Errorf("Expecting status: %d got: %v", 0, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  "http://",
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			IfNot: Action{
				Cmd: "test 3 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceIfStatusMatch502(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		http.Error(w, http.StatusText(502), 502)
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 502" {
				t.Errorf("Expecting: %q, got: %q", "Status: 502", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		e := "No defined cmd"
		if i["output"] != e {
			t.Errorf("Expecting %q, got %q", e, i["output"])
		}
		if i["status"].(float64) != 502 {
			t.Errorf("Expecting status: %d got: %v", 502, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			IfNot:  Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			502: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{
				Notify: "yes",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceIfStatusNoMatch(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		http.Error(w, http.StatusText(505), 505)
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 505" {
				t.Errorf("Expecting: %q, got: %q", "Status: 505", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		e := "No defined cmd"
		if i["output"] != e {
			t.Errorf("Expecting %q, got %q", e, i["output"])
		}
		if i["status"].(float64) != 505 {
			t.Errorf("Expecting status: %d got: %v", 505, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			IfNot:  Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			502: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"x-db-kapputt":   Action{},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceIfHeaderMatch(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.Header().Set("x-db-kapputt", "si si si")
		fmt.Fprintln(w, "Hello")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Header: X-Db-Kapputt" {
				t.Errorf("Expecting: %q, got: %q", "Header: x-db-kapputt", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		e := "exit status 1"
		if i["output"] != e {
			t.Errorf("Expecting %q, got %q", e, i["output"])
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			IfNot:  Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"X-Db-Kapputt": Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceStatus202(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.WriteHeader(http.StatusAccepted)
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 202" {
				t.Errorf("Expecting: %q, got: %q", "Status: 202", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if o, ok := i["output"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		if i["status"].(float64) != 202 {
			t.Errorf("Expecting status: %d got: %v", 202, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 202,
			IfNot:  Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"X-Db-Kapputt": Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceMissingHeader(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.Header().Set("X-Abc", "xyz")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Header: test: xxx" {
				t.Errorf("Expecting: %q, got: %q", "Header: test: xxx", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 1 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if o, ok := i["output"]; ok {
			e := "No defined cmd"
			if o != e {
				t.Errorf("Expecting %q, got %q", e, o)
			}
		} else {
			t.Errorf("key not found: %q", "output")
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			Header: map[string]string{
				"test":  "xxx",
				"X-Abc": "xyz",
			},
			IfNot: Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"X-Db-Kapputt": Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceMatchingHeader(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.Header().Set("X-Abc", "xyz")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 200" {
				t.Errorf("Expecting: %q, got: %q", "Status: 200", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if o, ok := i["output"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			Header: map[string]string{
				"X-Abc": "xyz",
			},
			IfNot: Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"X-Db-Kapputt": Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceMatchingHeaderPrefix(t *testing.T) {
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.Header().Set("content-type", "application/json; charset=UTF-8")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 200" {
				t.Errorf("Expecting: %q, got: %q", "Status: 200", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if o, ok := i["output"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			Header: map[string]string{
				"content-type": "application/json",
			},
			IfNot: Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"X-Db-Kapputt": Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
}

func TestSuperviceLogErr(t *testing.T) {
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  "--",
		Log:  "http://",
		Expect: Expect{
			Status: 200,
		},
	}
	ez := new(Epazote)
	ser := *s["s 1"]
	ez.Log(&ser, []byte{0})

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
}

func TestSuperviceMatchingHeaderDebugGreen(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.Header().Set("X-Abc", "xyz")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 200" {
				t.Errorf("Expecting: %q, got: %q", "Status: 200", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 0 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if o, ok := i["output"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 200,
			Header: map[string]string{
				"X-Abc": "xyz",
			},
			IfNot: Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"X-Db-Kapputt": Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
		debug:    true,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
}

func TestSuperviceMatchingHeaderDebugRed(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.Header().Set("X-Abc", "xyz")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 200" {
				t.Errorf("Expecting: %q, got: %q", "Status: 200", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 1 {
				t.Errorf("Expecting: 1 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		e := "No defined cmd"
		if i["output"] != e {
			t.Errorf("Expecting %q, got %q", e, i["output"])
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 300,
			Header: map[string]string{
				"X-Abc": "xyz",
			},
			IfNot: Action{},
		},
		IfStatus: map[int]Action{
			501: Action{},
			503: Action{},
		},
		IfHeader: map[string]Action{
			"x-amqp-kapputt": Action{Notify: "yes"},
			"X-Db-Kapputt": Action{
				Cmd: "test 1 -gt 2",
			},
		},
	}
	ez := &Epazote{
		Services: s,
		debug:    true,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
}

func TestSupervice302(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		w.WriteHeader(http.StatusFound)
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		if b, ok := i["because"]; ok {
			if b != "Status: 302" {
				t.Errorf("Expecting: %q, got: %q", "Status: 302", b)
			}
		} else {
			t.Errorf("key not found: %q", "because")
		}
		// check exit
		if e, ok := i["exit"]; ok {
			if e.(float64) != 0 {
				t.Errorf("Expecting: 1 got: %v", e.(float64))
			}
		} else {
			t.Errorf("key not found: %q", "exit")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if o, ok := i["output"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		if i["status"].(float64) != 302 {
			t.Errorf("Expecting status: %d got: %v", 302, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name: "s 1",
		URL:  check_s.URL,
		Log:  log_s.URL,
		Expect: Expect{
			Status: 302,
			IfNot:  Action{},
		},
		IfStatus: map[int]Action{
			200: Action{},
		},
	}
	ez := &Epazote{
		Services: s,
		debug:    true,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
}

func TestSuperviceFollow(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	check_end := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex")
	}))
	defer check_end.Close()
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		http.Redirect(w, r, check_end.URL, http.StatusFound)
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check name
		if n, ok := i["name"]; ok {
			if n != "s 1" {
				t.Errorf("Expecting  %q, got: %q", "s 1", n)
			}
		} else {
			t.Errorf("key not found: %q", "name")
		}
		// check because
		e := "Body regex match: 0BC20225-2E72-4646-9202-8467972199E1"
		if i["because"] != e {
			t.Errorf("Expecting: %q, got: %v", e, i["because"])
		}
		// check exit
		if i["exit"].(float64) != 0 {
			t.Errorf("Expecting: 0 got: %v", i["exit"])
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if o, ok := i["output"]; ok {
			t.Errorf("key should not exist,content: %q", o)
		}
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	re := regexp.MustCompile(`(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}`)
	s["s 1"] = &Service{
		Name:   "s 1",
		URL:    check_s.URL,
		Follow: true,
		Log:    log_s.URL,
		Expect: Expect{
			Status: 200,
			Body:   "(?i)[a-z0-9]{8}-[a-z0-9]{4}-[1-5][a-z0-9]{3}-[a-z0-9]{4}-[a-z0-9]{12}",
			body:   re,
		},
		IfStatus: map[int]Action{
			302: Action{},
		},
	}
	ez := &Epazote{
		Services: s,
		debug:    true,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}
}

func TestSuperviceSkipCmd(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "Hello, epazote match 0BC20225-2E72-4646-9202-8467972199E1 regex")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:   "s 1",
		URL:    check_s.URL,
		Follow: true,
		Log:    log_s.URL,
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

	if buf.Len() == 0 {
		t.Error("Expecting log.Println error")
	}

	if s["s 1"].status != 2 {
		t.Errorf("Expecting status == 2 got: %v", s["s 1"].status)
	}

	s["s 1"].status = 0
	s["s 1"].Expect.Status = 200
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()

	if s["s 1"].status != 0 {
		t.Errorf("Expecting status == 0 got: %v", s["s 1"].status)
	}
}

func TestSuperviceCount1000(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	check_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer check_s.Close()
	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:   "s 1",
		URL:    check_s.URL,
		Follow: true,
		Log:    log_s.URL,
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
	if s["s 1"].status != 1000 {
		t.Errorf("Expecting status: 1000 got: %v", s["s 1"].status)
	}
}

// server.CloseClientConnections not workng on golang 1.6
func TestSuperviceRetrie(t *testing.T) {
	buf.Reset()
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

	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check exit
		if i["exit"].(float64) != 0 {
			t.Errorf("Expecting: 0 got: %v", i["exit"])
		}
		// check because
		e := "Body regex match: molcajete"
		if i["because"] != e {
			t.Errorf("Expecting: %q, got: %v", e, i["because"])
		}
		// check retries
		if i["retries"].(float64) != 2 {
			t.Errorf("Expecting: 2 got: %v", i["retries"])
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	re := regexp.MustCompile(`molcajete`)
	s["s 1"] = &Service{
		Name:       "s 1",
		URL:        server.URL,
		RetryLimit: 3,
		ReadLimit:  17,
		Log:        log_s.URL,
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
	rc := s["s 1"].retryCount
	if rc != 2 {
		t.Errorf("Expecting retryCount = 2 got: %d", rc)
	}
	if counter != 3 {
		t.Errorf("Expecting 3 got: %v", counter)
	}
}

func TestSuperviceRetrieLimit(t *testing.T) {
	buf.Reset()
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

	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check exit
		if i["exit"].(float64) != 1 {
			t.Errorf("Expecting: 1 got: %v", i["exit"])
		}
		// check retries
		if i["retries"].(float64) != 4 {
			t.Errorf("Expecting: 4 got: %v", i["retries"])
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if i["status"].(float64) != 0 {
			t.Errorf("Expecting status: %d got: %v", 0, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:          "s 1",
		URL:           server.URL,
		RetryLimit:    5,
		RetryInterval: 1,
		Log:           log_s.URL,
		Expect: Expect{
			Status: 200,
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
	rc := s["s 1"].retryCount
	if rc != 4 {
		t.Errorf("Expecting retryCount = 4 got: %d", rc)
	}
}

func TestSuperviceRetrieLimit0(t *testing.T) {
	buf.Reset()
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

	log_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		decoder := json.NewDecoder(r.Body)
		var i map[string]interface{}
		err := decoder.Decode(&i)
		if err != nil {
			t.Error(err)
		}
		// check exit
		if i["exit"].(float64) != 0 {
			t.Errorf("Expecting: 0 got: %v", i["exit"])
		}
		// check retries
		if _, ok := i["retries"]; ok {
			t.Errorf("retries key found")
		}
		// check url
		if _, ok := i["url"]; !ok {
			t.Error("URL key not found")
		}
		// check output
		if i["status"].(float64) != 200 {
			t.Errorf("Expecting status: %d got: %v", 200, i["status"])
		}
		wg.Done()
	}))
	defer log_s.Close()
	s := make(Services)
	s["s 1"] = &Service{
		Name:          "s 1",
		URL:           server.URL,
		RetryLimit:    0,
		RetryInterval: 1,
		ReadLimit:     1024,
		Log:           log_s.URL,
		Expect: Expect{
			Status: 200,
		},
	}
	ez := &Epazote{
		Services: s,
	}
	wg.Add(1)
	ez.Supervice(s["s 1"])()
	wg.Wait()
	rc := s["s 1"].retryCount
	if rc != 0 {
		t.Errorf("Expecting retryCount = 0 got: %d", rc)
	}
}

func TestSuperviceReadLimit(t *testing.T) {
	buf.Reset()
	var server *httptest.Server
	var h http.HandlerFunc = func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, "0123456789")
	}
	server = httptest.NewServer(h)
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
	ez := &Epazote{
		Services: s,
	}
	ez.debug = true
	ez.Supervice(s["s 1"])()
	rc := s["s 1"].retryCount
	if rc != 0 {
		t.Errorf("Expecting retryCount = 0 got: %d", rc)
	}

	data := buf.String()
	re := regexp.MustCompile("(?m)[\r\n]+^01234$")
	match := re.FindString(data)
	if match == "" {
		t.Error("Expecting: 01234")
	}
}
