package task

import "strconv"

func ParsePort(s string) (int, error) {
	n, _ := strconv.Atoi(s)
	return n, nil
}
