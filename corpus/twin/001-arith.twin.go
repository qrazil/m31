// Twin for 001-arith. Truth comes from Go, not from our own compiler.
//
// Go is the right twin here: same altitude, int64 semantics, a real spec,
// and written by other people. This is the only oracle layer that can catch
// a bug in our frontend or IR passes — the compiler/optimisation matrix
// only ever catches backend bugs.
package main

import "fmt"

func main() {
	var x int64 = 1000000
	fmt.Println(x*3 + 7)
	fmt.Println((x - 1) / 7)
	fmt.Println(x % 97)
	fmt.Println(0 - x)
}
