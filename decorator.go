package epazote

import "fmt"

// Decorator for Client
type Decorator func(Task) func()

// Decorate will decorate a client with a slice of passed decorators
func Decorate(t Task, ds ...Decorator) func() {
	decorated := t
	for _, decorate := range ds {
		decorated = decorate(decorated)
	}
	return decorated
}

type Task func()

type MockTask struct {
	mailman MailMan
}

func (mc MockTask) Do() func() {
	return func() {
		fmt.Println("moc func")
	}
}
