// Command agentwars is the SuperChallenge Agents War racer.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"time"

	"agentwars/internal/app"
	"agentwars/internal/solvers"
	"agentwars/internal/team"
	"agentwars/internal/web"
)

const usage = `SuperChallenge Agents War racer

Usage: agentwars <command> [flags]

Commands:
  race          Run the race. Spends one attempt.
  dry-run       Everything up to the race without starting it: Turnstile token,
                warm connections, plays left. Spends no attempt.
  bench         Network setup diagnostics and API endpoint timings. Spends no attempt.
  cookie-bench  Compare warmed requests with and without Chrome's cookies. Spends no attempt.
  team          Every interval, the next member of a CSV of nickname,email races
                --attempts times. Spends those attempts each turn and runs until stopped.
  solve         Solve one prompt offline: agentwars solve "<prompt>"
  serve         Container default: races non-stop, --attempts per generated
                {prefix}{i}@amundi.com email, and serves a read-only page that
                streams the current race. Spends attempts until stopped.

Run "agentwars <command> -h" for the flags of a command.
`

func main() {
	if err := run(os.Args[1:]); err != nil {
		if !errors.Is(err, flag.ErrHelp) {
			fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		}
		os.Exit(1)
	}
}

func parse(f *app.Flags, args []string, validate func()) error {
	if err := f.Parse(args); err != nil {
		return err
	}
	validate()
	return f.Err()
}

func run(args []string) error {
	if len(args) == 0 {
		fmt.Fprint(os.Stderr, usage)
		return errors.New("missing command")
	}
	ctx := context.Background()
	command, args := args[0], args[1:]
	f := app.NewFlags(command)
	switch command {
	case "solve":
		if err := parse(f, args, func() {}); err != nil {
			return err
		}
		if f.NArg() != 1 {
			return errors.New(`usage: agentwars solve "<prompt>"`)
		}
		answer, ok := solvers.Solve(f.Arg(0))
		if !ok {
			return errors.New("no exact solver for this prompt")
		}
		fmt.Println(answer)
		return nil

	case "bench":
		var common app.Common
		var rounds int
		common.Register(f)
		f.Int(&rounds, "rounds", "", 20, "getCompetition samples per route")
		if err := parse(f, args, func() { common.Validate(f); f.Range("rounds", rounds, 0, 1<<31) }); err != nil {
			return err
		}
		return app.Bench(ctx, &common, rounds)

	case "cookie-bench":
		var common app.Common
		var rounds int
		common.Register(f)
		f.Int(&rounds, "rounds", "", 30, "paired getCompetition calls (1-100)")
		if err := parse(f, args, func() { common.Validate(f); f.Range("rounds", rounds, 1, 100) }); err != nil {
			return err
		}
		return app.CookieBench(ctx, &common, rounds)

	case "dry-run":
		var common app.Common
		common.Register(f)
		if err := parse(f, args, func() { common.Validate(f) }); err != nil {
			return err
		}
		return app.DryRun(ctx, &common)

	case "race":
		var race app.RaceArgs
		race.Register(f)
		if err := parse(f, args, func() { race.Validate(f) }); err != nil {
			return err
		}
		return app.RunRace(ctx, &race)

	case "team":
		var race app.RaceArgs
		var csv string
		var everyMin, attempts, start int
		race.Register(f)
		f.String(&csv, "csv", "QUIZ_SC_TEAM_CSV", "", "CSV of nickname,email")
		f.Int(&everyMin, "every-min", "QUIZ_SC_TEAM_EVERY_MIN", 60, "minutes between turns")
		f.Int(&attempts, "attempts", "QUIZ_SC_TEAM_ATTEMPTS", 10, "attempts per turn (1-10)")
		f.Int(&start, "start", "", 0, "turn to start from (0 = first row), to resume the rotation after a restart")
		err := parse(f, args, func() {
			race.Validate(f)
			f.Range("every-min", everyMin, 1, 1<<31)
			f.Range("attempts", attempts, 1, 10)
			f.Range("start", start, 0, 1<<62)
		})
		if err != nil {
			return err
		}
		if csv == "" {
			return errors.New("--csv (QUIZ_SC_TEAM_CSV) is required")
		}
		return team.Run(ctx, &race, csv, time.Duration(everyMin)*time.Minute, attempts, start)

	case "serve":
		var race app.RaceArgs
		var port, attempts, emailStart int
		var host, prefix string
		race.Register(f)
		f.Int(&port, "port", "PORT", 3000, "page port")
		f.String(&host, "host", "HOST", "0.0.0.0", "page address")
		f.String(&prefix, "email-prefix", "QUIZ_SC_EMAIL_PREFIX", "ghazi_", "emails are {prefix}{i}@amundi.com; progress is saved per prefix")
		f.Int(&attempts, "attempts", "QUIZ_SC_ATTEMPTS_PER_EMAIL", 10, "races per email (1-255)")
		f.Int(&emailStart, "email-start", "QUIZ_SC_EMAIL_START", 1, "first email index; a higher index saved in the runs directory wins")
		err := parse(f, args, func() {
			race.Validate(f)
			f.Range("port", port, 0, 65535)
			f.Range("attempts", attempts, 1, 255)
			f.Range("email-start", emailStart, 0, 1<<62)
		})
		if err != nil {
			return err
		}
		return web.Serve(ctx, race, host, port, prefix, attempts, uint64(emailStart))

	case "help", "-h", "--help":
		fmt.Print(usage)
		return nil
	}
	fmt.Fprint(os.Stderr, usage)
	return fmt.Errorf("unknown command %q", command)
}
