package epazote

import (
	"net/smtp"
	"strconv"
)

// MailMan to simplify tests
type MailMan interface {
	Send(to []string, body []byte) error
}

// mailMan implements MailMan
type mailMan struct {
	conf *Email
	send func(string, smtp.Auth, string, []string, []byte) error
}

// Send send email
func (m *mailMan) Send(to []string, body []byte) error {
	// x.x.x.x:25
	addr := m.conf.Server + ":" + strconv.Itoa(m.conf.Port)
	// auth Set up authentication information.
	auth := smtp.PlainAuth("",
		m.conf.Username,
		m.conf.Password,
		m.conf.Server,
	)
	return m.send(addr, auth, m.conf.Headers["from"], to, body)
}

// NewMailMan returns mailMan that satisfies MailMan interface
func NewMailMan(conf *Email) MailMan {
	return &mailMan{
		conf,
		smtp.SendMail,
	}
}
