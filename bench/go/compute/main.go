package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"

	"github.com/alexandernicholson/rebar/bench/go/common"
)

type computeRequest struct {
	N int64 `json:"n"`
}

type computeResponse struct {
	Result uint64 `json:"result"`
}

func healthHandler(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
	fmt.Fprint(w, "ok")
}

func computeHandler(w http.ResponseWriter, r *http.Request) {
	var req computeRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, "bad request", http.StatusBadRequest)
		return
	}

	if req.N < 0 || req.N > 92 {
		// Crash injection: panic inside a goroutine-like scope with defer/recover
		func() {
			defer func() {
				if rv := recover(); rv != nil {
					http.Error(w, "internal server error", http.StatusInternalServerError)
				}
			}()
			panic(fmt.Sprintf("crash injection: n=%d out of range [0,92]", req.N))
		}()
		return
	}

	// Spawn a goroutine to compute fib(n) and collect the result via a channel
	ch := make(chan uint64, 1)
	go func() {
		ch <- common.Fib(req.N)
	}()
	result := <-ch

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(computeResponse{Result: result})
}

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /health", healthHandler)
	mux.HandleFunc("POST /compute", computeHandler)

	addr := "0.0.0.0:" + common.ComputeHTTPPort
	log.Printf("Compute service listening on %s", addr)
	log.Fatal(http.ListenAndServe(addr, mux))
}
