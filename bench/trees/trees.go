// The same program in Go: a tracing garbage collector, for contrast.
package main

import "fmt"

type Tree struct{ l, r *Tree }

func build(depth int) *Tree {
	if depth == 0 {
		return &Tree{}
	}
	return &Tree{build(depth - 1), build(depth - 1)}
}

func check(t *Tree) int {
	if t.l == nil {
		return 1
	}
	return 1 + check(t.l) + check(t.r)
}

func main() {
	total := 0
	for i := 0; i < 16; i++ {
		total += check(build(18))
	}
	fmt.Println(total)
}
