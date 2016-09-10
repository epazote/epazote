package epazote

import (
	"strings"
	"testing"
)

func TestVerifyBadEmail(t *testing.T) {
	cfg, err := New("test/epazote-bad-email.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	if err == nil {
		t.Error("Expecting error")
	}
	e := Red(`Verify notify email addresses for service: service 1 - "mail: missing phrase"`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmail(t *testing.T) {
	cfg, err := New("test/epazote-email.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	if err != nil {
		t.Errorf("Expecting error: %s", err)
	}
}

func TestVerifyEmailNoTo(t *testing.T) {
	cfg, err := New("test/epazote-email-noto.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Service "service 1" need smtp/headers/to settings to be available to notify.`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailNoServer(t *testing.T) {
	cfg, err := New("test/epazote-email-noserver.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`SMTP server required for been available to send email notifications.`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailIfStatus(t *testing.T) {
	cfg, err := New("test/epazote-email-ifstatus.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Verify notify email addresses for service ["service 1" if_status: 502]: "mail: missing phrase"`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailIfStatusYes(t *testing.T) {
	cfg, err := New("test/epazote-email-ifstatus-yes.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Service ["service 1" - 502] need smtp/headers/to settings to be available to notify.`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailIfHeader(t *testing.T) {
	cfg, err := New("test/epazote-email-ifheader.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Verify notify email addresses for service ["service 1" if_header: x-xyz-kaputt]: "mail: missing phrase"`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailIfHeaderYes(t *testing.T) {
	cfg, err := New("test/epazote-email-ifheader-yes.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Service ["service 1" - x-xyz-kaputt] need smtp/headers/to settings to be available to notify.`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailTest(t *testing.T) {
	cfg, err := New("test/epazote-email-test.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Verify notify email addresses for service: service X - "mail: missing phrase"`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailTestYes(t *testing.T) {
	cfg, err := New("test/epazote-email-test-yes.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Service "service X" need smtp/headers/to settings to be available to notify.`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailServer(t *testing.T) {
	cfg, err := New("test/epazote-email-server.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`SMTP server required for been available to send email notifications.`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailServerOk(t *testing.T) {
	cfg, err := New("test/epazote-email-server-ok.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	if err != nil {
		t.Error(err)
	}

	if cfg.Config.SMTP.Headers["from"] != "from@email" {
		t.Error("Expecting from@email")
	}

	if cfg.Services["service X"].IfNot.Notify != "m@tilde t@reco" {
		t.Error("Expecting m@tilde t@reco")
	}
}

func TestVerifyEmailBadTo(t *testing.T) {
	cfg, err := New("test/epazote-email-badto.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	e := Red(`Verify recipient's email address: mail: missing phrase`)
	if err.Error() != e {
		t.Errorf("Expecting %q got %q", e, err.Error())
	}
}

func TestVerifyEmailFrom(t *testing.T) {
	cfg, err := New("test/epazote-email-from.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	if err != nil {
		t.Error(err)
	}
	if !strings.HasPrefix(cfg.Config.SMTP.Headers["from"], "epazote@") {
		t.Error("Expecting email with prefix: epazote@")
	}
}

func TestVerifyEmailSubject(t *testing.T) {
	cfg, err := New("test/epazote-email-subject.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.VerifyEmail()
	if err != nil {
		t.Error(err)
	}
	if cfg.Config.SMTP.Headers["subject"] != "[name, because]" {
		t.Errorf("Expecting subject: [name - exit] got: %v", cfg.Config.SMTP.Headers["subject"])
	}
}

func TestVerifyEmailNoHeaders(t *testing.T) {
	cfg, err := New("test/epazote-basic.yml")
	if err != nil {
		t.Error(err)
	}

	if cfg.Config.SMTP.Headers != nil {
		t.Errorf("Expecting nil")
	}

	err = cfg.VerifyEmail()
	if err != nil {
		t.Error(err)
	}

	if cfg.Config.SMTP.Headers == nil {
		t.Errorf("Expecting map")
	}
}
