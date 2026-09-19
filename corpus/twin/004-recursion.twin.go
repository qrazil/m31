package main

import "fmt"

func fact(n int64) int64 {
	if n <= 1 {
		return 1
	}
	return n * fact(n-1)
}

func fib(n int64) int64 {
	if n < 2 {
		return n
	}
	return fib(n-1) + fib(n-2)
}

func main() {
	fmt.Println(fact(10))
	fmt.Println(fact(20))
	fmt.Println(fib(20))
}
