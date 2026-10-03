package app

import "testing"

func parse(args ...string) (*RaceArgs, error) {
	var r RaceArgs
	f := NewFlags("test")
	r.Register(f)
	if err := f.Parse(args); err != nil {
		return nil, err
	}
	r.Validate(f)
	return &r, f.Err()
}

func TestPreparationAndHedgeDefaultsAreExplicit(t *testing.T) {
	for _, env := range []string{"QUIZ_SC_CONNECTIONS", "QUIZ_SC_NETWORK_PROBES", "QUIZ_SC_HEDGE_MS", "QUIZ_SC_MAX_REQUESTS"} {
		t.Setenv(env, "")
	}
	var r RaceArgs
	f := NewFlags("test")
	r.Register(f)
	for name, expected := range map[string]string{
		"connections": "2", "network-probes": "3", "hedge-ms": "150", "max-requests": "4",
	} {
		if got := f.Lookup(name).DefValue; got != expected {
			t.Errorf("%s: %s", name, got)
		}
	}
}

func TestDefaultsWithoutEnvironment(t *testing.T) {
	r, err := parse()
	if err != nil {
		t.Fatal(err)
	}
	if r.Connections != 2 || r.NetworkProbes != 3 || r.HedgeMs != 150 || r.MaxRequests != 4 {
		t.Fatalf("%+v", r)
	}
}

func TestPreparationFlagsAreBoundedAndHedgingIsOverridable(t *testing.T) {
	for _, bad := range [][2]string{
		{"--connections", "0"}, {"--connections", "1"}, {"--connections", "17"}, {"--network-probes", "11"},
	} {
		if _, err := parse(bad[0], bad[1]); err == nil {
			t.Error(bad)
		}
	}
	r, err := parse("--connections", "3", "--network-probes", "0", "--hedge-ms", "100")
	if err != nil {
		t.Fatal(err)
	}
	if r.Connections != 3 || r.NetworkProbes != 0 || r.HedgeMs != 100 {
		t.Fatalf("%+v", r)
	}
}

func TestEnvironmentOverridesDefaults(t *testing.T) {
	t.Setenv("QUIZ_SC_HEDGE_MS", "200")
	r, err := parse()
	if err != nil || r.HedgeMs != 200 {
		t.Fatal(r, err)
	}
	t.Setenv("QUIZ_SC_HEDGE_MS", "soon")
	if _, err := parse(); err == nil {
		t.Fatal("invalid environment accepted")
	}
}
