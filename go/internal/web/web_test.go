package web

import (
	"os"
	"path/filepath"
	"testing"
)

func TestEmailsFollowTheIndex(t *testing.T) {
	if Email("ghazi.", 7) != "ghazi.7@amundi.com" || Email("studio_", 7) != "studio_7@amundi.com" {
		t.Fatal(Email("ghazi.", 7))
	}
	if _, err := CleanEmail(Email("studio_", 0)); err != nil {
		t.Fatal(err)
	}
	if _, err := CleanNickname(Nickname); err != nil {
		t.Fatal(err)
	}
}

func TestEachEmailGetsItsAttemptsThenTheNextStarts(t *testing.T) {
	p := position{3, 1}
	for expected := 2; expected <= 10; expected++ {
		p = p.next(10)
		if p != (position{3, expected}) {
			t.Fatal(p)
		}
	}
	if p.next(10) != (position{4, 1}) {
		t.Fatal(p.next(10))
	}
}

func TestTheSavedPositionResumesPerPrefixButNeverGoesBelowStart(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "runs")
	if got := loadPosition(dir, "ghazi.", 5); got != (position{5, 1}) {
		t.Fatal(got)
	}
	savePosition(dir, "ghazi.", position{42, 7})
	if _, err := os.Stat(filepath.Join(dir, "next_race_ghazi")); err != nil {
		t.Fatal(err)
	}
	for _, c := range []struct {
		prefix string
		start  uint64
		want   position
	}{
		{"ghazi.", 5, position{42, 7}},
		{"studio_", 5, position{5, 1}},
		{"ghazi.", 100, position{100, 1}},
	} {
		if got := loadPosition(dir, c.prefix, c.start); got != c.want {
			t.Errorf("%+v: %+v", c, got)
		}
	}
}
