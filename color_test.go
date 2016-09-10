package epazote

import (
	"testing"
)

func TestColorRed(t *testing.T) {
	color := Red("@")

	if color != "\x1b[0;31m@\x1b[0;00m" {
		t.Errorf("Expected red got: %s", color)
	}
}

func TestColorGreen(t *testing.T) {
	color := Green("@")

	if color != "\x1b[0;32m@\x1b[0;00m" {
		t.Errorf("Expected green got: %s", color)
	}
}

func TestColorYellow(t *testing.T) {
	color := Yellow("@")

	if color != "\x1b[0;33m@\x1b[0;00m" {
		t.Errorf("Expected yellow got: %s", color)
	}
}

func TestIconOk(t *testing.T) {
	i := Icon("1F621")
	if i != 128545 {
		t.Errorf("Expecting: 128545 got: %v", i)
	}
}

func TestIconBad(t *testing.T) {
	i := Icon("Z")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
}
