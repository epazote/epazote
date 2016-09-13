package epazote

import (
	"bytes"
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"time"
)

// Do, execute the command in the if_not block
func (e *Epazote) Do(cmd string, skip bool) string {
	if skip {
		return "Skipping cmd"
	}
	if cmd != "" {
		var shell = "sh"
		if sh := os.Getenv("SHELL"); sh != "" {
			shell = sh
		}
		out, err := exec.Command(shell, "-c", cmd).CombinedOutput()
		if err != nil {
			return err.Error()
		}
		return string(out)
	}
	return "No defined cmd"
}

// Supervice check services
func (e *Epazote) Supervice(s *Service) func() {
	return func() {
		defer func() {
			if r := recover(); r != nil {
				log.Printf("Verify service %s options: %q", Red(s.Name), r)
			}
		}()

		// Mailman instance
		m := NewMailMan(&e.Config.SMTP)

		// skip "do cmd", to avoid a loop
		skip := false
		if s.status > s.Stop && s.Stop != -1 {
			skip = true
		}

		// Run Test if no URL
		// execute the Test cmd if exit > 0 execute the if_not cmd
		if s.URL == "" {
			var shell = "sh"
			if sh := os.Getenv("SHELL"); sh != "" {
				shell = sh
			}
			if e.debug {
				log.Printf("Service: %q, SHELL: %q, Test cmd args: %s", shell, s.Name, s.Test.Test)
			}
			cmd := exec.Command(shell, "-c", s.Test.Test)
			var out bytes.Buffer
			cmd.Stdout = &out
			if err := cmd.Run(); err != nil {
				e.Report(m, s, &s.Test.IfNot, nil, 1, 0, fmt.Sprintf("Test cmd: %s", err), e.Do(s.Test.IfNot.Cmd, skip))
				return
			}
			e.Report(m, s, nil, nil, 0, 0, fmt.Sprintf("Test cmd: %s", out.String()), "")
			return
		}

		// HTTP GET service URL, by defaults retries 3 times with intervals of 1 second
		var res *http.Response
		s.retryCount = -1
		err := Try(func(attempt int) (bool, error) {
			var err error
			res, err = HTTPGet(s.URL, s.Follow, s.Insecure, s.Header, s.Timeout)
			if err != nil {
				time.Sleep(time.Duration(s.RetryInterval) * time.Millisecond)
			}
			s.retryCount++
			return attempt < s.RetryLimit, err
		})
		if err != nil {
			e.Report(m, s, &s.Expect.IfNot, res, 1, 0, fmt.Sprintf("GET: %s", err), e.Do(s.Expect.IfNot.Cmd, skip))
			return
		}

		// Read Body first and close if not used
		if s.Expect.Body != "" {
			var body []byte
			var err error
			if s.ReadLimit > 0 {
				body, err = ioutil.ReadAll(io.LimitReader(res.Body, s.ReadLimit))
			} else {
				body, err = ioutil.ReadAll(res.Body)
			}
			res.Body.Close()
			if err != nil {
				log.Printf("Could not read Body for service %q, Error: %s", Red(s.Name), err)
				return
			}
			r := s.Expect.body.FindString(string(body))
			if r == "" {
				e.Report(m, s, &s.Expect.IfNot, res, 1, res.StatusCode, fmt.Sprintf("Body no regex match: %s", s.Expect.body.String()), e.Do(s.Expect.IfNot.Cmd, skip))
				return
			}
			e.Report(m, s, nil, res, 0, res.StatusCode, fmt.Sprintf("Body regex match: %s", r), "")
			return
		} else if s.ReadLimit > 0 {
			chunkedBody, err := ioutil.ReadAll(io.LimitReader(res.Body, s.ReadLimit))
			res.Body.Close()
			if err != nil {
				log.Printf("Could not read Body for service %q, read_limit %d, Error: %s", Red(s.Name), s.ReadLimit, err)
				return
			}
			if e.debug {
				log.Printf("Service %q, read_limit: %d, Body: \n%s", s.Name, s.ReadLimit, chunkedBody)
			}
		} else {
			// close body since will not be used anymore
			res.Body.Close()
		}

		// if_status
		if s.IfStatus != nil {
			// check if there is an Action for the returned StatusCode
			if a, ok := s.IfStatus[res.StatusCode]; ok {
				e.Report(m, s, &a, res, 1, res.StatusCode, fmt.Sprintf("Status: %d", res.StatusCode), e.Do(a.Cmd, skip))
				return
			}
		}

		// if_header
		if s.IfHeader != nil {
			// return if true
			r := false
			for k, a := range s.IfHeader {
				if res.Header.Get(k) != "" {
					r = true
					e.Report(m, s, &a, res, 1, res.StatusCode, fmt.Sprintf("Header: %s", k), e.Do(a.Cmd, skip))
				}
			}
			if r {
				return
			}
		}

		// Status
		if res.StatusCode != s.Expect.Status {
			e.Report(m, s, &s.Expect.IfNot, res, 1, res.StatusCode, fmt.Sprintf("Status: %d", res.StatusCode), e.Do(s.Expect.IfNot.Cmd, skip))
			return
		}

		// Header
		if s.Expect.Header != nil {
			for k, v := range s.Expect.Header {
				if !strings.HasPrefix(res.Header.Get(k), v) {
					e.Report(m, s, &s.Expect.IfNot, res, 1, res.StatusCode, fmt.Sprintf("Header: %s: %s", k, v), e.Do(s.Expect.IfNot.Cmd, skip))
					return
				}
			}
		}

		// SSLExpiry - check SSL certificate expiration date
		if res.TLS != nil {
			// alert 72 hours before by default if no ssl-expiry set
			abce := GetInterval(259200, s.Expect.SSL.Every)
			for _, cert := range res.TLS.PeerCertificates {
				expiresIn := int(cert.NotAfter.Sub(time.Now().UTC()).Seconds())
				if expiresIn <= abce {
					e.Report(m, s, &s.Expect.IfNot, res, 1, res.StatusCode, fmt.Sprintf("cert: %s expires in: %s", cert.Subject.CommonName, cert.NotAfter.Format(time.RFC1123Z)), "")
					return
				}
			}
		}

		// fin if all is ok
		if res.StatusCode == s.Expect.Status {
			e.Report(m, s, nil, res, 0, res.StatusCode, fmt.Sprintf("Status: %d", res.StatusCode), "")
			return
		}
	}
}
