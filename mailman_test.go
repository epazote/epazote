package epazote

import (
	"bytes"
	"encoding/base64"
	"errors"
	"fmt"
	"io/ioutil"
	"log"
	"net/smtp"
	"os"
	"regexp"
	"strings"
	"sync"
	"testing"
)

// emailRecorder for testing
type emailRecorder struct {
	addr string
	auth smtp.Auth
	from string
	to   []string
	msg  []byte
}

// mock smtp.SendMail
func mockSend(errToReturn error, wg *sync.WaitGroup) (func(string, smtp.Auth, string, []string, []byte) error, *emailRecorder) {
	r := &emailRecorder{}
	return func(addr string, a smtp.Auth, from string, to []string, msg []byte) error {
		defer wg.Done()
		*r = emailRecorder{addr, a, from, to, msg}
		return errToReturn
	}, r
}

func TestEmailSendSuccessful(t *testing.T) {
	var wg sync.WaitGroup
	c := &Email{}
	f, r := mockSend(nil, &wg)
	sender := &mailMan{c, f}
	body := "Hello World"
	wg.Add(1)
	err := sender.Send([]string{"me@example.com"}, []byte(body))
	if err != nil {
		t.Errorf("unexpected error: %s", err)
	}
	expect(t, body, string(r.msg))
}

func TestSendEmail(t *testing.T) {
	var wg sync.WaitGroup
	c := &Email{}
	f, r := mockSend(nil, &wg)
	sender := &mailMan{c, f}
	body := "Hello World"
	e := &Epazote{}
	wg.Add(1)
	e.SendEmail(sender, []string{"me@example.com"}, "[name - exit]", []byte(body))

	data, err := base64.StdEncoding.DecodeString(string(r.msg))
	if err != nil {
		t.Error(err)
	}
	expect(t, body, string(data))
}

func TestReport(t *testing.T) {
	type Return struct {
		exitCode, httpStatus int
		because, output      string
	}
	var testTable = []struct {
		a   *Action
		h   map[string]string
		err string
		r   Return
		s   string
	}{
		{
			&Action{Notify: "33test@ejemplo.org", Msg: []string{"OK", "NO OK"}, Emoji: []string{"1f621"}},
			map[string]string{
				"to": "33test@ejemplo.org",
			},
			"",
			Return{1, 200, "because", "output"},
			"Subject: =?UTF-8?b?8J+SqSAgW25hbWUsIGJlY2F1c2Vd?=",
		},
		{
			&Action{Notify: "yes", Msg: []string{"testing notifications"}},
			map[string]string{
				"from":    "epazote@domain.tld",
				"to":      "test@ejemplo.org",
				"subject": "[name: name - exit - url - because]",
			},
			"send email fake error",
			Return{1, 200, "because", "output"},
			"Subject: =?UTF-8?b?8J+SqSAgW25hbWU6IG5hbWUgLSBleGl0IC0gdXJsIC0gYmVjYXVzZV0=?=",
		},
		{
			&Action{Notify: "yes", Msg: []string{"testing notifications"}, Emoji: []string{"0"}},
			map[string]string{
				"from":    "epazote@domain.tld",
				"to":      "test@ejemplo.org",
				"subject": "[_name_, _because_]",
			},
			"send email fake error",
			Return{1, 200, "because", "output"},
			"Subject: [s 1, because]",
		},
	}
	var wg sync.WaitGroup
	for _, tt := range testTable {
		var err error
		tmpfile, err := ioutil.TempFile("", "TestReport")
		if err != nil {
			t.Error(err)
		}
		log.SetOutput(tmpfile)
		log.SetFlags(0)
		c := Email{"username", "password", "server", 587, tt.h, true}
		e := &Epazote{}
		e.Config.SMTP = c
		e.VerifyEmail()
		if tt.err == "" {
			err = nil
		} else {
			err = errors.New(tt.err)
		}
		f, r := mockSend(err, &wg)
		sender := &mailMan{&c, f}
		service := &Service{
			Name: "s 1",
			URL:  "http://about.epazote.io",
			Expect: Expect{
				Status: 200,
			},
		}
		wg.Add(1)
		e.Report(sender, service, tt.a, nil, tt.r.exitCode, tt.r.httpStatus, tt.r.because, tt.r.output)
		wg.Wait()
		expect(t, "server:587", r.addr)
		expect(t, tt.h["from"], r.from)
		expect(t, tt.h["to"], r.to[0])

		crlf := []byte("\r\n\r\n")
		index := bytes.Index(r.msg, crlf)
		data := r.msg[index+len(crlf):]
		data, err = base64.StdEncoding.DecodeString(string(data))
		if err != nil {
			t.Error(err)
		}
		if len(tt.err) > 0 {
			b, _ := ioutil.ReadFile(tmpfile.Name())
			expect(t, fmt.Sprintf("ERROR attempting to send email: %q", strings.TrimSpace(tt.err)), strings.TrimSpace(string(b)))
		}

		re := regexp.MustCompile(`Subject.*`)
		match := re.FindString(string(r.msg))
		expect(t, tt.s, strings.TrimSpace(match))
		// TODO

		os.Remove(tmpfile.Name())
	}
}

/*
func TestReportCustomEmoji(t *testing.T) {
	var wg sync.WaitGroup
	buf.Reset()
	headers := map[string]string{
		"from":    "epazote@domain.tld",
		"to":      "test@ejemplo.org",
		"subject": "[_name_, _because_]",
	}
	c := Email{"username", "password", "server", 587, headers, true}
	f, r := mockSend(errors.New("I love errors"), &wg)
	sender := &mailMan{&c, f}
	ss := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{Notify: "yes", Msg: []string{"testing notifications"}, Emoji: []string{"1F621"}}
	e := &Epazote{}
	e.Config.SMTP = c

	wg.Add(1)
	e.Report(sender, ss, a, nil, 0, 200, "because", "output")
	wg.Wait()

	if r.addr != "server:587" {
		t.Errorf("Expecting %q got %q", "server:587", r.addr)
	}
	if r.from != "epazote@domain.tld" {
		t.Errorf("Expecting %q got %q", "epazote@domain.tld", r.from)
	}
	if r.to[0] != "test@ejemplo.org" {
		t.Errorf("Expecting %q got %q", "test@ejemplo.org", r.to[0])
	}

	crlf := []byte("\r\n\r\n")
	index := bytes.Index(r.msg, crlf)

	data := r.msg[index+len(crlf):]

	data, err := base64.StdEncoding.DecodeString(string(data))
	if err != nil {
		t.Error(err)
	}

	if buf.Len() < 10 {
		t.Errorf("buf len not matching, got: %q", buf)
	}

	re := regexp.MustCompile(`Subject.*`)
	match := re.FindString(string(r.msg))
	u := strings.Split(match, ": ")
	u[1] = strings.TrimSpace(u[1])
	dec := new(mime.WordDecoder)
	header, err := dec.DecodeHeader(u[1])
	if err != nil {
		t.Error(err)
	}
	x := fmt.Sprintf("%x", header)
	if x != "f09f98a120205b7320312c20626563617573655d" {
		t.Error(x)
	}
}

func TestReportEmoji0(t *testing.T) {
	var wg sync.WaitGroup
	buf.Reset()
	headers := map[string]string{
		"from":    "epazote@domain.tld",
		"to":      "test@ejemplo.org",
		"subject": "[_name_, _because_]",
	}
	c := Email{"username", "password", "server", 587, headers, true}
	f, r := mockSend(errors.New("I love errors"), &wg)
	sender := &mailMan{&c, f}
	ss := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{Notify: "yes", Msg: []string{"testing notifications"}, Emoji: []string{"1f44c", "1f44e"}}
	e := &Epazote{}
	e.Config.SMTP = c

	wg.Add(1)
	e.Report(sender, ss, a, nil, 0, 200, "because", "output")
	wg.Wait()

	if r.addr != "server:587" {
		t.Errorf("Expecting %q got %q", "server:587", r.addr)
	}
	if r.from != "epazote@domain.tld" {
		t.Errorf("Expecting %q got %q", "epazote@domain.tld", r.from)
	}
	if r.to[0] != "test@ejemplo.org" {
		t.Errorf("Expecting %q got %q", "test@ejemplo.org", r.to[0])
	}

	crlf := []byte("\r\n\r\n")
	index := bytes.Index(r.msg, crlf)

	data := r.msg[index+len(crlf):]

	data, err := base64.StdEncoding.DecodeString(string(data))
	if err != nil {
		t.Error(err)
	}

	if buf.Len() < 10 {
		t.Errorf("buf len not matching, got: %q", buf)
	}

	re := regexp.MustCompile(`Subject.*`)
	match := re.FindString(string(r.msg))
	u := strings.Split(match, ": ")
	u[1] = strings.TrimSpace(u[1])
	dec := new(mime.WordDecoder)
	header, err := dec.DecodeHeader(u[1])
	if err != nil {
		t.Error(err)
	}
	x := fmt.Sprintf("%x", header)
	if x != "f09f918c20205b7320312c20626563617573655d" {
		t.Errorf("Expecting: %s, Got: %s", "f09f918c20205b7320312c20626563617573655d", x)
	}

}

func TestReportEmoji1(t *testing.T) {
	var wg sync.WaitGroup
	buf.Reset()
	headers := map[string]string{
		"from":    "epazote@domain.tld",
		"to":      "test@ejemplo.org",
		"subject": "[_name_, _because_]",
	}
	c := Email{"username", "password", "server", 587, headers, true}
	f, r := mockSend(errors.New("I love errors"), &wg)
	sender := &mailMan{&c, f}
	ss := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{Notify: "yes", Msg: []string{"testing notifications"}, Emoji: []string{"1f44c", "1f44e"}}
	e := &Epazote{}
	e.Config.SMTP = c

	wg.Add(1)
	e.Report(sender, ss, a, nil, 1, 200, "because", "output")
	wg.Wait()

	if r.addr != "server:587" {
		t.Errorf("Expecting %q got %q", "server:587", r.addr)
	}
	if r.from != "epazote@domain.tld" {
		t.Errorf("Expecting %q got %q", "epazote@domain.tld", r.from)
	}
	if r.to[0] != "test@ejemplo.org" {
		t.Errorf("Expecting %q got %q", "test@ejemplo.org", r.to[0])
	}

	crlf := []byte("\r\n\r\n")
	index := bytes.Index(r.msg, crlf)

	data := r.msg[index+len(crlf):]

	data, err := base64.StdEncoding.DecodeString(string(data))
	if err != nil {
		t.Error(err)
	}

	if buf.Len() < 10 {
		t.Errorf("buf len not matching, got: %q", buf)
	}

	re := regexp.MustCompile(`Subject.*`)
	match := re.FindString(string(r.msg))
	u := strings.Split(match, ": ")
	u[1] = strings.TrimSpace(u[1])
	dec := new(mime.WordDecoder)
	header, err := dec.DecodeHeader(u[1])
	if err != nil {
		t.Error(err)
	}
	x := fmt.Sprintf("%x", header)
	if x != "f09f918e20205b7320312c20626563617573655d" {
		t.Errorf("Expecting: %s, Got: %s", "f09f918e20205b7320312c20626563617573655d", x)
	}
}

func TestReportMsg0(t *testing.T) {
	var wg sync.WaitGroup
	buf.Reset()
	headers := map[string]string{
		"from":    "epazote@domain.tld",
		"to":      "test@ejemplo.org",
		"subject": "[_name_, _because_]",
	}
	c := Email{"username", "password", "server", 587, headers, true}
	f, r := mockSend(errors.New("I love errors"), &wg)
	sender := &mailMan{&c, f}
	ss := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{Notify: "yes", Msg: []string{"msg-1", "msg-2"}}
	e := &Epazote{}
	e.Config.SMTP = c

	wg.Add(1)
	e.Report(sender, ss, a, nil, 0, 200, "because", "output")
	wg.Wait()

	if r.addr != "server:587" {
		t.Errorf("Expecting %q got %q", "server:587", r.addr)
	}
	if r.from != "epazote@domain.tld" {
		t.Errorf("Expecting %q got %q", "epazote@domain.tld", r.from)
	}
	if r.to[0] != "test@ejemplo.org" {
		t.Errorf("Expecting %q got %q", "test@ejemplo.org", r.to[0])
	}

	crlf := []byte("\r\n\r\n")
	index := bytes.Index(r.msg, crlf)

	data := r.msg[index+len(crlf):]

	data, err := base64.StdEncoding.DecodeString(string(data))
	if err != nil {
		t.Error(err)
	}
	index = bytes.Index(data, crlf)
	if "msg-1" != strings.TrimSpace(string(data[:index])) {
		t.Errorf("Expecfing: msg-2 Got: %s", string(data[:index]))
	}
}

func TestReportMsg1(t *testing.T) {
	var wg sync.WaitGroup
	buf.Reset()
	headers := map[string]string{
		"from":    "epazote@domain.tld",
		"to":      "test@ejemplo.org",
		"subject": "[name, because]",
	}
	c := Email{"username", "password", "server", 587, headers, true}
	f, r := mockSend(errors.New("I love errors"), &wg)
	sender := &mailMan{&c, f}
	ss := &Service{
		Name: "s 1",
		URL:  "http://about.epazote.io",
		Expect: Expect{
			Status: 200,
		},
	}
	a := &Action{Notify: "yes", Msg: []string{"msg-1", "msg-2"}}
	e := &Epazote{}
	e.Config.SMTP = c

	wg.Add(1)
	e.Report(sender, ss, a, nil, 1, 200, "because", "output")
	wg.Wait()

	if r.addr != "server:587" {
		t.Errorf("Expecting %q got %q", "server:587", r.addr)
	}
	if r.from != "epazote@domain.tld" {
		t.Errorf("Expecting %q got %q", "epazote@domain.tld", r.from)
	}
	if r.to[0] != "test@ejemplo.org" {
		t.Errorf("Expecting %q got %q", "test@ejemplo.org", r.to[0])
	}

	crlf := []byte("\r\n\r\n")
	index := bytes.Index(r.msg, crlf)

	data := r.msg[index+len(crlf):]

	data, err := base64.StdEncoding.DecodeString(string(data))
	if err != nil {
		t.Error(err)
	}
	index = bytes.Index(data, crlf)
	if "msg-2" != strings.TrimSpace(string(data[:index])) {
		t.Errorf("Expecfing: msg-2 Got: %s", string(data[:index]))
	}
}
*/
