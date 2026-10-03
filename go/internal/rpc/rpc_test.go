package rpc

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"sync/atomic"
	"testing"
	"time"
)

var ctx = context.Background()

func TestInspectionPreservesTheBodyAndExposesActualResponseMetadata(t *testing.T) {
	listener, _ := net.Listen("tcp", "127.0.0.1:0")
	mux := http.NewServeMux()
	mux.HandleFunc("POST /api/rpc/superchallenge/getCompetition", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("x-vercel-id", "fra1::fra1::test")
		w.Header().Set("server-timing", "app;dur=40")
		fmt.Fprint(w, `{"json":{"title":"test"}}`)
	})
	server := &http.Server{Handler: mux}
	go server.Serve(listener)
	defer server.Close()
	route := WithOrigin("http://"+listener.Addr().String(), "test", "")
	body, info, err := route.Inspect(ctx, "getCompetition", map[string]any{})
	if err != nil {
		t.Fatal(err)
	}
	if body.(map[string]any)["title"] != "test" || info.Version != "HTTP/1.1" ||
		info.Peer != listener.Addr().String() || *info.VercelID != "fra1::fra1::test" ||
		*info.ServerTiming != "app;dur=40" {
		t.Fatalf("%v %+v", body, info)
	}
}

func TestTimedCallSeparatesResponseHeadersFromBodyReading(t *testing.T) {
	listener, _ := net.Listen("tcp", "127.0.0.1:0")
	done := make(chan struct{})
	go func() {
		defer close(done)
		socket, _ := listener.Accept()
		defer socket.Close()
		buffer := make([]byte, 4096)
		socket.Read(buffer)
		time.Sleep(20 * time.Millisecond)
		socket.Write([]byte("HTTP/1.1 200 OK\r\ncontent-length: 20\r\n\r\n"))
		time.Sleep(30 * time.Millisecond)
		socket.Write([]byte(`{"json":{"ok":true}}`))
	}()
	route := WithOrigin("http://"+listener.Addr().String(), "test", "")
	value, timing, err := route.callRawTimed(ctx, "p", []byte(`{"json":{}}`))
	<-done
	if err != nil {
		t.Fatal(err)
	}
	if value.(map[string]any)["ok"] != true {
		t.Fatal(value)
	}
	if timing.Headers < 15*time.Millisecond || timing.Body < 20*time.Millisecond ||
		timing.Total < timing.Headers+timing.Body {
		t.Fatalf("%+v", timing)
	}
	if timing.Version != "HTTP/1.1" || *timing.Peer != listener.Addr().String() {
		t.Fatalf("%+v", timing)
	}
}

// server is a one-request-per-connection server: request `n` (0-based) waits
// `delays[n]` ms, then answers `status` with `body`.
func server(t *testing.T, delays []int, status int, body string) (string, *atomic.Int64) {
	listener, _ := net.Listen("tcp", "127.0.0.1:0")
	t.Cleanup(func() { listener.Close() })
	count := new(atomic.Int64)
	go func() {
		for {
			socket, err := listener.Accept()
			if err != nil {
				return
			}
			n := int(count.Add(1) - 1)
			delay := 0
			if n < len(delays) {
				delay = delays[n]
			}
			go func() {
				defer socket.Close()
				buffer := make([]byte, 4096)
				socket.Read(buffer)
				time.Sleep(time.Duration(delay) * time.Millisecond)
				fmt.Fprintf(socket, "HTTP/1.1 %d X\r\ncontent-length: %d\r\nconnection: close\r\n\r\n%s", status, len(body), body)
			}()
		}
	}()
	return "http://" + listener.Addr().String(), count
}

func routes(origin string) []*Rpc {
	return []*Rpc{WithOrigin(origin, "test", ""), WithOrigin(origin, "test", "")}
}

func TestASlowRequestIsHedged(t *testing.T) {
	origin, count := server(t, []int{2000, 0}, 200, `{"json":{"isCorrect":true}}`)
	started := time.Now()
	reply, err := Hedged(ctx, routes(origin), "p", map[string]any{}, 100*time.Millisecond, 4)
	if err != nil {
		t.Fatal(err)
	}
	if reply.Value.(map[string]any)["isCorrect"] != true || reply.Sent != 2 || reply.WinnerRoute != 1 {
		t.Fatalf("%+v", reply)
	}
	if time.Since(started) >= time.Second || count.Load() != 2 {
		t.Fatalf("%v %d", time.Since(started), count.Load())
	}
}

func TestAServerVerdictIsNotRetried(t *testing.T) {
	origin, count := server(t, nil, 409, `{"json":{"code":"CONFLICT"}}`)
	_, err := Hedged(ctx, routes(origin), "p", map[string]any{}, 500*time.Millisecond, 4)
	if StatusOf(err) != 409 || count.Load() != 1 {
		t.Fatalf("%v %d", err, count.Load())
	}
}

func TestRequestsAreCapped(t *testing.T) {
	origin, count := server(t, []int{400, 400, 400, 400, 400, 400, 400, 400, 400, 400}, 200, `{"json":1}`)
	reply, err := Hedged(ctx, routes(origin), "p", map[string]any{}, 50*time.Millisecond, 3)
	if err != nil {
		t.Fatal(err)
	}
	if reply.Sent != 3 || count.Load() != 3 {
		t.Fatalf("%d %d", reply.Sent, count.Load())
	}
}

func TestA429StopsTheDuplicatesAndRetriesAlone(t *testing.T) {
	origin, count := server(t, []int{0, 0, 0, 0}, 429, `{"json":{"message":"Too Many Requests"}}`)
	route := WithOrigin(origin, "test", "")
	started := time.Now()
	_, err := Hedged(ctx, []*Rpc{route, route}, "p", map[string]any{}, 5*time.Millisecond, 3)
	if err == nil || count.Load() != 3 {
		t.Fatalf("%v %d", err, count.Load())
	}
	if elapsed := time.Since(started); elapsed < 2*ThrottlePause {
		t.Fatal(elapsed)
	}
}
