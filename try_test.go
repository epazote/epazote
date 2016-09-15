// https://medium.com/@matryer/retrying-in-golang-quicktip-f688d00e650a#.ylrrg0mn3
// https://github.com/matryer/try/blob/master/try_test.go
package epazote

import (
	"errors"
	"fmt"
	"log"
	"testing"
)

func TestTryExample(t *testing.T) {
	SomeFunction := func() (string, error) {
		return "", nil
	}
	err := Try(func(attempt int) (bool, error) {
		var err error
		_, err = SomeFunction()
		return attempt < 5, err // try 5 times
	})
	if err != nil {
		log.Fatalln("error:", err)
	}
}

func TestTryExamplePanic(t *testing.T) {
	SomeFunction := func() (string, error) {
		panic("test panic")
	}
	err := Try(func(attempt int) (retry bool, err error) {
		retry = attempt < 5 // try 5 times
		defer func() {
			if r := recover(); r != nil {
				err = fmt.Errorf("panic: %v", r)
			}
		}()
		_, err = SomeFunction()
		return
	})
	if err.Error() != "panic: test panic" {
		t.Errorf("Expecting: %s, got: %s", "panic: test panic", err.Error())
	}
}

func TestTryDoSuccessful(t *testing.T) {
	callCount := 0
	err := Try(func(attempt int) (bool, error) {
		callCount++
		return attempt < 5, nil
	})
	if err != nil {
		t.Error(err)
	}
	if callCount != 1 {
		t.Error("Expecting callcount = 1")
	}
}

func TestTryDoFailed(t *testing.T) {
	theErr := errors.New("something went wrong")
	callCount := 0
	err := Try(func(attempt int) (bool, error) {
		callCount++
		return attempt < 5, theErr
	})
	if err.Error() != theErr.Error() {
		t.Errorf("Expecting: %s Got: %s", theErr.Error(), err.Error())
	}
	if callCount != 5 {
		t.Error("Expecting callCount to be 5")
	}
}

func TestTryPanics(t *testing.T) {
	theErr := errors.New("something went wrong")
	callCount := 0
	err := Try(func(attempt int) (retry bool, err error) {
		retry = attempt < 5
		defer func() {
			if r := recover(); r != nil {
				err = fmt.Errorf("panic: %v", r)
			}
		}()
		callCount++
		if attempt > 2 {
			panic("I don't like three")
		}
		err = theErr
		return
	})
	if err.Error() != "panic: I don't like three" {
		t.Errorf("Expecting: %s Got: %s", "panic: I don't like three", err.Error())
	}
	if callCount != 5 {
		t.Error("Expecting callCount to be 5")
	}
}

func TestRetryLimit(t *testing.T) {
	err := Try(func(attempt int) (bool, error) {
		return true, errors.New("nope")
	})
	if err == nil {
		t.Error("Expecting an error")
	}
}
