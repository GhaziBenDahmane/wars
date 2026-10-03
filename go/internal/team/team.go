// Package team runs one member's attempts per tick, rotating through a CSV of
// `nickname,email`, so attempts land at all hours, including the quiet ones.
package team

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"agentwars/internal/app"
	"agentwars/internal/report"
	"agentwars/internal/web"
)

type Member struct {
	Nickname string
	Email    string
}

// Parse reads `nickname,email` per line. Blank lines, `#` comments and a
// `nickname,email` header are skipped.
func Parse(csv string) ([]Member, error) {
	var members []Member
	for index, line := range strings.Split(strings.ReplaceAll(csv, "\r\n", "\n"), "\n") {
		line = strings.TrimLeft(strings.TrimSpace(line), "\ufeff")
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		cut := strings.LastIndex(line, ",")
		if cut < 0 {
			return nil, fmt.Errorf("line %d: expected nickname,email", index+1)
		}
		nickname, email := line[:cut], line[cut+1:]
		if strings.EqualFold(strings.TrimSpace(nickname), "nickname") {
			continue
		}
		nickname, err := web.CleanNickname(strings.Trim(strings.TrimSpace(nickname), `"`))
		if err != nil {
			return nil, fmt.Errorf("line %d: %w", index+1, err)
		}
		email, err = web.CleanEmail(strings.Trim(strings.TrimSpace(email), `"`))
		if err != nil {
			return nil, fmt.Errorf("line %d: %w", index+1, err)
		}
		members = append(members, Member{nickname, email})
	}
	if len(members) == 0 {
		return nil, errors.New("no team member in the CSV")
	}
	return members, nil
}

// Run never returns on its own. The first turn starts right away; the next
// ones start one interval after the previous start, whatever the outcome (a
// turn that overruns delays the next ones). A turn runs `attempts` races back
// to back for the same member.
func Run(ctx context.Context, args *app.RaceArgs, csv string, every time.Duration, attempts, start int) error {
	text, err := os.ReadFile(csv)
	if err != nil {
		return err
	}
	members, err := Parse(string(text))
	if err != nil {
		return err
	}
	report.Say("team mode: %d members, %d attempts every %d min", len(members), attempts, int(every.Minutes()))
	due := time.Now()
	for turn := start; ; turn++ {
		if wait := time.Until(due); wait > 0 {
			time.Sleep(wait)
		} else {
			due = time.Now()
		}
		due = due.Add(every)
		member := members[turn%len(members)]
		raceArgs := *args
		raceArgs.Email = member.Email
		raceArgs.Nickname = member.Nickname
		for attempt := 1; attempt <= attempts; attempt++ {
			report.Clear()
			report.Say("turn %d, attempt %d/%d: %s <%s>", turn, attempt, attempts, member.Nickname, member.Email)
			if err := app.RunRace(ctx, &raceArgs); err != nil {
				report.Say("turn %d, attempt %d/%d failed: %v", turn, attempt, attempts, err)
			}
		}
	}
}
