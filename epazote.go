package epazote

import (
	"regexp"
	"sync"
)

const (
	herb = "1f33f"
	shit = "1f4a9"
)

// Epazote parent struct
type Epazote struct {
	sync.RWMutex `yaml:"-" json:"-"`
	Config       Config
	Services     Services
	debug        bool
}

// Config
type (
	// Config struct
	Config struct {
		SMTP Email `yaml:"smtp,omitempty"`
		Scan Scan  `yaml:"scan,omitempty"`
	}

	// Email server details for sending email notifications
	Email struct {
		Username string            `yaml:",omitempty"`
		Password string            `yaml:",omitempty"`
		Server   string            `yaml:",omitempty"`
		Port     int               `yaml:",omitempty"`
		Headers  map[string]string `yaml:",omitempty"`
		enabled  bool
	}

	// Scan paths to scan
	Scan struct {
		Paths []string `yaml:",omitempty"`
		Every `yaml:",inline"`
	}
)

// Services
type (
	// Services list of services to check
	Services map[string]*Service

	// Service struct
	Service struct {
		Name          string            `yaml:"-" json:"name"`
		URL           string            `yaml:",omitempty" json:"url,omitempty"`
		Disable       bool              `yaml:",omitempty" json:"-"`
		Follow        bool              `yaml:",omitempty" json:"-"`
		Header        map[string]string `yaml:",omitempty" json:"-"`
		IfHeader      map[string]Action `yaml:"if_header,omitempty" json:"-"`
		IfStatus      map[int]Action    `yaml:"if_status,omitempty" json:"-"`
		Insecure      bool              `yaml:",omitempty" json:"-"`
		Log           string            `yaml:",omitempty" json:"-"`
		ReadLimit     int64             `yaml:"read_limit,omitempty" json:"read_limit,omitempty"`
		RetryInterval int               `yaml:"retry_interval,omitempty" json:"-"`
		RetryLimit    int               `yaml:"retry_limit,omitempty" json:"-"`
		Stop          int               `yaml:",omitempty" json:"-"`
		Threshold     Threshold         `yaml:",omitempty" json:"-"`
		Timeout       int               `yaml:",omitempty" json:"-"`
		Every         `yaml:",inline" json:"-"`
		Test          `yaml:",inline" json:",omitempty"`
		Expect        Expect `json:"-"`
		status        int
		action        *Action
		retryCount    int
	}

	// Every how often call/check the services
	Every struct {
		Seconds, Minutes, Hours int `yaml:",omitempty"`
	}

	// Test for NO web services
	Test struct {
		Test  string `yaml:",omitempty" json:"test,omitempty"`
		IfNot Action `yaml:"if_not,omitempty" json:"-"`
	}

	// Threshold default to 2
	Threshold struct {
		Healthy   int `yaml:",omitempty"`
		Unhealthy int `yaml:",omitempty"`
		healthy   int
	}

	// Expect do someting if not getting what you expect
	Expect struct {
		Body   string `yaml:",omitempty"`
		body   *regexp.Regexp
		Header map[string]string `yaml:",omitempty"`
		Status int               `yaml:",omitempty"`
		SSL    SSL               `yaml:"ssl,omitempty" json:"-"`
		IfNot  Action            `yaml:"if_not,omitempty"`
	}

	// Action a corrective/notify action to perform
	Action struct {
		Cmd    string   `yaml:",omitempty"`
		Emoji  []string `yaml:",omitempty"`
		HTTP   []HTTP   `yaml:"http,omitempty"`
		Msg    []string `yaml:",omitempty"`
		Notify string   `yaml:",omitempty"`
	}
)

// HTTP custom endpoints to call when notifying for example hipchat
type HTTP struct {
	URL    string            `yaml:",omitempty"`
	Method string            `yaml:",omitempty"`
	Header map[string]string `yaml:",omitempty"`
	Data   string            `yaml:",omitempty"`
}

// SSL notify before a certificate expires
type SSL struct {
	Every `yaml:",inline"`
}
