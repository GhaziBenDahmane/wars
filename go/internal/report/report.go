// Package report sends progress lines to stderr and to an in-memory buffer
// the web page polls.
package report

import (
	"fmt"
	"os"
	"sync"
)

var (
	mu    sync.Mutex
	lines []string
)

func Say(format string, args ...any) {
	line := fmt.Sprintf(format, args...)
	fmt.Fprintln(os.Stderr, line)
	mu.Lock()
	lines = append(lines, line)
	mu.Unlock()
}

func Clear() {
	mu.Lock()
	lines = nil
	mu.Unlock()
}

func Lines() []string {
	mu.Lock()
	defer mu.Unlock()
	return append([]string{}, lines...)
}
