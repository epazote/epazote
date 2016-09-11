package epazote

import (
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"

	"gopkg.in/yaml.v2"
)

const (
	herb = "1f33f"
	shit = "1f4a9"
)

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
