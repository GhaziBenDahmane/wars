package race

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"agentwars/internal/rpc"
)

var ctx = context.Background()

type seen struct {
	index     int
	procedure string
	peer      string
}

type probeState struct {
	mu            sync.Mutex
	seen          []seen
	failAt        int
	failureStatus int
}

func (s *probeState) entries() []seen {
	s.mu.Lock()
	defer s.mu.Unlock()
	return slices.Clone(s.seen)
}

func probeServer(t *testing.T, failAt, failureStatus int) (*Session, *probeState) {
	state := &probeState{failAt: failAt, failureStatus: failureStatus}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		index, _ := strconv.Atoi(r.UserAgent())
		procedure := strings.TrimPrefix(r.URL.Path, "/api/rpc/superchallenge/")
		state.mu.Lock()
		state.seen = append(state.seen, seen{index, procedure, r.RemoteAddr})
		count, total := 0, len(state.seen)
		for _, entry := range state.seen {
			if entry.index == index {
				count++
			}
		}
		state.mu.Unlock()
		if state.failAt == total {
			w.WriteHeader(state.failureStatus)
			w.Write([]byte(`{"json":{"message":"probe failed"}}`))
			return
		}
		delay := 5
		switch {
		case index == 0:
			delay = 80
		case index == 1 && count == 1:
			delay = 100
		case index == 1 && count <= 4:
			delay = 5
		case index == 1:
			delay = 70
		case index == 2 && count <= 4:
			delay = 30
		}
		time.Sleep(time.Duration(delay) * time.Millisecond)
		json.NewEncoder(w).Encode(Object{"json": Object{"route": index}})
	}))
	t.Cleanup(server.Close)
	session := &Session{Base: Object{"code": "test"}}
	for index := range 3 {
		session.Routes = append(session.Routes, rpc.WithOrigin(server.URL, strconv.Itoa(index), ""))
	}
	return session, state
}

func route(value any) string {
	return rpc.Text(value.(Object)["route"])
}

func indices(entries []seen) []int {
	var found []int
	for _, entry := range entries {
		found = append(found, entry.index)
	}
	return found
}

func allCompetition(entries []seen) bool {
	for _, entry := range entries {
		if entry.procedure != "getCompetition" {
			return false
		}
	}
	return true
}

func millis(values ...int) []time.Duration {
	var found []time.Duration
	for _, v := range values {
		found = append(found, time.Duration(v)*time.Millisecond)
	}
	return found
}

func TestRouteRankingPenalizesTailsAndExcludesIncompleteSamples(t *testing.T) {
	timings := [][]time.Duration{millis(1, 2, 3, 80, 100), millis(40, 40, 40, 40, 40), millis(30, 30, 30, 30, 30), millis(1, 1, 1, 1)}
	if got := rankRoutes(timings, 5); !slices.Equal(got, []int{2, 1, 0}) {
		t.Fatal(got)
	}
	if len(rankRoutes(timings, 0)) != 0 || len(rankRoutes([][]time.Duration{nil, nil}, 5)) != 0 {
		t.Fatal("incomplete samples ranked")
	}
	if got := rankRoutes([][]time.Duration{millis(20, 20, 20), millis(20, 20, 20)}, 3); !slices.Equal(got, []int{0, 1}) {
		t.Fatal(got)
	}
}

func TestCandidateCountIsValidatedBeforeCreatingClients(t *testing.T) {
	for _, count := range []int{0, 1, 17, 1 << 40} {
		if _, err := NewSession("test", "test", "", count); err == nil {
			t.Fatal(count)
		}
	}
}

func TestPreparationRechecksFinalistsAndReusesWinningConnections(t *testing.T) {
	session, state := probeServer(t, 0, 200)
	competition, err := session.Prepare(ctx, 3)
	if err != nil {
		t.Fatal(err)
	}
	if route(competition) != "0" || len(session.Routes) != 2 {
		t.Fatal(competition, len(session.Routes))
	}
	entries := state.entries()
	if len(entries) != 18 || !allCompetition(entries) {
		t.Fatal(entries)
	}
	if got := indices(entries[3:12]); !slices.Equal(got, []int{0, 1, 2, 1, 2, 0, 2, 0, 1}) {
		t.Fatal(got)
	}
	if got := indices(entries[12:]); !slices.Equal(got, []int{1, 2, 2, 1, 1, 2}) {
		t.Fatal(got)
	}
	for i, want := range []string{"2", "1"} {
		value, err := session.Routes[i].Call(ctx, "submitAnswerV2", Object{})
		if err != nil || route(value) != want {
			t.Fatal(value, err)
		}
	}
	entries = state.entries()
	for index := range 3 {
		peers := map[string]bool{}
		for _, entry := range entries {
			if entry.index == index {
				peers[entry.peer] = true
			}
		}
		if len(peers) != 1 {
			t.Fatalf("route %d must reuse its original connection: %v", index, peers)
		}
	}
	if entries[0].peer == entries[1].peer || entries[1].peer == entries[2].peer {
		t.Fatal("routes share a connection")
	}
}

func TestPreparationStopsImmediatelyOnThrottling(t *testing.T) {
	for _, failAt := range []int{1, 4, 13} {
		session, state := probeServer(t, failAt, 429)
		_, err := session.Prepare(ctx, 3)
		if rpc.StatusOf(err) != 429 || len(session.Routes) != 3 {
			t.Fatal(failAt, err)
		}
		if entries := state.entries(); len(entries) != failAt || !allCompetition(entries) {
			t.Fatal(failAt, entries)
		}
	}
}

func TestPreparationRejectsAnIncompleteFinalistRecheck(t *testing.T) {
	session, state := probeServer(t, 13, 500)
	_, err := session.Prepare(ctx, 3)
	if err == nil || !strings.Contains(err.Error(), "recheck") || len(session.Routes) != 3 {
		t.Fatal(err)
	}
	if !allCompetition(state.entries()) {
		t.Fatal(state.entries())
	}
}

func TestPreparationExcludesACandidateWithAFailedSample(t *testing.T) {
	session, state := probeServer(t, 4, 500)
	if _, err := session.Prepare(ctx, 3); err != nil {
		t.Fatal(err)
	}
	if len(session.Routes) != 2 || slices.Contains(indices(state.entries()[12:]), 0) {
		t.Fatal(state.entries())
	}
}

func TestPreparationRequiresTwoCompleteCandidates(t *testing.T) {
	session, state := probeServer(t, 3, 500)
	session.Routes = session.Routes[:2]
	_, err := session.Prepare(ctx, 3)
	if err == nil || !strings.Contains(err.Error(), "fewer than two routes") {
		t.Fatal(err)
	}
	if entries := state.entries(); len(entries) != 8 || !allCompetition(entries) {
		t.Fatal(entries)
	}
}

func TestPreparationValidatesSampleCountBeforeProbing(t *testing.T) {
	session, state := probeServer(t, 0, 200)
	for _, rounds := range []int{0, 1, 2, 11} {
		if _, err := session.Prepare(ctx, rounds); err == nil {
			t.Fatal(rounds)
		}
	}
	if len(state.entries()) != 0 {
		t.Fatal(state.entries())
	}
}

func TestDeadlineRampsToTheFloor(t *testing.T) {
	setup := Object{"questionDeadlineSec": json.Number("2"), "questionDeadlineStartSec": json.Number("5"),
		"questionDeadlineRampQuestions": json.Number("40")}
	for answered, want := range map[int]float64{0: 5000, 20: 3500, 199: 2000} {
		if got := DeadlineMs(setup, answered); got != want {
			t.Errorf("%d: %v", answered, got)
		}
	}
}

func TestCodeFromURL(t *testing.T) {
	if got := CompetitionCode("https://superchallenge.io/play/SUPERCHALLENGE-JAWVUX/"); got != "JAWVUX" {
		t.Fatal(got)
	}
}

func TestDrillsFromResponses(t *testing.T) {
	if len(FindDrills(Object{"next": Object{"id": "gen-2"}})) != 1 ||
		len(FindDrills(Object{"run": Object{"drills": []any{Object{"id": "a"}}}})) != 1 ||
		len(FindDrills(Object{"ended": "goal", "next": nil})) != 0 {
		t.Fatal("drills")
	}
}
