package epazote

import (
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"
	"regexp"

	"gopkg.in/yaml.v2"
)

const (
	herb = "1f33f"
	shit = "1f4a9"
)

type Epazote struct {
	Config   Config
	Services Services
	debug    bool
}

type Config struct {
	SMTP Email `yaml:"smtp,omitempty"`
	Scan Scan  `yaml:"scan,omitempty"`
}

type Email struct {
	Username string            `yaml:",omitempty"`
	Password string            `yaml:",omitempty"`
	Server   string            `yaml:",omitempty"`
	Port     int               `yaml:",omitempty"`
	Headers  map[string]string `yaml:",omitempty"`
	enabled  bool
}

type Every struct {
	Seconds, Minutes, Hours int `yaml:",omitempty"`
}

type Scan struct {
	Paths []string `yaml:",omitempty"`
	Every `yaml:",inline"`
}

type Services map[string]*Service

type Test struct {
	Test  string `yaml:",omitempty" json:"test,omitempty"`
	IfNot Action `yaml:"if_not,omitempty" json:"-"`
}

type Service struct {
	Name          string            `json:"name" yaml:"-"`
	URL           string            `yaml:",omitempty" json:"url,omitempty"`
	RetryInterval int               `yaml:"retry_interval,omitempty" json:"-"`
	RetryLimit    int               `yaml:"retry_limit,omitempty" json:"-"`
	ReadLimit     int64             `yaml:"read_limit,omitempty" json:"read_limit,omitempty"`
	Header        map[string]string `yaml:",omitempty" json:"-"`
	Follow        bool              `yaml:",omitempty" json:"-"`
	Insecure      bool              `yaml:",omitempty" json:"-"`
	Stop          int64             `yaml:",omitempty" json:"-"`
	Threshold     Threshold         `yaml:",omitempty" json:"-"`
	Timeout       int               `yaml:",omitempty" json:"-"`
	IfStatus      map[int]Action    `yaml:"if_status,omitempty" json:"-"`
	IfHeader      map[string]Action `yaml:"if_header,omitempty" json:"-"`
	Log           string            `yaml:",omitempty" json:"-"`
	Test          `yaml:",inline" json:",omitempty"`
	Every         `yaml:",inline" json:"-"`
	Expect        Expect `json:"-"`
	status        int64
	action        *Action
	retryCount    int
}

type Threshold struct {
	Healthy   int `yaml:",omitempty"`
	Unhealthy int `yaml:",omitempty"`
}

type Expect struct {
	Body   string `yaml:",omitempty"`
	body   *regexp.Regexp
	Header map[string]string `yaml:",omitempty"`
	Status int               `yaml:",omitempty"`
	IfNot  Action            `yaml:"if_not,omitempty"`
}

type Action struct {
	Cmd    string   `yaml:",omitempty"`
	Notify string   `yaml:",omitempty"`
	Msg    []string `yaml:",omitempty"`
	Emoji  []string `yaml:",omitempty"`
	HTTP   []HTTP   `yaml:"http,omitempty"`
}

type HTTP struct {
	URL    string            `yaml:",omitempty"`
	Method string            `yaml:",omitempty"`
	Header map[string]string `yaml:",omitempty"`
	Data   string            `yaml:",omitempty"`
}

func New(file string) (*Epazote, error) {
	yml_file, err := ioutil.ReadFile(file)
	if err != nil {
		return nil, err
	}

	var ez Epazote

	if err := yaml.Unmarshal(yml_file, &ez); err != nil {
		return nil, err
	}

	return &ez, nil
}

// CheckPaths verify that directories exist and are readable
func (e *Epazote) CheckPaths() error {
	if len(e.Config.Scan.Paths) > 0 {
		for k, d := range e.Config.Scan.Paths {
			if _, err := os.Stat(d); os.IsNotExist(err) {
				return fmt.Errorf("Verify that directory: %s, exists and is readable.", d)
			}
			r, err := filepath.EvalSymlinks(d)
			if err != nil {
				return err
			}
			e.Config.Scan.Paths[k] = r
		}
		return nil
	}
	return nil
}

// VerifyUrls, we can't supervice unreachable services
func (e *Epazote) VerifyUrls() error {
	ch := AsyncGet(&e.Services)
	for i := 0; i < len(e.Services); i++ {
		x := <-ch
		if x.Err != nil {
			// if not a valid URL check if service contains a test & if_not
			if len(e.Services[x.Service].Test.Test) > 0 {
				if len(e.Services[x.Service].Test.IfNot.Cmd) == 0 {
					return fmt.Errorf("%s - Verify test, missing cmd", Red(x.Service))
				}
			} else {
				return fmt.Errorf("%s - Verify URL: %q", Red(x.Service), x.Err)
			}
		}
	}
	return nil
}

// PathOrServices check if at least one path or service is set
func (e *Epazote) PathsOrServices() error {
	if len(e.Config.Scan.Paths) == 0 && e.Services == nil {
		return fmt.Errorf("%s", Red("No services to supervices or paths to scan."))
	}
	return nil
}

// GetInterval return the check interval in seconds
func GetInterval(d int, e Every) int {
	// default to 60 seconds
	if d < 1 {
		d = 60
	}

	every := d

	if e.Seconds > 0 {
		return e.Seconds
	} else if e.Minutes > 0 {
		return 60 * e.Minutes
	} else if e.Hours > 0 {
		return 3600 * e.Hours
	}

	return every
}

func ParseScan(file string) (Services, error) {
	yml_file, err := ioutil.ReadFile(file)
	if err != nil {
		return nil, err
	}

	var s Services

	if err := yaml.Unmarshal(yml_file, &s); err != nil {
		return nil, err
	}

	if len(s) == 0 {
		return nil, fmt.Errorf("[%s] No services found.", Red(file))
	}

	return s, nil
}
