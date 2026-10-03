package network

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
)

func fixture() map[string]any {
	return map[string]any{
		"timing": map[string]any{
			"time_namelookup": 0.002, "time_connect": 0.003, "time_appconnect": 0.007,
			"time_starttransfer": 0.150, "time_total": 0.151,
			"http_code": 200, "http_version": "2", "remote_ip": "127.0.0.1", "remote_port": 443,
		},
		"headers": map[string]any{"x-vercel-id": []string{"fra1::fra1::test"}, "server-timing": []string{"app;dur=140"}},
	}
}

func encode(value any) []byte {
	raw, _ := json.Marshal(value)
	return raw
}

func near(a, b float64) bool { return a-b < 1e-9 && b-a < 1e-9 }

func TestSeparatesCumulativeCurlTimingsIntoPhases(t *testing.T) {
	s, err := parse(encode(fixture()))
	if err != nil {
		t.Fatal(err)
	}
	tls, ok := s.tlsMs()
	if !near(s.tcpMs(), 1) || !ok || !near(tls, 4) || !near(s.setupSeconds()*1000, 7) {
		t.Fatal(s.tcpMs(), tls, s.setupSeconds())
	}
	if v, _ := s.header("x-vercel-id"); v != "fra1::fra1::test" {
		t.Fatal(v)
	}
	if v, _ := s.header("Server-Timing"); v != "app;dur=140" {
		t.Fatal(v)
	}
}

func TestSupportsPlainHTTPAndMissingRoutingHeaders(t *testing.T) {
	f := fixture()
	f["timing"].(map[string]any)["time_appconnect"] = 0.0
	f["headers"] = map[string]any{}
	s, err := parse(encode(f))
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := s.tlsMs(); ok || s.setupSeconds() != 0.003 {
		t.Fatal(s.setupSeconds())
	}
	if _, ok := s.header("x-vercel-id"); ok {
		t.Fatal("header")
	}
}

func TestRejectsInvalidOrInconsistentTimings(t *testing.T) {
	for field, value := range map[string]float64{
		"time_namelookup": -1, "time_connect": 0.001, "time_appconnect": 0.001,
		"time_starttransfer": 0.001, "time_total": 0.001,
	} {
		f := fixture()
		f["timing"].(map[string]any)[field] = value
		if _, err := parse(encode(f)); err == nil {
			t.Error(field)
		}
	}
	if _, err := parse([]byte("curl: unknown --write-out variable")); err == nil {
		t.Error("garbage parsed")
	}
}

func TestCommandIsDirectBoundedAndDoesNotStartARace(t *testing.T) {
	args := arguments("https://superchallenge.io", "test", "test-agent")
	pair := func(a, b string) bool {
		for i := range len(args) - 1 {
			if args[i] == a && args[i+1] == b {
				return true
			}
		}
		return false
	}
	if args[0] != "--disable" || !pair("--noproxy", "*") || !pair("--max-time", "10") ||
		!strings.HasSuffix(args[len(args)-1], "/getCompetition") {
		t.Fatal(args)
	}
	if slices.ContainsFunc(args, func(a string) bool {
		return strings.Contains(a, "startRun") || a == "--insecure" || a == "--location"
	}) {
		t.Fatal(args)
	}
}

// Requires curl >= 7.83; uses only a loopback server. Set AGENTWARS_CURL_TEST=1.
func TestCurlProbeReadsTimingsAndHeadersFromALocalServer(t *testing.T) {
	if os.Getenv("AGENTWARS_CURL_TEST") != "1" {
		t.Skip("set AGENTWARS_CURL_TEST=1 (requires curl >= 7.83)")
	}
	var count atomic.Int64
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		count.Add(1)
		var input struct {
			JSON struct {
				Code string `json:"code"`
			} `json:"json"`
		}
		json.NewDecoder(r.Body).Decode(&input)
		w.Header().Set("x-vercel-id", "fra1::fra1::test")
		if input.JSON.Code == "throttled" {
			w.WriteHeader(http.StatusTooManyRequests)
		}
		w.Write([]byte(`{"json":{}}`))
	}))
	defer server.Close()
	ctx := context.Background()
	s, err := probe(ctx, server.URL, "test", "test")
	throttled := benchAt(ctx, server.URL, "throttled", "test", 3)
	disabled := benchAt(ctx, server.URL, "test", "test", 0)
	invalid := benchAt(ctx, server.URL, "test", "test", 11)
	if err != nil {
		t.Fatal(err)
	}
	if v, _ := s.header("x-vercel-id"); s.Timing.HTTPCode != 200 || s.Timing.RemoteIP != "127.0.0.1" || v != "fra1::fra1::test" {
		t.Fatal(s.Timing, v)
	}
	if _, ok := s.tlsMs(); ok {
		t.Fatal("tls on plain http")
	}
	if throttled == nil || !strings.Contains(throttled.Error(), "HTTP 429") || disabled != nil || invalid == nil {
		t.Fatal(throttled, disabled, invalid)
	}
	if count.Load() != 2 {
		t.Fatal(count.Load())
	}
}
