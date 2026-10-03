package team

import (
	"slices"
	"testing"
)

func TestParsesMembersAndSkipsHeaderAndComments(t *testing.T) {
	members, err := Parse("\ufeffnickname,email\n# off this week\n\nAda, ada@example.com\n\"Bob, Jr\",bob@example.com\n")
	if err != nil {
		t.Fatal(err)
	}
	want := []Member{{"Ada", "ada@example.com"}, {"Bob, Jr", "bob@example.com"}}
	if !slices.Equal(members, want) {
		t.Fatal(members)
	}
}

func TestRejectsBadLinesAndEmptyFiles(t *testing.T) {
	for _, csv := range []string{"Ada ada@example.com", "Ada,not-an-email", ",ada@example.com", "nickname,email\n"} {
		if _, err := Parse(csv); err == nil {
			t.Error(csv)
		}
	}
}
