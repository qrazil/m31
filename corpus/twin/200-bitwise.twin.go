package main

import "fmt"

func main() {
	var x int64 = 1234567
	fmt.Println((x & 255) | 4096)
	fmt.Println(x ^ (x >> 3))
	fmt.Println(^x & 65535)
	fmt.Println((x << 40) >> 20)
	fmt.Println(-x >> 5)
	fmt.Println(-x << 50)
	fmt.Println(x&1 == 1)

	var h int64 = 17
	for i := int64(0); i < 20; i++ {
		h = h ^ (h << 7)
		h = h ^ (h >> 9)
		h = h*-7046029254386353131 + i
	}
	fmt.Println(h)

	var v int64 = -1234567
	var n int64
	for k := uint(0); k < 64; k++ {
		n += (v >> k) & 1
	}
	fmt.Println(n)
}
