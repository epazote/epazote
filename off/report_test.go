package epazote

import (
	"fmt"
	"io/ioutil"
	"net/http"
	"net/http/httptest"
	"net/url"
	"sync"
	"testing"
)

func TestReportHTTPGet(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	custom_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		if r.Method != "GET" {
			t.Errorf("Expecting Method GET got: %s", r.Method)
		}
		if r.FormValue("param") != "list" {
			t.Errorf("Expecting param = list got: %s", r.FormValue("param"))
		}
		wg.Done()
	}))
	defer custom_s.Close()

	s := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{
		HTTP: []HTTP{
			HTTP{
				URL: fmt.Sprintf("%s/?param=list", custom_s.URL),
			},
		},
	}
	ez := &Epazote{}
	wg.Add(1)
	ez.Report(nil, s, a, nil, 1, 200, "because", "output")
	wg.Wait()
}

func TestReportHTTP0(t *testing.T) {
	buf.Reset()
	s := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{
		HTTP: []HTTP{
			HTTP{
				URL: "http://no-call",
			},
		},
	}
	ez := &Epazote{}
	ez.debug = true
	ez.Report(nil, s, a, nil, 0, 200, "because", "output")
}

func TestReportHTTP001(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	custom_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		if r.Method != "GET" {
			t.Errorf("Expecting Method GET got: %s", r.Method)
		}
		if r.FormValue("exit") != "0" {
			t.Errorf("Expecting exit = 0 got: %s", r.FormValue("exit"))
		}
		if r.FormValue("status") != "200" {
			t.Errorf("Expecting status = 200 got: %s", r.FormValue("status"))
		}
		wg.Done()
	}))
	defer custom_s.Close()

	s := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{
		HTTP: []HTTP{
			HTTP{
				URL: fmt.Sprintf("%s/?exit=_exit_&status=_status_", custom_s.URL),
			},
			HTTP{
				URL: fmt.Sprintf("%s/?exit=_exit_", custom_s.URL),
			},
		},
	}
	ez := &Epazote{}
	ez.debug = true
	wg.Add(1)
	ez.Report(nil, s, a, nil, 0, 200, "because", "output")
	wg.Wait()
}

func TestReportHTTP011(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	custom_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		if r.Method != "GET" {
			t.Errorf("Expecting Method GET got: %s", r.Method)
		}
		if r.FormValue("exit") != "1" {
			t.Errorf("Expecting exit = 1 got: %s", r.FormValue("exit"))
		}
		wg.Done()
	}))
	defer custom_s.Close()

	s := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{
		HTTP: []HTTP{
			HTTP{
				URL: fmt.Sprintf("%s/?exit=0", custom_s.URL),
			},
			HTTP{
				URL: fmt.Sprintf("%s/?exit=1", custom_s.URL),
			},
		},
	}
	ez := &Epazote{}
	ez.debug = true
	wg.Add(1)
	ez.Report(nil, s, a, nil, 1, 200, "because", "output")
	wg.Wait()
}

func TestReportHTTPPost(t *testing.T) {
	buf.Reset()
	var wg sync.WaitGroup
	custom_s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("User-agent") != "epazote" {
			t.Error("Expecting User-agent: epazote")
		}
		if r.Header.Get("Content-Type") != "application/x-www-form-urlencoded" {
			t.Error("Expecting Content-Type: application/x-www-form-urlencoded")
		}
		if r.Method != "POST" {
			t.Errorf("Expecting Method POST got: %s", r.Method)
		}

		body, _ := ioutil.ReadAll(r.Body)
		values, _ := url.ParseQuery(string(body))

		var expected = []struct {
			key, val string
		}{
			{"room_id", "10"},
			{"from", "Alerts"},
			{"message", "A new user signed up"},
			{"status", "200"},
		}

		for _, v := range expected {
			got := values[v.key]
			if got[0] != v.val {
				t.Errorf("Expecting %s got: %s", got[0], v.val)
			}
		}

		wg.Done()
	}))
	defer custom_s.Close()

	s := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{
		HTTP: []HTTP{
			HTTP{
				URL:    custom_s.URL,
				Method: "post",
				Data:   "room_id=10&from=Alerts&message=A+new+user+signed+up&status=_status_",
				Header: map[string]string{
					"Content-Type": "application/x-www-form-urlencoded",
				},
			},
		},
	}
	ez := &Epazote{}
	ez.debug = true
	wg.Add(1)
	ez.Report(nil, s, a, nil, 1, 200, "because", "output")
	wg.Wait()
}

func TestReportHTTPNoURL(t *testing.T) {
	buf.Reset()
	s := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{
		HTTP: []HTTP{
			HTTP{
				Method: "PUT",
			},
		},
	}
	ez := &Epazote{}
	ez.Report(nil, s, a, nil, 1, 200, "because", "output")
}
