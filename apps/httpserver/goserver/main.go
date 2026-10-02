// goserver: the minimal, fair comparison baseline for apps/httpserver -- a
// "Hello, World!" HTTP/1.1 server using ONLY Go's standard library
// (net/http), no third-party dependencies. Same response body, same status
// code, same configurable host/port shape as the m31 server it is measured
// against (apps/httpserver/BENCHMARK.md).
//
//	go build -o goserver main.go
//	./goserver                      listen on 0.0.0.0:8080
//	./goserver -port 9000            listen on 0.0.0.0:9000
//	./goserver -host 127.0.0.1        listen on a specific address
package main

import (
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
)

const body = "Hello, World!\n"

func hello(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write([]byte(body))
}

func main() {
	host := flag.String("host", "0.0.0.0", "address to listen on")
	port := flag.Int("port", 8080, "port to listen on")
	flag.Parse()

	addr := fmt.Sprintf("%s:%d", *host, *port)
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		log.Fatalf("goserver: listen %s: %v", addr, err)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/", hello)

	fmt.Printf("goserver: listening on %s\n", ln.Addr().String())
	if err := http.Serve(ln, mux); err != nil {
		log.Fatalf("goserver: serve: %v", err)
	}
}
