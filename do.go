package epazote

import "fmt"

func DoTest(url string) Decorator {
	return func(t Task) func() {
		return func() {
			fmt.Println("satisfy interface")
		}
	}
}

/*
func DoGet() Decorator {
	return func(t Task) Task {
		fmt.Println("DoGet")
		return t
	}
}
*/
