// Package app holds the jobs behind both the CLI and the web page. Every
// setting comes from a flag or its environment variable.
package app

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"slices"
	"strconv"
	"sync"
	"time"

	"agentwars/internal/browser"
	"agentwars/internal/llm"
	"agentwars/internal/network"
	"agentwars/internal/race"
	"agentwars/internal/report"
	"agentwars/internal/rpc"
	"agentwars/internal/solvers"
)

// UserAgent is used when no browser is involved (bench, or a token passed in).
const UserAgent = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) " +
	"Chrome/149.0.0.0 Safari/537.36"

// Flags registers flags whose defaults come from environment variables; an
// empty variable counts as unset. Invalid environment values are reported by
// Err once parsing is done.
type Flags struct {
	*flag.FlagSet
	errs []error
}

func NewFlags(name string) *Flags {
	return &Flags{FlagSet: flag.NewFlagSet(name, flag.ContinueOnError)}
}

func (f *Flags) String(p *string, name, env, def, usage string) {
	if value := os.Getenv(env); env != "" && value != "" {
		def = value
	}
	f.StringVar(p, name, def, usage+envNote(env))
}

func (f *Flags) Int(p *int, name, env string, def int, usage string) {
	if value := os.Getenv(env); env != "" && value != "" {
		parsed, err := strconv.Atoi(value)
		if err != nil {
			f.errs = append(f.errs, fmt.Errorf("invalid %s %q: %w", env, value, err))
		}
		def = parsed
	}
	f.IntVar(p, name, def, usage+envNote(env))
}

func envNote(env string) string {
	if env == "" {
		return ""
	}
	return " [env " + env + "]"
}

// Range rejects a flag value outside [low, high].
func (f *Flags) Range(name string, value, low, high int) {
	if value < low || value > high {
		f.errs = append(f.errs, fmt.Errorf("--%s %d is not in %d..=%d", name, value, low, high))
	}
}

func (f *Flags) Err() error {
	return errors.Join(f.errs...)
}

type Common struct {
	URL               string
	Chrome            string
	CDPURL            string // attach to a running Chrome instead of launching one
	Profile           string // Chrome profile directory to reuse (default: a throwaway one)
	TurnstileTimeoutS int
	Connections       int
	NetworkProbes     int
}

func (c *Common) Register(f *Flags) {
	f.String(&c.URL, "url", "QUIZ_SC_URL", "https://superchallenge.io/play/SUPERCHALLENGE-JAWVUX", "play page")
	f.String(&c.Chrome, "chrome", "QUIZ_SC_CHROME", "chromium", "Chrome binary")
	f.String(&c.CDPURL, "cdp-url", "QUIZ_SC_CDP_URL", "", "attach to a running Chrome instead of launching one")
	f.String(&c.Profile, "profile", "QUIZ_SC_PROFILE", "", "Chrome profile directory to reuse (default: a throwaway one)")
	f.Int(&c.TurnstileTimeoutS, "turnstile-timeout-s", "QUIZ_SC_TURNSTILE_TIMEOUT_S", 60, "Turnstile timeout in seconds")
	f.Int(&c.Connections, "connections", "QUIZ_SC_CONNECTIONS", 2, "independent routes used by bench only; races always use two")
	f.Int(&c.NetworkProbes, "network-probes", "QUIZ_SC_NETWORK_PROBES", 3, "fresh curl connection probes for bench (0 disables; requires curl >= 7.83)")
}

func (c *Common) Validate(f *Flags) {
	f.Range("connections", c.Connections, 2, 16)
	f.Range("network-probes", c.NetworkProbes, 0, 10)
	f.Range("turnstile-timeout-s", c.TurnstileTimeoutS, 0, 1<<31)
}

type RaceArgs struct {
	Common
	Email          string
	Nickname       string // leaderboard name; without it the score is not submitted
	Locale         string
	HedgeMs        int // send a duplicate answer on the other connection after this long
	MaxRequests    int
	RunsDir        string
	TurnstileToken string // use this token instead of getting one from Chrome
}

func (r *RaceArgs) Register(f *Flags) {
	r.Common.Register(f)
	f.String(&r.Email, "email", "QUIZ_SC_EMAIL", "", "email of the attempt")
	f.String(&r.Nickname, "nickname", "QUIZ_SC_NICKNAME", "", "leaderboard name; without it the score is not submitted")
	f.String(&r.Locale, "locale", "QUIZ_SC_LOCALE", "fr", "question locale")
	f.Int(&r.HedgeMs, "hedge-ms", "QUIZ_SC_HEDGE_MS", 150, "send a duplicate answer on the other connection after this long")
	f.Int(&r.MaxRequests, "max-requests", "QUIZ_SC_MAX_REQUESTS", 4, "requests per answer, duplicates and retries included")
	f.String(&r.RunsDir, "runs-dir", "QUIZ_SC_RUNS_DIR", "runs", "where race logs go")
	f.String(&r.TurnstileToken, "turnstile-token", "QUIZ_SC_TURNSTILE_TOKEN", "", "use this Turnstile token instead of getting one from Chrome")
}

func (r *RaceArgs) Validate(f *Flags) {
	r.Common.Validate(f)
	f.Range("hedge-ms", r.HedgeMs, 0, 1<<31)
	f.Range("max-requests", r.MaxRequests, 0, 1<<31)
}

func credentials(ctx context.Context, common *Common) (*browser.Credentials, error) {
	started := time.Now()
	c, err := browser.Get(ctx, common.Chrome, common.CDPURL, common.Profile, common.URL,
		time.Duration(common.TurnstileTimeoutS)*time.Second)
	if err != nil {
		return nil, err
	}
	report.Say("turnstile token in %.1fs (%d chars, %d cookie bytes)",
		time.Since(started).Seconds(), len(c.TurnstileToken), len(c.Cookie))
	return c, nil
}

func ms(d time.Duration) float64 {
	return d.Seconds() * 1000
}

func Bench(ctx context.Context, common *Common, rounds int) error {
	code := race.CompetitionCode(common.URL)
	if err := network.Bench(ctx, code, UserAgent, common.NetworkProbes); err != nil {
		return err
	}
	session, err := race.NewSession(code, UserAgent, "", common.Connections)
	if err != nil {
		return err
	}
	competition, err := session.Warm(ctx)
	if err != nil {
		return err
	}
	race.ReportCompetition(competition)
	report.Say("warm getCompetition endpoint RTT: includes server work; not network-only or submitAnswerV2 latency")
	all, err := session.Bench(ctx, max(rounds, 1))
	if err != nil {
		return err
	}
	for index, timings := range all {
		if len(timings) == 0 {
			continue
		}
		slices.Sort(timings)
		n := len(timings)
		report.Say("getCompetition route %d: min %.1f p50 %.1f p90 %.1f max %.1f ms",
			index, ms(timings[0]), ms(timings[n/2]), ms(timings[n*9/10]), ms(timings[n-1]))
	}
	return nil
}

func CookieBench(ctx context.Context, common *Common, rounds int) error {
	c, err := credentials(ctx, common)
	if err != nil {
		return err
	}
	input := map[string]any{"productId": rpc.ProductID, "code": race.CompetitionCode(common.URL)}
	withCookie := rpc.New(c.UserAgent, c.Cookie)
	withoutCookie := rpc.New(c.UserAgent, "")
	// Both requests of a pair go out at the same time.
	pair := func() (time.Duration, time.Duration, error) {
		var wg sync.WaitGroup
		var withElapsed, withoutElapsed time.Duration
		var withErr, withoutErr error
		wg.Go(func() {
			started := time.Now()
			_, withErr = withCookie.Call(ctx, "getCompetition", input)
			withElapsed = time.Since(started)
		})
		wg.Go(func() {
			started := time.Now()
			_, withoutErr = withoutCookie.Call(ctx, "getCompetition", input)
			withoutElapsed = time.Since(started)
		})
		wg.Wait()
		return withElapsed, withoutElapsed, errors.Join(withErr, withoutErr)
	}
	if _, _, err := pair(); err != nil {
		return err
	}
	var withTimings, withoutTimings []time.Duration
	delta := 0.0
	for range rounds {
		with, without, err := pair()
		if err != nil {
			return err
		}
		withTimings = append(withTimings, with)
		withoutTimings = append(withoutTimings, without)
		delta += ms(with) - ms(without)
	}
	stats := func(timings []time.Duration) (float64, float64, float64) {
		slices.Sort(timings)
		var total time.Duration
		for _, t := range timings {
			total += t
		}
		n := len(timings)
		return ms(total / time.Duration(n)), ms(timings[n/2]), ms(timings[n*9/10])
	}
	withMean, withP50, withP90 := stats(withTimings)
	withoutMean, withoutP50, withoutP90 := stats(withoutTimings)
	report.Say("cookie A/B: %d paired getCompetition calls; no attempt started", rounds)
	report.Say("with cookie:    mean %.1f p50 %.1f p90 %.1f ms", withMean, withP50, withP90)
	report.Say("without cookie: mean %.1f p50 %.1f p90 %.1f ms", withoutMean, withoutP50, withoutP90)
	report.Say("paired mean delta (with - without): %+.1f ms", delta/float64(rounds))
	return nil
}

func warmSolvers() {
	started := time.Now()
	solvers.Warm()
	report.Say("solvers warmed in %.2f ms", ms(time.Since(started)))
}

func DryRun(ctx context.Context, common *Common) error {
	warmSolvers()
	c, err := credentials(ctx, common)
	if err != nil {
		return err
	}
	session, err := race.NewSession(race.CompetitionCode(common.URL), c.UserAgent, c.Cookie, 2)
	if err != nil {
		return err
	}
	competition, err := session.Warm(ctx)
	if err != nil {
		return err
	}
	race.ReportCompetition(competition)
	report.Say("dry run OK: user agent %s", c.UserAgent)
	return nil
}

func RunRace(ctx context.Context, args *RaceArgs) error {
	if args.Email == "" {
		return errors.New("QUIZ_SC_EMAIL is not set")
	}
	warmSolvers()
	token, userAgent, cookie := args.TurnstileToken, UserAgent, ""
	if token == "" {
		c, err := credentials(ctx, &args.Common)
		if err != nil {
			return err
		}
		token, userAgent, cookie = c.TurnstileToken, c.UserAgent, c.Cookie
	}
	code := race.CompetitionCode(args.URL)
	session, err := race.NewSession(code, userAgent, cookie, 2)
	if err != nil {
		return err
	}
	competition, err := session.Warm(ctx)
	if err != nil {
		return err
	}
	race.ReportCompetition(competition)
	config := &race.Config{
		Code:        code,
		Email:       args.Email,
		Nickname:    args.Nickname,
		Locale:      args.Locale,
		HedgeAfter:  time.Duration(args.HedgeMs) * time.Millisecond,
		MaxRequests: max(args.MaxRequests, 1),
		RunsDir:     args.RunsDir,
	}
	return race.Race(ctx, session, config, token, llm.FromEnv())
}
