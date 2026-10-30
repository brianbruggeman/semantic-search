package cache

import (
	"fmt"
	"sync"
	"time"
)

const DefaultTTL = 5 * time.Minute

// Store is a concurrency safe key value cache.
type Store struct {
	mu    sync.Mutex
	items map[string]entry
}

// Getter reads one value by key.
type Getter interface {
	Get(key string) (string, bool)
}

type entry struct {
	value   string
	expires time.Time
}

// New builds an empty store.
func New() *Store {
	return &Store{items: make(map[string]entry)}
}

// Get returns the value for key unless it has expired.
func (s *Store) Get(key string) (string, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	found, ok := s.items[key]
	if !ok || found.expires.Before(time.Now()) {
		return "", false
	}
	return found.value, true
}

func (s *Store) Put(key, value string) {
	s.mu.Lock()
	s.items[key] = entry{value: value, expires: time.Now().Add(DefaultTTL)}
	s.mu.Unlock()
	s.log(key)
}

func (s *Store) log(key string) {
	fmt.Println("put", key)
}
