// Package web races non-stop and serves a read-only page that streams the
// current race. Emails are generated, `{prefix}{i}@amundi.com`, and each one
// races `attempts` times before the next. The position is saved in the runs
// directory, one file per prefix, so a restart resumes where it stopped and
// several processes with different prefixes can share the directory.
package web

import (
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"agentwars/internal/app"
	"agentwars/internal/race"
	"agentwars/internal/report"
)

//go:embed page.html
var page []byte

const Nickname = "AMUNDI STU SQUAD"

// failurePause follows a failed race, so a lasting outage does not spin.
const failurePause = 10 * time.Second

func Email(prefix string, index uint64) string {
	return fmt.Sprintf("%s%d@amundi.com", prefix, index)
}

// position is the next race: email index and attempt number (1-based) for
// that email.
type position struct {
	index   uint64
	attempt int
}

func (p position) next(attempts int) position {
	if p.attempt >= attempts {
		return position{p.index + 1, 1}
	}
	return position{p.index, p.attempt + 1}
}

// positionPath turns `ghazi_` into `next_race_ghazi`.
func positionPath(runsDir, prefix string) string {
	return filepath.Join(runsDir, "next_race_"+strings.TrimRight(prefix, "._-"))
}

// loadPosition is the saved position, or the first attempt of `start` if
// nothing is saved or the saved index is behind `start`.
func loadPosition(runsDir, prefix string, start uint64) position {
	text, err := os.ReadFile(positionPath(runsDir, prefix))
	if err == nil {
		if index, attempt, ok := strings.Cut(strings.TrimSpace(string(text)), " "); ok {
			i, errIndex := strconv.ParseUint(index, 10, 64)
			a, errAttempt := strconv.ParseUint(attempt, 10, 8)
			if errIndex == nil && errAttempt == nil && i >= start && a >= 1 {
				return position{i, int(a)}
			}
		}
	}
	return position{start, 1}
}

func savePosition(runsDir, prefix string, p position) {
	err := os.MkdirAll(runsDir, 0o755)
	if err == nil {
		err = os.WriteFile(positionPath(runsDir, prefix), fmt.Appendf(nil, "%d %d\n", p.index, p.attempt), 0o644)
	}
	if err != nil {
		report.Say("could not save the next race: %v", err)
	}
}

func CleanEmail(value string) (string, error) {
	email := strings.TrimSpace(value)
	parts := strings.Split(email, "@")
	valid := email != "" && len(email) <= 254 &&
		!strings.ContainsFunc(email, func(c rune) bool {
			return c < utf8.RuneSelf && (unicode.IsSpace(c) || unicode.IsControl(c))
		}) &&
		len(parts) == 2 && parts[0] != "" &&
		strings.Contains(parts[1], ".") && !strings.HasPrefix(parts[1], ".") && !strings.HasSuffix(parts[1], ".")
	if !valid {
		return "", errors.New("Enter a valid email address")
	}
	return email, nil
}

func CleanNickname(value string) (string, error) {
	nickname := strings.TrimSpace(value)
	if nickname == "" || utf8.RuneCountInString(nickname) > 40 || strings.ContainsFunc(nickname, unicode.IsControl) {
		return "", errors.New("Leaderboard name must be 1–40 characters")
	}
	return nickname, nil
}

type panel struct {
	args     app.RaceArgs
	prefix   string
	attempts int
	mu       sync.Mutex
	current  map[string]any
}

func (p *panel) status(w http.ResponseWriter, _ *http.Request) {
	p.mu.Lock()
	current := p.current
	body, err := json.Marshal(map[string]any{
		"current": current,
		"lines":   report.Lines(),
		"config": map[string]any{
			"code":     race.CompetitionCode(p.args.URL),
			"nickname": Nickname,
			"email":    strings.Replace(Email(p.prefix, 0), "0@", "{i}@", 1),
			"attempts": p.attempts,
			"hedge_ms": p.args.HedgeMs,
		},
	})
	p.mu.Unlock()
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.Header().Set("content-type", "application/json")
	w.Write(body)
}

func (p *panel) setCurrent(current map[string]any) {
	p.mu.Lock()
	p.current = current
	p.mu.Unlock()
}

func (p *panel) setState(state string) {
	p.mu.Lock()
	if p.current != nil {
		p.current["state"] = state
	}
	p.mu.Unlock()
}

// raceForever never returns: it races back to back, `attempts` per email.
func (p *panel) raceForever(ctx context.Context, start uint64) {
	at := loadPosition(p.args.RunsDir, p.prefix, start)
	at.attempt = min(at.attempt, p.attempts)
	for {
		index, attempt := at.index, at.attempt
		email := Email(p.prefix, index)
		args := p.args
		args.Email = email
		args.Nickname = Nickname
		at = at.next(p.attempts)
		// Saved before racing: a crash mid-race must not repeat this attempt.
		savePosition(args.RunsDir, p.prefix, at)
		p.setCurrent(map[string]any{"index": index, "email": email, "attempt": attempt, "state": "racing"})
		report.Clear()
		report.Say("%s <%s>: attempt %d/%d", Nickname, email, attempt, p.attempts)
		if err := app.RunRace(ctx, &args); err != nil {
			report.Say("error: %v", err)
			p.setState("failed")
			if !slices.Contains(report.Lines(), race.StartingRun) {
				// Failed before startRunV2 (Turnstile, warmup): retry this attempt.
				at = position{index, attempt}
				savePosition(args.RunsDir, p.prefix, at)
			}
			time.Sleep(failurePause)
		}
	}
}

func Serve(ctx context.Context, args app.RaceArgs, host string, port int, prefix string, attempts int, start uint64) error {
	if _, err := CleanEmail(Email(prefix, 0)); err != nil {
		return fmt.Errorf("QUIZ_SC_EMAIL_PREFIX %q does not make an email: %w", prefix, err)
	}
	p := &panel{args: args, prefix: prefix, attempts: attempts}
	address := net.JoinHostPort(host, strconv.Itoa(port))
	listener, err := net.Listen("tcp", address)
	if err != nil {
		return err
	}
	go p.raceForever(ctx, start)
	mux := http.NewServeMux()
	mux.HandleFunc("GET /{$}", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("content-type", "text/html; charset=utf-8")
		w.Write(page)
	})
	mux.HandleFunc("GET /status", p.status)
	fmt.Fprintf(os.Stderr, "live page on http://%s\n", address)
	return http.Serve(listener, mux)
}
