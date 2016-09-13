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
			HTTP{
				URL: fmt.Sprintf("%s/?param=list", server.URL),
			},
		},
	}
	ez := &Epazote{}
	wg.Add(1)
	ez.Report(nil, &Service{}, a, nil, 1, 200, "because", "output")
	wg.Wait()
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
			HTTP{
				URL: fmt.Sprintf("%s/?exit=_exit_&status=_status_", server.URL),
			},
			HTTP{
				URL: fmt.Sprintf("%s/?exit=_exit_", server.URL),
			},
		},
	}
	ez := &Epazote{}
	ez.debug = true
	wg.Add(1)
	ez.Report(nil, &Service{}, a, nil, 0, 200, "because", "output")
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
			HTTP{
				URL: fmt.Sprintf("%s/?exit=0", server.URL),
			},
			HTTP{
				URL: fmt.Sprintf("%s/?exit=1", server.URL),
			},
		},
	}
	ez := &Epazote{}
	ez.debug = true
	wg.Add(1)
	ez.Report(nil, &Service{}, a, nil, 1, 200, "because", "output")
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
			HTTP{
				URL:    server.URL,
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
	ez.Report(nil, &Service{}, a, nil, 1, 200, "because", "output")
	wg.Wait()
}
