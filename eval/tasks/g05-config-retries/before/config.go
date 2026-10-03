package task

type Config struct {
	Name string
}

func NewConfig(name string) Config {
	return Config{Name: name}
}
