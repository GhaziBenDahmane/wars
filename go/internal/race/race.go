// Package race is the race loop. Everything off the hot path happens before
// `startRunV2` (Turnstile, two warm routes, solver warmup, Chrome killed); in
// the loop a question costs one solve (microseconds) plus one hedged round
// trip. Logs stay in memory and are written once the run is over.
package race

import (
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"time"

	"agentwars/internal/llm"
	"agentwars/internal/report"
	"agentwars/internal/rpc"
	"agentwars/internal/solvers"
)

const (
	// safety is the margin kept between an LLM fallback and the question deadline.
	safety = 250 * time.Millisecond
	// llmFloor is the time an LLM fallback always gets, even past the
	// deadline: the alternative is "?", which is wrong anyway, so a late
	// answer can only help.
	llmFloor       = 4 * time.Second
	prepareTimeout = 30 * time.Second
)

// StartingRun is logged right before `startRunV2`; a failure without it
// spent no attempt.
const StartingRun = "starting the run"

type Object = map[string]any

type Config struct {
	Code        string
	Email       string
	Nickname    string
	Locale      string
	HedgeAfter  time.Duration
	MaxRequests int
	RunsDir     string
}

type Session struct {
	Routes []*rpc.Rpc
	Base   Object
}

// CompetitionCode turns `/play/SUPERCHALLENGE-JAWVUX` into `JAWVUX` (the slug
// is cosmetic).
func CompetitionCode(playURL string) string {
	path := playURL
	if i := strings.IndexAny(path, "?#"); i >= 0 {
		path = path[:i]
	}
	path = strings.TrimRight(path, "/")
	segment := path[strings.LastIndex(path, "/")+1:]
	return segment[strings.LastIndex(segment, "-")+1:]
}

func field(value any, key string) any {
	if object, ok := value.(Object); ok {
		return object[key]
	}
	return nil
}

func number(value any) (float64, bool) {
	switch n := value.(type) {
	case json.Number:
		f, err := n.Float64()
		return f, err == nil
	case float64:
		return n, true
	case int:
		return float64(n), true
	}
	return 0, false
}

// DeadlineMs is the per-question deadline; it mirrors the client's linear
// ramp exactly.
func DeadlineMs(agentWars any, answered int) float64 {
	floor := 2.0
	if f, ok := number(field(agentWars, "questionDeadlineSec")); ok {
		floor = f
	}
	floor *= 1000
	start := floor
	if s, ok := number(field(agentWars, "questionDeadlineStartSec")); ok {
		start = 1000 * s
	}
	ramp := 0.0
	if r, ok := number(field(agentWars, "questionDeadlineRampQuestions")); ok && r >= 0 && r == math.Trunc(r) {
		ramp = r
	}
	if ramp <= 0 || start <= floor {
		return math.Round(floor)
	}
	return math.Round(start - (start-floor)*math.Min(float64(answered), ramp)/ramp)
}

func objects(list any) []Object {
	var found []Object
	items, _ := list.([]any)
	for _, item := range items {
		if object, ok := item.(Object); ok {
			found = append(found, object)
		}
	}
	return found
}

// FindDrills returns the drills of an RPC response: `drills`, `next`, or
// nested one level down.
func FindDrills(value any) []Object {
	found := objects(field(value, "drills"))
	if next, ok := field(value, "next").(Object); ok {
		found = append(found, next)
	}
	if object, ok := value.(Object); ok && len(found) == 0 {
		keys := make([]string, 0, len(object))
		for key := range object {
			keys = append(keys, key)
		}
		slices.Sort(keys)
		for _, key := range keys {
			found = append(found, objects(field(object[key], "drills"))...)
		}
	}
	return found
}

func DrillPrompt(drill Object) string {
	data := field(drill, "patternData")
	if prompt, ok := field(data, "prompt").(string); ok {
		return prompt
	}
	return rpc.Text(data)
}

func now() float64 {
	return float64(time.Now().UnixNano()) / 1e9
}

type runLog struct {
	lines []string
}

func (l *runLog) write(event string, data Object) {
	data["t"] = now()
	data["event"] = event
	l.lines = append(l.lines, rpc.Text(data))
}

func (l *runLog) save(dir string) (string, error) {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	path := filepath.Join(dir, fmt.Sprintf("race-%d.jsonl", time.Now().Unix()))
	var text strings.Builder
	for _, line := range l.lines {
		text.WriteString(line)
		text.WriteByte('\n')
	}
	return path, os.WriteFile(path, []byte(text.String()), 0o644)
}

func NewSession(code, userAgent, cookie string, candidates int) (*Session, error) {
	if candidates < 2 || candidates > 16 {
		return nil, errors.New("connection candidates must be between 2 and 16")
	}
	routes := make([]*rpc.Rpc, candidates)
	for i := range routes {
		routes[i] = rpc.ForRoute(userAgent, cookie, i)
	}
	return &Session{Routes: routes, Base: Object{"productId": rpc.ProductID, "code": code}}, nil
}

// Warm opens TLS + HTTP/2 on every route and returns the competition.
func (s *Session) Warm(ctx context.Context) (any, error) {
	var competition any
	for index, route := range s.Routes {
		value, info, err := route.Inspect(ctx, "getCompetition", s.Base)
		if err != nil {
			return nil, err
		}
		info.Report(index)
		if index == 0 {
			competition = value
		}
	}
	if len(s.Routes) == 0 {
		return nil, errors.New("no route")
	}
	return competition, nil
}

// Prepare ranks every route on `getCompetition`, rechecks the best two and
// keeps their connection pools as primary and backup.
func (s *Session) Prepare(ctx context.Context, rounds int) (any, error) {
	if rounds < 3 || rounds > 10 {
		return nil, errors.New("connection samples must be between 3 and 10")
	}
	if len(s.Routes) < 2 {
		return nil, errors.New("at least two routes are required")
	}
	ctx, cancel := context.WithTimeout(ctx, prepareTimeout)
	defer cancel()
	competition, err := s.prepare(ctx, rounds)
	if err != nil && ctx.Err() != nil {
		return nil, fmt.Errorf("connection selection exceeded 30 seconds; no attempt started: %w", err)
	}
	return competition, err
}

func (s *Session) prepare(ctx context.Context, rounds int) (any, error) {
	report.Say("comparing %d connections on getCompetition, not network-only (%d samples each, then rechecking two)", len(s.Routes), rounds)
	competition, err := s.Warm(ctx)
	if err != nil {
		return nil, err
	}
	timings, err := s.Bench(ctx, rounds)
	if err != nil {
		return nil, err
	}
	ranked := rankRoutes(timings, rounds)
	if len(ranked) < 2 {
		return nil, errors.New("fewer than two routes completed all connection samples")
	}
	for _, index := range ranked {
		reportRoute("candidate", index, timings[index])
	}
	checked, err := s.probe(ctx, ranked[:2], rounds)
	if err != nil {
		return nil, err
	}
	winners := rankRoutes(checked, rounds)
	if len(winners) != 2 {
		return nil, errors.New("both finalist routes must pass the connection recheck")
	}
	for _, index := range winners {
		reportRoute("recheck", index, checked[index])
	}
	s.Routes = []*rpc.Rpc{s.Routes[winners[0]], s.Routes[winners[1]]}
	report.Say("selected route %d as primary, route %d as backup", winners[0], winners[1])
	return competition, nil
}

// Bench times round trips of the (harmless) `getCompetition` call on each route.
func (s *Session) Bench(ctx context.Context, rounds int) ([][]time.Duration, error) {
	indices := make([]int, len(s.Routes))
	for i := range indices {
		indices[i] = i
	}
	return s.probe(ctx, indices, rounds)
}

func (s *Session) probe(ctx context.Context, indices []int, rounds int) ([][]time.Duration, error) {
	timings := make([][]time.Duration, len(s.Routes))
	for round := range rounds {
		for offset := range indices {
			index := indices[(round+offset)%len(indices)]
			started := time.Now()
			_, err := s.Routes[index].Call(ctx, "getCompetition", s.Base)
			switch {
			case err == nil:
				timings[index] = append(timings[index], time.Since(started))
			case rpc.StatusOf(err) == 429:
				return nil, fmt.Errorf("connection probing throttled; no attempt started: %w", err)
			default:
				if ctx.Err() != nil {
					return nil, err
				}
				report.Say("route %d: failed after %v: %v", index, time.Since(started), err)
			}
		}
	}
	return timings, nil
}

func routeStats(timings []time.Duration) (mean, median, p90 time.Duration) {
	sorted := slices.Clone(timings)
	slices.Sort(sorted)
	var total time.Duration
	for _, t := range sorted {
		total += t
	}
	n := len(sorted)
	mean = total / time.Duration(n)
	median = (sorted[(n-1)/2] + sorted[n/2]) / 2
	p90 = sorted[int(math.Round(float64(n-1)*0.9))]
	return
}

func rankRoutes(timings [][]time.Duration, rounds int) []int {
	type key struct {
		score, median time.Duration
		index         int
	}
	var keys []key
	for index, samples := range timings {
		if rounds > 0 && len(samples) == rounds {
			mean, median, p90 := routeStats(samples)
			keys = append(keys, key{(mean + p90) / 2, median, index})
		}
	}
	slices.SortFunc(keys, func(a, b key) int {
		return cmp.Or(cmp.Compare(a.score, b.score), cmp.Compare(a.median, b.median), cmp.Compare(a.index, b.index))
	})
	ranked := make([]int, len(keys))
	for i, k := range keys {
		ranked[i] = k.index
	}
	return ranked
}

func ms(d time.Duration) float64 {
	return d.Seconds() * 1000
}

func reportRoute(stage string, index int, timings []time.Duration) {
	mean, median, p90 := routeStats(timings)
	report.Say("getCompetition %s route %d: mean %.1f p50 %.1f p90 %.1f ms", stage, index, ms(mean), ms(median), ms(p90))
}

func answer(ctx context.Context, model *llm.Llm, prompt string, budget time.Duration) (string, string) {
	if answer, ok := solvers.Solve(prompt); ok {
		return answer, "exact"
	}
	if model != nil {
		ctx, cancel := context.WithTimeout(ctx, max(budget, llmFloor))
		defer cancel()
		if answer, err := model.Answer(ctx, prompt); err == nil {
			return answer, "llm"
		}
	}
	return "?", "none"
}

type sample struct {
	source                 string
	solve, rtt             time.Duration
	correct                bool
	headers, bodyReadTimes time.Duration
}

// Race spends one attempt: `startRunV2`, then every question until the run
// ends, then the score.
func Race(ctx context.Context, session *Session, config *Config, turnstileToken string, model *llm.Llm) error {
	log := &runLog{}
	err := run(ctx, session, config, turnstileToken, model, log)
	if err != nil {
		var data any
		var rpcErr *rpc.Error
		if errors.As(err, &rpcErr) {
			data = rpcErr.Body
		}
		log.write("error", Object{"error": err.Error(), "data": data})
	}
	if path, saveErr := log.save(config.RunsDir); saveErr != nil {
		report.Say("could not write the log: %v", saveErr)
	} else {
		report.Say("log: %s", path)
	}
	return err
}

func run(ctx context.Context, session *Session, config *Config, turnstileToken string, model *llm.Llm, log *runLog) error {
	startInput := Object{
		"productId":      rpc.ProductID,
		"code":           config.Code,
		"locale":         config.Locale,
		"uiLocale":       config.Locale,
		"turnstileToken": turnstileToken,
		"email":          config.Email,
	}
	// From here on the attempt counts as spent, even if the call fails.
	report.Say("%s", StartingRun)
	raceStarted := time.Now()
	// Never duplicated: a second startRunV2 could spend a second attempt.
	start, err := session.Routes[0].Call(ctx, "startRunV2", startInput)
	if err != nil {
		return err
	}
	received := time.Now()
	startEvent := Object{"response": start}
	// Set by the Rust `serve` platform: what this race tries.
	if variant := os.Getenv("QUIZ_SC_VARIANT"); variant != "" {
		startEvent["variant"] = json.RawMessage(variant)
	}
	log.write("start", startEvent)
	runToken, ok := field(start, "runToken").(string)
	if !ok {
		return errors.New("no runToken")
	}
	agentWars := field(field(start, "setup"), "agentWars")
	queue := FindDrills(start)
	answered := 0
	rtt := 150 * time.Millisecond
	summary := make([]sample, 0, 256)
	var ended string
	for {
		if answered >= len(queue) {
			ended = "no drill left"
			break
		}
		drill := queue[answered]
		prompt := DrillPrompt(drill)
		deadline := received.Add(time.Duration(DeadlineMs(agentWars, answered)) * time.Millisecond)
		budget := max(time.Until(deadline)-rtt-safety, 0)
		thought := time.Now()
		submission, source := answer(ctx, model, prompt, budget)
		solved := time.Now()
		input := Object{
			"productId":  rpc.ProductID,
			"code":       config.Code,
			"runToken":   runToken,
			"drillId":    drill["id"],
			"submission": submission,
		}
		reply, err := rpc.Hedged(ctx, session.Routes, "submitAnswerV2", input, config.HedgeAfter, config.MaxRequests)
		if rpc.StatusOf(err) == 409 {
			// Too late: the run is over but its score can still be saved.
			report.Say("[%d] %v (%s answer %q for: %s)", answered+1, err, source, submission, prompt)
			log.write("answer", Object{"index": answered, "drill": drill, "submission": submission,
				"source": source, "error": err.Error()})
			ended = err.Error()
			break
		}
		if err != nil {
			return err
		}
		received = time.Now()
		roundTrip := received.Sub(solved)
		rtt = (rtt*7 + roundTrip*3) / 10
		response := reply.Value
		correct, _ := field(response, "isCorrect").(bool)
		timing := reply.Timing
		log.write("answer", Object{
			"index":         answered,
			"drill":         drill,
			"submission":    submission,
			"source":        source,
			"solve_ms":      ms(solved.Sub(thought)),
			"rtt_ms":        ms(roundTrip),
			"headers_ms":    ms(timing.Headers),
			"body_ms":       ms(timing.Body),
			"request_ms":    ms(timing.Total),
			"requests":      reply.Sent,
			"winner_route":  reply.WinnerRoute,
			"http_version":  timing.Version,
			"peer":          timing.Peer,
			"x_vercel_id":   timing.VercelID,
			"server_timing": timing.ServerTiming,
			"response":      response,
		})
		summary = append(summary, sample{source, solved.Sub(thought), roundTrip, correct, timing.Headers, timing.Body})
		if source != "exact" {
			verdict := "wrong"
			if correct {
				verdict = "right"
			}
			report.Say("[%d] UNKNOWN (%s, %s) answered %q for: %s", answered+1, source, verdict, submission, prompt)
		} else if !correct {
			report.Say("[%d] WRONG %q for: %s", answered+1, submission, prompt)
		}
		if !correct || field(response, "ended") != nil {
			if correct {
				answered++
			}
			ended = fmt.Sprintf("%s (score %s)", rpc.Text(field(response, "ended")), rpc.Text(field(response, "runningScore")))
			break
		}
		answered++
		for _, next := range FindDrills(response) {
			if !slices.ContainsFunc(queue, func(d Object) bool { return reflect.DeepEqual(d["id"], next["id"]) }) {
				queue = append(queue, next)
			}
		}
	}
	report.Say("run ended: %s after %d correct in %.3fs", ended, answered, time.Since(raceStarted).Seconds())
	printSummary(summary)
	if config.Nickname == "" {
		report.Say("no nickname: score not submitted to the leaderboard")
		return nil
	}
	input := Object{}
	for key, value := range session.Base {
		input[key] = value
	}
	input["runToken"] = runToken
	input["email"] = config.Email
	input["nickname"] = config.Nickname
	saved, err := session.Routes[0].Call(ctx, "submitScoreV2", input)
	if err != nil {
		return err
	}
	report.Say("score saved: %s", rpc.Text(saved))
	log.write("score", Object{"response": saved})
	return nil
}

func quantile(values []float64, q float64) float64 {
	return values[int(math.Round(float64(len(values)-1)*q))]
}

func printSummary(summary []sample) {
	if len(summary) == 0 {
		return
	}
	var rtts, headers []float64
	var solveMax, bodyMax time.Duration
	fallbacks := 0
	for _, s := range summary {
		rtts = append(rtts, ms(s.rtt))
		headers = append(headers, ms(s.headers))
		solveMax = max(solveMax, s.solve)
		bodyMax = max(bodyMax, s.bodyReadTimes)
		if s.source != "exact" {
			fallbacks++
		}
	}
	slices.Sort(rtts)
	slices.Sort(headers)
	report.Say("rtt ms: p50 %.1f p90 %.1f max %.1f | slowest solve %v | non-exact answers %d",
		quantile(rtts, 0.5), quantile(rtts, 0.9), quantile(rtts, 1), solveMax, fallbacks)
	report.Say("response headers ms: p50 %.1f p90 %.1f | slowest body read %v",
		quantile(headers, 0.5), quantile(headers, 0.9), bodyMax)
}

// ReportCompetition prints the title, plays left and the race setup.
func ReportCompetition(competition any) {
	title, ok := field(competition, "title").(string)
	if !ok {
		title = "?"
	}
	report.Say("%s | plays left: %s | agentWars: %s", title,
		rpc.Text(field(competition, "playsLeft")), rpc.Text(field(competition, "agentWars")))
}
