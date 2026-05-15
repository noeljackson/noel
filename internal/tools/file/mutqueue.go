package file

import (
	"path/filepath"
	"sync"
)

var mutationQueues = struct {
	sync.Mutex
	locks map[string]*sync.Mutex
}{
	locks: make(map[string]*sync.Mutex),
}

// WithLock serializes file mutation operations targeting the same file.
func WithLock(path string, fn func() error) error {
	key := filepath.Clean(path)

	mutationQueues.Lock()
	lock := mutationQueues.locks[key]
	if lock == nil {
		lock = &sync.Mutex{}
		mutationQueues.locks[key] = lock
	}
	mutationQueues.Unlock()

	lock.Lock()
	defer lock.Unlock()

	return fn()
}
