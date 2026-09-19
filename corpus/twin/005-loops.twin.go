package main

import "fmt"

func sumTo(n int64) int64 {
	var i int64 = 0
	var total int64 = 0
	for i <= n {
		total = total + i
		i = i + 1
	}
	return total
}

func main() {
	fmt.Println(sumTo(10))
	fmt.Println(sumTo(100))

	var r int64 = 0
	var a int64 = 0
	for a < 5 {
		var b int64 = 0
		for b < 5 {
			r = r + a*b
			b = b + 1
		}
		a = a + 1
	}
	fmt.Println(r)

	var z int64 = 0
	for false {
		z = 99
	}
	fmt.Println(z)
}
