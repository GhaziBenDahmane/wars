// Package rpc is the oRPC client for superchallenge.io:
// `POST /api/rpc/superchallenge/<proc>` with `{"json": input}`, answering
// `{"json": output}` (non-2xx on errors).
package rpc

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptrace"
	"os"
	"strings"
	"time"

	"agentwars/internal/report"
)

const (
	Origin    = "https://superchallenge.io"
	ProductID = "superchallenge"
	rpcPrefix = "/api/rpc/superchallenge/"
)

// Error is a verdict from the server (e.g. 409 "question deadline passed"),
// as opposed to a transport failure: never retried.
type Error struct {
	Procedure string
	Status    int
	Body      any
}

func (e *Error) Error() string {
	body, _ := e.Body.(map[string]any)
	message, _ := body["message"].(string)
	code, _ := body["code"].(string)
	return fmt.Sprintf("%s failed (%d %s): %s data=%s", e.Procedure, e.Status, code, message, Text(body["data"]))
}

// StatusOf is the HTTP status of a server verdict, or 0 for any other error.
func StatusOf(err error) int {
	var rpcErr *Error
	if errors.As(err, &rpcErr) {
		return rpcErr.Status
	}
	return 0
}

var dialer = net.Dialer{Timeout: 5 * time.Second}

// Rpc is one client = one connection pool, so two Rpcs give two independent
// connections for hedging.
type Rpc struct {
	client    *http.Client
	endpoint  string
	userAgent string
	cookie    string
}

// ResponseInfo is the transport metadata of one response.
type ResponseInfo struct {
	Version      string
	Peer         string
	VercelID     *string
	ServerTiming *string
}

type CallTiming struct {
	Headers      time.Duration
	Body         time.Duration
	Total        time.Duration
	Version      string
	Peer         *string
	VercelID     *string
	ServerTiming *string
}

type HedgedResponse struct {
	Value       any
	Sent        int
	WinnerRoute int
	Timing      CallTiming
}

func optional(s *string) string {
	if s == nil {
		return "None"
	}
	return fmt.Sprintf("Some(%q)", *s)
}

func (info ResponseInfo) Report(index int) {
	peer := "None"
	if info.Peer != "" {
		peer = "Some(" + info.Peer + ")"
	}
	report.Say("http warmup route %d: %s peer=%s x-vercel-id=%s server-timing=%s",
		index, info.Version, peer, optional(info.VercelID), optional(info.ServerTiming))
}

func New(userAgent, cookie string) *Rpc {
	return ForRoute(userAgent, cookie, 0)
}

// EdgeIP is the address route `route` connects to instead of the one DNS
// returns: `QUIZ_SC_EDGE_IP=a+b` gives the i-th address in turn, "" keeps DNS.
func EdgeIP(route int) string {
	var edges []string
	for _, edge := range strings.Split(os.Getenv("QUIZ_SC_EDGE_IP"), "+") {
		if edge = strings.TrimSpace(edge); edge != "" {
			edges = append(edges, edge)
		}
	}
	if len(edges) == 0 {
		return ""
	}
	return edges[route%len(edges)]
}

// ForRoute is the client of route `route`, on that route's edge address.
func ForRoute(userAgent, cookie string, route int) *Rpc {
	r := WithOrigin(Origin, userAgent, cookie)
	if edge := EdgeIP(route); edge != "" {
		address := net.JoinHostPort(edge, "443")
		r.client.Transport.(*http.Transport).DialContext = func(ctx context.Context, network, _ string) (net.Conn, error) {
			return dialer.DialContext(ctx, network, address)
		}
	}
	return r
}

func WithOrigin(origin, userAgent, cookie string) *Rpc {
	transport := &http.Transport{
		Proxy:               nil,
		ForceAttemptHTTP2:   true,
		MaxIdleConnsPerHost: 4,
		IdleConnTimeout:     600 * time.Second,
		TLSHandshakeTimeout: 5 * time.Second,
		HTTP2:               &http.HTTP2Config{SendPingTimeout: 15 * time.Second},
	}
	transport.DialContext = (&dialer).DialContext
	// `QUIZ_SC_HTTP1=1` forces HTTP/1.1, to compare it with HTTP/2.
	if os.Getenv("QUIZ_SC_HTTP1") == "1" {
		transport.ForceAttemptHTTP2 = false
		transport.Protocols = new(http.Protocols)
		transport.Protocols.SetHTTP1(true)
	}
	return &Rpc{
		client:    &http.Client{Transport: transport, Timeout: 10 * time.Second},
		endpoint:  origin + rpcPrefix,
		userAgent: userAgent,
		cookie:    cookie,
	}
}

func encode(input any) ([]byte, error) {
	return json.Marshal(map[string]any{"json": input})
}

func (r *Rpc) Call(ctx context.Context, procedure string, input any) (any, error) {
	body, err := encode(input)
	if err != nil {
		return nil, err
	}
	value, _, err := r.callRawTimed(ctx, procedure, body)
	return value, err
}

func (r *Rpc) callRawTimed(ctx context.Context, procedure string, body []byte) (any, CallTiming, error) {
	started := time.Now()
	response, info, err := r.send(ctx, procedure, body)
	if err != nil {
		return nil, CallTiming{}, err
	}
	headers := time.Since(started)
	bodyStarted := time.Now()
	value, err := readResponse(procedure, response)
	if err != nil {
		return nil, CallTiming{}, err
	}
	timing := CallTiming{
		Headers:      headers,
		Body:         time.Since(bodyStarted),
		Total:        time.Since(started),
		Version:      info.Version,
		VercelID:     info.VercelID,
		ServerTiming: info.ServerTiming,
	}
	if info.Peer != "" {
		timing.Peer = &info.Peer
	}
	return value, timing, nil
}

// Inspect calls a procedure and returns the response's transport metadata.
func (r *Rpc) Inspect(ctx context.Context, procedure string, input any) (any, ResponseInfo, error) {
	body, err := encode(input)
	if err != nil {
		return nil, ResponseInfo{}, err
	}
	response, info, err := r.send(ctx, procedure, body)
	if err != nil {
		return nil, info, err
	}
	value, err := readResponse(procedure, response)
	return value, info, err
}

func header(response *http.Response, name string) *string {
	if values := response.Header.Values(name); len(values) > 0 {
		return &values[0]
	}
	return nil
}

func (r *Rpc) send(ctx context.Context, procedure string, body []byte) (*http.Response, ResponseInfo, error) {
	var info ResponseInfo
	ctx = httptrace.WithClientTrace(ctx, &httptrace.ClientTrace{
		GotConn: func(conn httptrace.GotConnInfo) { info.Peer = conn.Conn.RemoteAddr().String() },
	})
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, r.endpoint+procedure, bytes.NewReader(body))
	if err != nil {
		return nil, info, err
	}
	request.Header.Set("content-type", "application/json")
	request.Header.Set("x-product-id", ProductID)
	request.Header.Set("origin", Origin)
	request.Header.Set("referer", Origin+"/")
	request.Header.Set("user-agent", r.userAgent)
	if r.cookie != "" {
		request.Header.Set("cookie", r.cookie)
	}
	response, err := r.client.Do(request)
	if err != nil {
		return nil, info, fmt.Errorf("%s: request failed: %w", procedure, err)
	}
	info.Version = response.Proto
	info.VercelID = header(response, "x-vercel-id")
	info.ServerTiming = header(response, "server-timing")
	return response, info, nil
}

func readResponse(procedure string, response *http.Response) (any, error) {
	defer response.Body.Close()
	raw, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, fmt.Errorf("%s: body lost: %w", procedure, err)
	}
	envelope, err := Decode(raw)
	if err != nil {
		envelope = nil
	}
	value := envelope
	if object, ok := envelope.(map[string]any); ok {
		if inner, ok := object["json"]; ok {
			value = inner
		}
	}
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		return nil, &Error{Procedure: procedure, Status: response.StatusCode, Body: value}
	}
	return value, nil
}

// Decode parses JSON keeping numbers as written (ids, scores), not float64.
func Decode(raw []byte) (any, error) {
	var value any
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	err := decoder.Decode(&value)
	return value, err
}

// Text is the JSON text of a value ("null" when absent).
func Text(value any) string {
	raw, err := json.Marshal(value)
	if err != nil {
		return "null"
	}
	return string(raw)
}

// ThrottlePause is the wait before resending after a 429 when nothing else
// is in flight.
const ThrottlePause = 100 * time.Millisecond

type outcome struct {
	route  int
	value  any
	timing CallTiming
	err    error
}

// Hedged is a hedged call: the network path sometimes stalls or drops a
// request, which costs the 2 s deadline. The server dedupes a repeated answer
// (`isReplay`), so a duplicate goes out on the next route whenever the
// in-flight ones are slower than `hedgeAfter` (or failed), and the first
// response wins.
func Hedged(ctx context.Context, routes []*Rpc, procedure string, input any,
	hedgeAfter time.Duration, maxRequests int) (*HedgedResponse, error) {
	body, err := encode(input)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithCancel(ctx)
	defer cancel() // drops the losers
	results := make(chan outcome, maxRequests)
	inFlight, sent := 0, 0
	var lastError error
	// Set by a 429: no more duplicates, and the requests still in flight decide.
	throttled := false
	for {
		if sent < maxRequests && (!throttled || inFlight == 0) {
			if throttled {
				time.Sleep(ThrottlePause)
			}
			route := sent % len(routes)
			go func() {
				value, timing, err := routes[route].callRawTimed(ctx, procedure, body)
				results <- outcome{route, value, timing, err}
			}()
			sent++
			inFlight++
		} else if inFlight == 0 {
			if lastError == nil {
				lastError = fmt.Errorf("%s: no route answered", procedure)
			}
			return nil, lastError
		}
		var next outcome
		if sent < maxRequests && !throttled {
			timer := time.NewTimer(hedgeAfter)
			select {
			case next = <-results:
				timer.Stop()
			case <-timer.C:
				continue // slow: send a duplicate
			}
		} else {
			next = <-results
		}
		inFlight--
		switch status := StatusOf(next.err); {
		case next.err == nil:
			return &HedgedResponse{Value: next.value, Sent: sent, WinnerRoute: next.route, Timing: next.timing}, nil
		case status == 429:
			throttled = true
			lastError = next.err
		case status != 0:
			return nil, next.err
		default:
			lastError = next.err // transport failure: next route now
		}
	}
}
