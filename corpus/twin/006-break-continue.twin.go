package main

import "fmt"

func firstSquareOver(limit int64) int64 {
	var i int64 = 0
	for i < 1000 {
		if i*i > limit {
			return i
		}
		i = i + 1
	}
	return -1
}

func main() {
	fmt.Println(firstSquareOver(50))

	var i int64 = 0
	var found int64 = 0
	for i < 100 {
		if i*i > 50 {
			found = i
			break
		}
		i = i + 1
	}
	fmt.Println(found)

	var j int64 = 0
	var odd int64 = 0
	for j < 10 {
		j = j + 1
		if j%2 == 0 {
			continue
		}
		odd = odd + j
	}
	fmt.Println(odd)

	var a int64 = 0
	var hits int64 = 0
	for a < 4 {
		var b int64 = 0
		for b < 4 {
			if b == 2 {
				break
			}
			hits = hits + 1
			b = b + 1
		}
		a = a + 1
	}
	fmt.Println(hits)
}
