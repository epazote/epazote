package epazote

import (
	"encoding/json"
	"fmt"
	"io/ioutil"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func TestHTTPGet(t *testing.T) {
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()
	res, err := HTTPGet(ts.URL, false, true, nil, 3)
	if err != nil {
		t.Error(err)
	}
	body, err := ioutil.ReadAll(res.Body)
	res.Body.Close()
	if err != nil {
		t.Error(err)
	}
	expect(t, "Hello, epazote\n", string(body))
	expect(t, 200, res.StatusCode)

}

func TestHTTPPost(t *testing.T) {
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		expect(t, "application/json", r.Header.Get("Content-Type"))
		decoder := json.NewDecoder(r.Body)
		var d struct{ Exit int }
		err := decoder.Decode(&d)
		if err != nil {
			t.Error(err)
		}
		expect(t, 0, d.Exit)
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()
	_, err := HTTPPost(ts.URL, []byte(`{"exit":0}`), nil)
	if err != nil {
		t.Error(err)
	}
}

func TestHTTPPostBadURL(t *testing.T) {
	_, err := HTTPPost("abc", []byte(`{"exit":0}`), nil)
	if err == nil {
		t.Error(err)
	}
}

func TestAsyngGet(t *testing.T) {
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()
	s := make(Services)
	s["s 1"] = &Service{
		URL: ts.URL,
	}
	ch := AsyncGet(&s)
	for i := 0; i < len(s); i++ {
		x := <-ch
		if x.Err != nil {
			t.Error(x.Err)
		}
	}
}

func TestIsURL(t *testing.T) {
	t.Parallel()

	var tests = []struct {
		param    string
		expected bool
	}{
		{"", false},
		{"http://foo.bar#com", true},
		{"http://foobar.com", true},
		{"https://foobar.com", true},
		{"foobar.com", true},
		{"http://foobar.coffee/", true},
		{"http://foobar.中文网/", true},
		{"http://foobar.org/", true},
		{"http://foobar.ORG", true},
		{"http://foobar.org:8080/", true},
		{"ftp://foobar.ru/", true},
		{"ftp.foo.bar", true},
		{"http://user:pass@www.foobar.com/", true},
		{"http://user:pass@www.foobar.com/path/file", true},
		{"http://127.0.0.1/", true},
		{"http://duckduckgo.com/?q=%2F", true},
		{"http://localhost:3000/", true},
		{"http://foobar.com/?foo=bar#baz=qux", true},
		{"http://foobar.com?foo=bar", true},
		{"http://www.xn--froschgrn-x9a.net/", true},
		{"http://foobar.com/a-", true},
		{"http://foobar.پاکستان/", true},
		{"http://foobar.c_o_m", false},
		{"", false},
		{"xyz://foobar.com", false},
		{"invalid.", false},
		{".com", false},
		{"rtmp://foobar.com", false},
		{"http://www.foo_bar.com/", false},
		{"http://localhost:3000/", true},
		{"http://foobar.com#baz=qux", true},
		{"http://foobar.com/t$-_.+!*\\'(),", true},
		{"http://www.foobar.com/~foobar", true},
		{"http://www.-foobar.com/", false},
		{"http://www.foo---bar.com/", false},
		{"http://r6---snnvoxuioq6.googlevideo.com", true},
		{"mailto:someone@example.com", true},
		{"irc://irc.server.org/channel", false},
		{"irc://#channel@network", true},
		{"/abs/test/dir", false},
		{"./rel/test/dir", false},
		{"http://foo^bar.org", false},
		{"http://foo&*bar.org", false},
		{"http://foo&bar.org", false},
		{"http://foo bar.org", false},
		{"http://foo.bar.org", true},
		{"http://www.foo.bar.org", true},
		{"http://www.foo.co.uk", true},
		{"foo", false},
		{"http://.foo.com", false},
		{"http://,foo.com", false},
		{",foo.com", false},
		// according to issues #62 #66
		{"https://pbs.twimg.com/profile_images/560826135676588032/j8fWrmYY_normal.jpeg", true},
		// according to #125
		{"http://prometheus-alertmanager.service.q:9093", true},
		{"https://www.logn-123-123.url.with.sigle.letter.d:12345/url/path/foo?bar=zzz#user", true},
		{"http://me.example.com", true},
		{"http://www.me.example.com", true},
		{"https://farm6.static.flickr.com", true},
		{"https://zh.wikipedia.org/wiki/Wikipedia:%E9%A6%96%E9%A1%B5", true},
		{"google", false},
		// According to #87
		{"http://hyphenated-host-name.example.co.in", true},
		{"http://cant-end-with-hyphen-.example.com", false},
		{"http://-cant-start-with-hyphen.example.com", false},
		{"http://www.domain-can-have-dashes.com", true},
		// url.Parse
		{"%//a/b/c/d;p?q#", false},
	}
	for _, test := range tests {
		actual := IsURL(test.param)
		if actual != test.expected {
			t.Errorf("Expected IsURL(%q) to be %v, got %v", test.param, test.expected, actual)
		}
	}
}

func TestHTTPGetTimeout(t *testing.T) {
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		time.Sleep(2 * time.Second)
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()

	_, err := HTTPGet(ts.URL, true, true, nil, 1)
	if err == nil {
		t.Errorf("Expecting: %s", "(Client.Timeout exceeded while awaiting headers)")
	}
}

func TestHTTPGetTimeoutNoFollow(t *testing.T) {
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		time.Sleep(2 * time.Second)
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()

	_, err := HTTPGet(ts.URL, false, true, nil, 1)
	if err == nil {
		t.Errorf("Expecting: %s", "(Client.Timeout exceeded while awaiting headers)")
	}
}

func TestHTTPGetInsecure(t *testing.T) {
	ts := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()

	_, err := HTTPGet(ts.URL, false, true, nil)
	if err != nil {
		t.Error(err)
	}
}

func TestHTTPGetInsecureVerify(t *testing.T) {
	ts := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "epazote", r.Header.Get("User-agent"))
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()

	_, err := HTTPGet(ts.URL, false, false, nil)
	if err == nil {
		t.Errorf("Expecting: %s", "x509: certificate signed by unknown authority")
	}
}

func TestHTTPGetCustomHeaders(t *testing.T) {
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		expect(t, "my-UA", r.Header.Get("User-agent"))
		expect(t, "gzip", r.Header.Get("Accept-Encoding"))
		expect(t, "http://localhost", r.Header.Get("Origin"))
		fmt.Fprintln(w, "Hello, epazote")
	}))
	defer ts.Close()

	h := make(map[string]string)
	h["User-Agent"] = "my-UA"
	h["Origin"] = "http://localhost"
	h["Accept-Encoding"] = "gzip"
	_, err := HTTPGet(ts.URL, false, false, h)
	if err != nil {
		t.Error(err)
	}
}
