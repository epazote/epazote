package epazote

import "fmt"

func DoTest(url string) Decorator {
	return func(t Task) func() {
		return func() {
			fmt.Println("do test")
		}
	}
}

func DoGet() Decorator {
	return func(t Task) func() {
		return func() {
			fmt.Println("do get")
		}
	}
}
