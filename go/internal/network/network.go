// Package network runs fresh curl connections to separate network setup
// (DNS, TCP, TLS) from endpoint latency. Bench only; never starts a race.
package network

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os/exec"
	"runtime"
	"slices"
	"strings"
	"time"

	"agentwars/internal/report"
	"agentwars/internal/rpc"
)

type timings struct {
	TimeNamelookup    float64 `json:"time_namelookup"`
	TimeConnect       float64 `json:"time_connect"`
	TimeAppconnect    float64 `json:"time_appconnect"`
	TimeStarttransfer float64 `json:"time_starttransfer"`
	TimeTotal         float64 `json:"time_total"`
	HTTPCode          int     `json:"http_code"`
	HTTPVersion       string  `json:"http_version"`
	RemoteIP          string  `json:"remote_ip"`
	RemotePort        int     `json:"remote_port"`
}

type sample struct {
	Timing  *timings            `json:"timing"`
	Headers map[string][]string `json:"headers"`
}

func parse(output []byte) (*sample, error) {
	var s sample
	if err := json.Unmarshal(output, &s); err != nil || s.Timing == nil || s.Headers == nil {
		return nil, errors.New("invalid curl timing output; network diagnostics require curl >= 7.83")
	}
	t := s.Timing
	for _, value := range []float64{t.TimeNamelookup, t.TimeConnect, t.TimeAppconnect, t.TimeStarttransfer, t.TimeTotal} {
		if math.IsInf(value, 0) || math.IsNaN(value) || value < 0 {
			return nil, errors.New("invalid curl timing values")
		}
	}
	if !(t.TimeConnect >= t.TimeNamelookup &&
		(t.TimeAppconnect == 0 || t.TimeAppconnect >= t.TimeConnect) &&
		t.TimeStarttransfer >= s.setupSeconds() &&
		t.TimeTotal >= t.TimeStarttransfer) {
		return nil, errors.New("inconsistent curl timing order")
	}
	return &s, nil
}

func (s *sample) setupSeconds() float64 {
	return math.Max(s.Timing.TimeAppconnect, s.Timing.TimeConnect)
}

func (s *sample) tcpMs() float64 {
	return (s.Timing.TimeConnect - s.Timing.TimeNamelookup) * 1000
}

func (s *sample) tlsMs() (float64, bool) {
	if s.Timing.TimeAppconnect > 0 {
		return (s.Timing.TimeAppconnect - s.Timing.TimeConnect) * 1000, true
	}
	return 0, false
}

func (s *sample) header(name string) (string, bool) {
	for key, values := range s.Headers {
		if strings.EqualFold(key, name) && len(values) > 0 {
			return values[0], true
		}
	}
	return "", false
}

func (s *sample) optionalHeader(name string) string {
	if value, ok := s.header(name); ok {
		return fmt.Sprintf("Some(%q)", value)
	}
	return "None"
}

func (s *sample) report(index int) {
	t := s.Timing
	tls := "n/a"
	if value, ok := s.tlsMs(); ok {
		tls = fmt.Sprintf("%.2f ms", value)
	}
	report.Say("network probe %d: DNS %.2f ms | TCP connect %.2f ms | TLS %s | setup %.2f ms",
		index+1, t.TimeNamelookup*1000, s.tcpMs(), tls, s.setupSeconds()*1000)
	report.Say("  getCompetition: TTFB %.2f ms | post-setup wait %.2f ms | total %.2f ms",
		t.TimeStarttransfer*1000, (t.TimeStarttransfer-s.setupSeconds())*1000, t.TimeTotal*1000)
	report.Say("  HTTP/%s peer=%s:%d status=%d x-vercel-id=%s server-timing=%s",
		t.HTTPVersion, t.RemoteIP, t.RemotePort, t.HTTPCode, s.optionalHeader("x-vercel-id"), s.optionalHeader("server-timing"))
}

func arguments(origin, code, userAgent string) []string {
	output := "/dev/null"
	if runtime.GOOS == "windows" {
		output = "NUL"
	}
	body, _ := json.Marshal(map[string]any{"json": map[string]any{"productId": rpc.ProductID, "code": code}})
	return []string{
		"--disable",
		"--silent", "--show-error", "--noproxy", "*", "--tcp-nodelay",
		"--connect-timeout", "5", "--max-time", "10",
		"--output", output,
		"--write-out", `{"timing":%{json},"headers":%{header_json}}`,
		"--user-agent", userAgent,
		"--header", "content-type: application/json",
		"--header", "x-product-id: " + rpc.ProductID,
		"--header", "origin: " + rpc.Origin,
		"--referer", rpc.Origin + "/",
		"--data-binary", string(body),
		"--url", origin + "/api/rpc/superchallenge/getCompetition",
	}
}

func probe(ctx context.Context, origin, code, userAgent string) (*sample, error) {
	ctx, cancel := context.WithTimeout(ctx, 12*time.Second)
	defer cancel()
	command := exec.CommandContext(ctx, "curl", arguments(origin, code, userAgent)...)
	var stdout, stderr bytes.Buffer
	command.Stdout, command.Stderr = &stdout, &stderr
	err := command.Run()
	if ctx.Err() == context.DeadlineExceeded {
		return nil, errors.New("curl network diagnostic exceeded 12 seconds")
	}
	var exit *exec.ExitError
	if errors.As(err, &exit) {
		return nil, fmt.Errorf("curl network diagnostic failed (%v): %s", exit, strings.TrimSpace(stderr.String()))
	}
	if err != nil {
		return nil, fmt.Errorf("cannot start curl; install curl >= 7.83, or disable diagnostics with --network-probes 0: %w", err)
	}
	return parse(stdout.Bytes())
}

func Bench(ctx context.Context, code, userAgent string, rounds int) error {
	return benchAt(ctx, rpc.Origin, code, userAgent, rounds)
}

func benchAt(ctx context.Context, origin, code, userAgent string, rounds int) error {
	if rounds < 0 || rounds > 10 {
		return errors.New("network probes must be between 0 and 10")
	}
	if rounds == 0 {
		report.Say("network setup diagnostics disabled (--network-probes 0)")
		return nil
	}
	report.Say("network setup diagnostics: %d fresh curl connections; separate from the racer's HTTP pools", rounds)
	report.Say("TCP/TLS setup is not endpoint RTT. TTFB and post-setup wait include network and server work.")
	connects := make([]float64, 0, rounds)
	for index := range rounds {
		s, err := probe(ctx, origin, code, userAgent)
		if err != nil {
			return err
		}
		s.report(index)
		if s.Timing.HTTPCode < 200 || s.Timing.HTTPCode >= 300 {
			return fmt.Errorf("network diagnostic returned HTTP %d; stopping before further probes", s.Timing.HTTPCode)
		}
		connects = append(connects, s.tcpMs())
	}
	slices.Sort(connects)
	n := len(connects)
	median := (connects[(n-1)/2] + connects[n/2]) / 2
	report.Say("TCP connect summary: min %.2f p50 %.2f max %.2f ms (DNS/TLS excluded)", connects[0], median, connects[n-1])
	return nil
}
