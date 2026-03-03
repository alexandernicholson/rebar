package main

import (
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"time"

	"github.com/alexandernicholson/rebar/bench/go/common"
)

type gateway struct {
	client      *http.Client
	computeBase string
	storeBase   string
}

func newGateway() *gateway {
	computeHost := os.Getenv("COMPUTE_HOST")
	if computeHost == "" {
		computeHost = "compute"
	}
	storeHost := os.Getenv("STORE_HOST")
	if storeHost == "" {
		storeHost = "store"
	}

	return &gateway{
		client: &http.Client{
			Timeout: 10 * time.Second,
		},
		computeBase: fmt.Sprintf("http://%s:%s", computeHost, common.ComputeHTTPPort),
		storeBase:   fmt.Sprintf("http://%s:%s", storeHost, common.StoreHTTPPort),
	}
}

func (g *gateway) healthHandler(w http.ResponseWriter, r *http.Request) {
	computeResp, computeErr := g.client.Get(g.computeBase + "/health")
	storeResp, storeErr := g.client.Get(g.storeBase + "/health")

	if computeErr != nil || storeErr != nil {
		http.Error(w, "service unavailable", http.StatusServiceUnavailable)
		return
	}
	defer computeResp.Body.Close()
	defer storeResp.Body.Close()

	if computeResp.StatusCode != http.StatusOK || storeResp.StatusCode != http.StatusOK {
		http.Error(w, "service unavailable", http.StatusServiceUnavailable)
		return
	}

	w.WriteHeader(http.StatusOK)
	fmt.Fprint(w, "ok")
}

func (g *gateway) computeHandler(w http.ResponseWriter, r *http.Request) {
	resp, err := g.client.Post(g.computeBase+"/compute", "application/json", r.Body)
	if err != nil {
		http.Error(w, "bad gateway", http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(resp.StatusCode)
	io.Copy(w, resp.Body)
}

func (g *gateway) storeGetHandler(w http.ResponseWriter, r *http.Request) {
	key := r.PathValue("key")
	if key == "" {
		http.Error(w, "missing key", http.StatusBadRequest)
		return
	}

	resp, err := g.client.Get(fmt.Sprintf("%s/store/%s", g.storeBase, key))
	if err != nil {
		http.Error(w, "bad gateway", http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(resp.StatusCode)
	io.Copy(w, resp.Body)
}

func (g *gateway) storePutHandler(w http.ResponseWriter, r *http.Request) {
	key := r.PathValue("key")
	if key == "" {
		http.Error(w, "missing key", http.StatusBadRequest)
		return
	}

	url := fmt.Sprintf("%s/store/%s", g.storeBase, key)
	req, err := http.NewRequest(http.MethodPut, url, r.Body)
	if err != nil {
		http.Error(w, "bad gateway", http.StatusBadGateway)
		return
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := g.client.Do(req)
	if err != nil {
		http.Error(w, "bad gateway", http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()

	// Forward response body if any
	body, _ := io.ReadAll(resp.Body)
	w.WriteHeader(resp.StatusCode)
	if len(body) > 0 {
		w.Write(body)
	}
}

func main() {
	g := newGateway()

	mux := http.NewServeMux()
	mux.HandleFunc("GET /health", g.healthHandler)
	mux.HandleFunc("POST /compute", g.computeHandler)
	mux.HandleFunc("GET /store/{key}", g.storeGetHandler)
	mux.HandleFunc("PUT /store/{key}", g.storePutHandler)

	addr := "0.0.0.0:" + common.GatewayHTTPPort
	log.Printf("Gateway service listening on %s", addr)
	log.Fatal(http.ListenAndServe(addr, mux))
}
