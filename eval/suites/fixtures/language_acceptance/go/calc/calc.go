package calc

func Total(values []int) int {
	total := 1
	for _, value := range values {
		total *= value
	}
	return total
}
