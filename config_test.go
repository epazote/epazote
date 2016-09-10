package epazote

import (
	"testing"
)

func TestConfigNew(t *testing.T) {
	var testTable = []struct {
		yml      string
		expected bool // true = error
	}{
		{"test/no-exists.yml", true},
		{"test/bad.yml", true},
		{"test/epazote.yml", false},
	}
	for _, tt := range testTable {
		_, err := New(tt.yml)
		if tt.expected {
			if err == nil {
				t.Fatal(err)
			}
		} else {
			if err != nil {
				t.Fatal(err)
			}
		}
	}
}

func TestConfigGetInterval(t *testing.T) {
	var testTable = []struct {
		every    Every
		d        int
		expected int
	}{
		{Every{}, 0, 60},
		{Every{1, 0, 0}, 30, 1},
		{Every{0, 1, 0}, 30, 60},
		{Every{0, 0, 1}, 30, 3600},
	}
	for _, tt := range testTable {
		i := GetInterval(tt.d, tt.every)
		expect(t, tt.expected, i)
	}
}

func TestParseScan(t *testing.T) {
	var testTable = []struct {
		yml      string
		expected bool // true = error
	}{
		{"test/no-exists.yml", true},
		{"test/bad.yml", true},
		{"test/empty.yml", true},
		{"test/bad-url.yml", false},
		{"test/every.yml", false},
	}
	for _, tt := range testTable {
		_, err := ParseScan(tt.yml)
		if tt.expected {
			if err == nil {
				t.Fatal(err)
			}
		} else {
			if err != nil {
				t.Fatal(err)
			}
		}
	}
}

func TestParseScanEvery(t *testing.T) {
	s, err := ParseScan("test/every.yml")

	if err != nil {
		t.Error(err)
	}

	switch {
	case s["service 1"].Every.Seconds != 30:
		t.Error("Expecting 60 got: ", s["service 1"].Every.Minutes)
	case s["service 2"].Every.Minutes != 1:
		t.Error("Expecting 1 got:", s["service 2"].Every.Minutes)
	case s["service 3"].Every.Hours != 2:
		t.Error("Expecting 2 got:", s["service 3"].Every.Hours)
	}
}

func TestCheckPaths(t *testing.T) {
	var testTable = []struct {
		yml      string
		expected bool // true = error
	}{
		{"test/epazote-checkpaths-ne.yml", true},
		{"test/epazote-checkpaths.yml", false},
		{"test/epazote-checkpaths-empty.yml", false},
		{"test/test.yml", false},
	}
	for _, tt := range testTable {
		cfg, err := New(tt.yml)
		if err != nil {
			t.Fatal(err, cfg)
		}
		err = cfg.CheckPaths()
		if tt.expected {
			if err == nil {
				t.Fatal(err)
			}
		} else {
			if err != nil {
				t.Fatal(err)
			}
		}
	}
}

func TestCheckVerifyUrls(t *testing.T) {
	var testTable = []struct {
		yml      string
		expected bool // true = error
	}{
		{"test/every.yml", false},
		{"test/epazote.yml", true},
		{"test/test.yml", true},
	}
	for _, tt := range testTable {
		cfg, err := New(tt.yml)
		if err != nil {
			t.Fatal(err, cfg)
		}
		err = cfg.VerifyUrls()
		if tt.expected {
			if err == nil {
				t.Fatal(err)
			}
		} else {
			if err != nil {
				t.Fatal(err)
			}
		}
	}
}

func TestPathsOrServicesEmpty(t *testing.T) {
	e := &Epazote{}
	err := e.PathsOrServices()
	if err == nil {
		t.Error(err)
	}
}

func TestPathsOrServices(t *testing.T) {
	cfg, err := New("test/epazote.yml")
	if err != nil {
		t.Error(err)
	}
	err = cfg.PathsOrServices()
	if err != nil {
		t.Error(err)
	}
}
