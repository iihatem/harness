package task

type Config struct {
	Name    string
	Retries int
}

func NewConfig(name string, retries int) Config {
	return Config{Name: name, Retries: retries}
}
