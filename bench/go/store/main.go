package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"sync"

	"github.com/alexandernicholson/rebar/bench/go/common"
)

type storeValue struct {
	Value string `json:"value"`
}

type storeResponse struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

// keyEntry holds the value for a single key with its own RWMutex.
type keyEntry struct {
	mu    sync.RWMutex
	value string
	set   bool
}

// store holds all key entries, protected by a global RWMutex.
type store struct {
	mu   sync.RWMutex
	keys map[string]*keyEntry
}

func newStore() *store {
	return &store{
		keys: make(map[string]*keyEntry),
	}
}

// getOrCreate returns the keyEntry for the given key, creating it if needed.
func (s *store) getOrCreate(key string) *keyEntry {
	// Fast path: read lock
	s.mu.RLock()
	entry, ok := s.keys[key]
	s.mu.RUnlock()
	if ok {
		return entry
	}

	// Slow path: write lock
	s.mu.Lock()
	defer s.mu.Unlock()
	// Double-check after acquiring write lock
	if entry, ok = s.keys[key]; ok {
		return entry
	}
	entry = &keyEntry{}
	s.keys[key] = entry
	return entry
}

func (s *store) get(key string) (string, bool) {
	s.mu.RLock()
	entry, ok := s.keys[key]
	s.mu.RUnlock()
	if !ok {
		return "", false
	}
	entry.mu.RLock()
	defer entry.mu.RUnlock()
	if !entry.set {
		return "", false
	}
	return entry.value, true
}

func (s *store) put(key, value string) {
	entry := s.getOrCreate(key)
	entry.mu.Lock()
	defer entry.mu.Unlock()
	entry.value = value
	entry.set = true
}

func healthHandler(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
	fmt.Fprint(w, "ok")
}

func makeGetHandler(s *store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		key := r.PathValue("key")
		if key == "" {
			http.Error(w, "missing key", http.StatusBadRequest)
			return
		}

		value, ok := s.get(key)
		if !ok {
			http.Error(w, "not found", http.StatusNotFound)
			return
		}

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(storeResponse{Key: key, Value: value})
	}
}

func makePutHandler(s *store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		key := r.PathValue("key")
		if key == "" {
			http.Error(w, "missing key", http.StatusBadRequest)
			return
		}

		var body storeValue
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}

		s.put(key, body.Value)
		w.WriteHeader(http.StatusOK)
	}
}

func main() {
	s := newStore()

	mux := http.NewServeMux()
	mux.HandleFunc("GET /health", healthHandler)
	mux.HandleFunc("GET /store/{key}", makeGetHandler(s))
	mux.HandleFunc("PUT /store/{key}", makePutHandler(s))

	addr := "0.0.0.0:" + common.StoreHTTPPort
	log.Printf("Store service listening on %s", addr)
	log.Fatal(http.ListenAndServe(addr, mux))
}
