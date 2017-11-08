package epazote

import (
	"fmt"
	"io/ioutil"
	"time"

	"github.com/go-yaml/yaml"
)

// GetInterval return the check interval in seconds
func GetInterval(d int, e Every) time.Duration {
	// default to 60 seconds
	if d < 1 {
		d = 60
	}
	if e.Seconds > 0 {
		return time.Duration(e.Seconds) * time.Second
	}
	if e.Minutes > 0 {
		return time.Duration(e.Minutes) * time.Minute
	}
	if e.Hours > 0 {
		return time.Duration(e.Hours) * time.Hour
	}
	return time.Duration(d) * time.Second
}

// ParseScan search for yml files
func ParseScan(file string) (Services, error) {
	ymlFile, err := ioutil.ReadFile(file)
	if err != nil {
		return nil, err
	}
	var s Services
	if err := yaml.Unmarshal(ymlFile, &s); err != nil {
		return nil, err
	}
	if len(s) == 0 {
		return nil, fmt.Errorf("[%s] No services found", Red(file))
	}
	return s, nil
}
