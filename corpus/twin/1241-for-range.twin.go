package main

import "fmt"

func main() {
	total := 0
	for i := 0; i < 10; i++ {
		total = total + i
	}
	fmt.Println(total)

	odd := 0
	for i := 0; i < 10; i++ {
		if i%2 == 0 {
			continue
		}
		odd = odd + i
	}
	fmt.Println(odd)

	for i := 3; i < 3; i++ {
		fmt.Println(-1)
	}
	for i := 4; i < 1; i++ {
		fmt.Println(-2)
	}

	tri := 0
	for y := 0; y < 5; y++ {
		for x := 0; x < y; x++ {
			_ = x
			tri = tri + 1
		}
	}
	fmt.Println(tri)

	neg := 0
	for i := -3; i < 3; i++ {
		neg = neg + i
	}
	fmt.Println(neg)

	early := 0
	for i := 0; i < 100; i++ {
		if i == 7 {
			break
		}
		early = early + 1
	}
	fmt.Println(early)
}
