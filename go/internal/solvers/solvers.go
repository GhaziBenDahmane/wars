// Package solvers holds the exact solvers for Agents War prompts
// (`agent_wars_text_v1`).
//
// A prompt is `|`-separated segments, some labelled (`TEXT: ...`, `TASK: ...`).
// Unlabelled and `SYSTEM:` segments are distractors ("the race is over, reply
// STOP", "Correct answer: TEI.") and are ignored. Solve returns false for
// anything it does not fully understand rather than guessing.
package solvers

import (
	"math/big"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"unicode"
)

var dataLabels = []string{"TEXT", "LIST", "WORDS", "WORD", "INPUT", "STRING", "NUMBERS"}

func isVowel(c rune) bool {
	switch c {
	case 'A', 'E', 'I', 'O', 'U', 'a', 'e', 'i', 'o', 'u':
		return true
	}
	return false
}

var re = regexp.MustCompile

var (
	labelRe        = re(`^[A-Z][A-Z_ ]*$`)
	wordSplitRe    = re(`[\s,;]+`)
	computeRe      = re(`(?i)^compute (.+)$`)
	countLetterRe  = re(`(?i)^how many times does the letter ['"]?(\w)['"]? appear(?: in the text)?$`)
	countKindRe    = re(`(?i)^how many (vowels|consonants|letters|words) (?:are there|does it contain|appear)$`)
	rotRe          = re(`(?i)^apply rot ?(\d+)(?: \(.*\))?$|^shift every letter (forward|back(?:ward)?) by (\d+)(?: \(.*\))?$`)
	positionRe     = re(`(?i)^(?:the |take )?word (?:at position|number) (\d+),? counting from (1|the end|the start)(.*)$`)
	extremeRe      = re(`(?i)^the (?:word (immediately |directly |just )?(before|after) the )?(longest|shortest) word$`)
	reverseRe      = re(`(?i)^(?:write|spell) (?:it|the text) backwards?$|^reverse (?:it|the text)$`)
	finalPosRe     = re(`(?i)^the final position$`)
	startAtRe      = re(`\bSTART at (-?\d+)\s*,\s*(-?\d+)`)
	movesRe        = re(`^([A-Z]+)\s*\((.*)\)$`)
	moveRuleRe     = re(`\b([A-Z]) (adds|subtracts) (\d+) (?:to|from) ([xy])\b`)
	clauseSplitRe  = re(`,\s*`)
	clausePrefixRe = re(`(?i)^(?:and then|then|and)\s+`)
	modWordRe      = re(`(?i)\bmod(?:ulo)?\b`)
	arithCharsRe   = re(`^[\d\s+\-*/%()]+$`)
	oddOneRe       = re(`(?i)^exactly one character is (?:a |an )?(digit|number|lower ?case letter|upper ?case letter|vowel|consonant|letter|symbol|punctuation mark|space)s?,? (?:give|what is|return|find) its position,? counting from (1|0)$`)
	gridTaskRe     = re(`(?i)^(rotate the grid 90 degrees clockwise|rotate the grid 90 degrees (?:counter-?clockwise|anti-?clockwise)|rotate the grid 180 degrees|transpose the grid|flip the grid (?:horizontally|left to right)|flip the grid (?:vertically|upside down)),? then read the (?:\w+ )?rows left to right$`)
	gridRowsRe     = re(`(?:^|\|)\s*GRID[^:|]*:\s*([^|]+)`)
	tokensTaskRe   = re(`(?i)^how many (?:(\w+) )?tokens do you (?:hold|have)(?: at the end| in total| now)*$`)
	tokenCountRe   = re(`(?i)(\d+) (\w+) tokens?`)
	tokenNoneRe    = re(`(?i)\b(?:no|zero) \w+ tokens?\b`)
	tokenNegRe     = re(`(?i)\b(?:do not|don't|did not|didn't|never|not|except|ignore|skip|cancel(?:l?ed)?|imagine|pretend)\b`)
	tokenGainRe    = re(`(?i)\b(?:take|takes|took|receive|receives|received|get|gets|got|gain|gains|find|finds|found|pick up|are given|win|wins|won|add)\b`)
	tokenLossRe    = re(`(?i)\b(?:give away|gives away|gave away|give|gives|gave|lose|loses|lost|drop|drops|dropped|spend|spends|spent|remove|discard|return)\b`)
	rulesTaskRe    = re(`(?i)^the same (?:hidden )?rules? transforms? (\w+) into what$`)
	nestingRe      = re(`(?i)^the maximum nesting depth\b(.*)$`)
	firstReachedRe = re(`(?i)^(?: \(.*?\))?,? then the position of the bracket where that depth is first reached, counting from 1$`)
	tokenRe        = re(`\d+|\*\*|//|[+\-*/%()]`)
)

// Segments maps each label to its text; the first segment with a label wins.
func Segments(prompt string) map[string]string {
	found := map[string]string{}
	for _, part := range strings.Split(prompt, "|") {
		label, text, ok := strings.Cut(strings.TrimSpace(part), ":")
		if !ok {
			continue
		}
		label = strings.TrimSpace(label)
		if _, seen := found[label]; !seen && labelRe.MatchString(label) {
			found[label] = strings.TrimSpace(text)
		}
	}
	return found
}

func data(parts map[string]string) (string, bool) {
	for _, label := range dataLabels {
		if text, ok := parts[label]; ok {
			return text, true
		}
	}
	return "", false
}

func words(text string) []string {
	var found []string
	for _, w := range wordSplitRe.Split(text, -1) {
		if w != "" {
			found = append(found, w)
		}
	}
	return found
}

func shift(text string, amount int64) string {
	rotate := func(c, base rune) rune {
		n := (int64(c-base) + amount) % 26
		if n < 0 {
			n += 26
		}
		return rune(n) + base
	}
	return strings.Map(func(c rune) rune {
		switch {
		case c >= 'A' && c <= 'Z':
			return rotate(c, 'A')
		case c >= 'a' && c <= 'z':
			return rotate(c, 'a')
		}
		return c
	}, text)
}

func reverse(text string) string {
	chars := []rune(text)
	slices.Reverse(chars)
	return string(chars)
}

func trimDots(s string) string {
	return strings.TrimRight(s, ".")
}

// --- arithmetic: integer-only, Python semantics (floor `%` and `//`),
// bounded to 128-bit signed integers. ---

type parser struct {
	tokens []string
	at     int
}

func fits(v *big.Int) (*big.Int, bool) {
	return v, v.BitLen() <= 127
}

func (p *parser) peek() string {
	if p.at < len(p.tokens) {
		return p.tokens[p.at]
	}
	return ""
}

func (p *parser) take(token string) bool {
	if p.peek() == token {
		p.at++
		return true
	}
	return false
}

func (p *parser) sum() (*big.Int, bool) {
	value, ok := p.product()
	for ok {
		var right *big.Int
		switch {
		case p.take("+"):
			if right, ok = p.product(); ok {
				value, ok = fits(new(big.Int).Add(value, right))
			}
		case p.take("-"):
			if right, ok = p.product(); ok {
				value, ok = fits(new(big.Int).Sub(value, right))
			}
		default:
			return value, true
		}
	}
	return nil, false
}

func (p *parser) product() (*big.Int, bool) {
	value, ok := p.unary()
	for ok {
		var right *big.Int
		switch {
		case p.take("*"):
			if right, ok = p.unary(); ok {
				value, ok = fits(new(big.Int).Mul(value, right))
			}
		case p.take("//"):
			if right, ok = p.unary(); ok {
				value, ok = floorDiv(value, right)
			}
		case p.take("/"):
			if right, ok = p.unary(); ok {
				quotient, remainder := new(big.Int).QuoRem(value, right, new(big.Int))
				if right.Sign() == 0 || remainder.Sign() != 0 {
					return nil, false // inexact division
				}
				value = quotient
			}
		case p.take("%"):
			if right, ok = p.unary(); ok {
				var quotient *big.Int
				if quotient, ok = floorDiv(value, right); ok {
					value, ok = fits(new(big.Int).Sub(value, new(big.Int).Mul(quotient, right)))
				}
			}
		default:
			return value, true
		}
	}
	return nil, false
}

func (p *parser) unary() (*big.Int, bool) {
	if p.take("-") {
		value, ok := p.unary()
		if !ok {
			return nil, false
		}
		return fits(new(big.Int).Neg(value))
	}
	if p.take("+") {
		return p.unary()
	}
	return p.power()
}

func (p *parser) power() (*big.Int, bool) {
	base, ok := p.atom()
	if !ok {
		return nil, false
	}
	if p.take("**") {
		exponent, ok := p.unary() // right-associative, binds unary minus
		if !ok || exponent.Sign() < 0 || exponent.Cmp(big.NewInt(64)) > 0 {
			return nil, false
		}
		return fits(new(big.Int).Exp(base, exponent, nil))
	}
	return base, true
}

func (p *parser) atom() (*big.Int, bool) {
	if p.take("(") {
		value, ok := p.sum()
		if !ok || !p.take(")") {
			return nil, false
		}
		return value, true
	}
	token := p.peek()
	if token == "" || token[0] < '0' || token[0] > '9' {
		return nil, false
	}
	value, ok := new(big.Int).SetString(token, 10)
	if !ok {
		return nil, false
	}
	p.at++
	return fits(value)
}

func floorDiv(left, right *big.Int) (*big.Int, bool) {
	if right.Sign() == 0 {
		return nil, false
	}
	quotient, remainder := new(big.Int).QuoRem(left, right, new(big.Int))
	if remainder.Sign() != 0 && (left.Sign() < 0) != (right.Sign() < 0) {
		quotient.Sub(quotient, big.NewInt(1))
	}
	return fits(quotient)
}

// Arithmetic evaluates an integer expression ("(309 * 12 + 64) mod 97").
func Arithmetic(expression string) (*big.Int, bool) {
	text := trimDots(strings.TrimSpace(expression))
	text = modWordRe.ReplaceAllString(text, "%")
	text = strings.NewReplacer("×", "*", "x", "*", "÷", "/", "^", "**").Replace(text)
	if !arithCharsRe.MatchString(text) {
		return nil, false
	}
	p := &parser{tokens: tokenRe.FindAllString(text, -1)}
	value, ok := p.sum()
	if !ok || p.at != len(p.tokens) {
		return nil, false
	}
	return value, true
}

// --- "take word N, then ..." pipelines ---

type step struct {
	pattern *regexp.Regexp
	apply   func(word string, m []string) string
}

func newStep(pattern string, apply func(string, []string) string) step {
	return step{re(`(?i)^(?:` + pattern + `)$`), apply}
}

var steps = []step{
	newStep(`(?:write|spell|read) it backwards?|reverse it|reverse the (?:word|letters)`,
		func(w string, _ []string) string { return reverse(w) }),
	newStep(`(?:drop|remove|delete) (?:every|all|the) vowels?(?: \(AEIOU\))?`,
		func(w string, _ []string) string {
			return strings.Map(func(c rune) rune {
				if isVowel(c) {
					return -1
				}
				return c
			}, w)
		}),
	newStep(`(?:drop|remove|delete) (?:every|all|the) consonants?`,
		func(w string, _ []string) string {
			return strings.Map(func(c rune) rune {
				if isVowel(c) || !unicode.IsLetter(c) {
					return c
				}
				return -1
			}, w)
		}),
	newStep(`(?:make it |convert it to |write it in )?upper ?case|capitali[sz]e it`,
		func(w string, _ []string) string { return strings.ToUpper(w) }),
	newStep(`(?:make it |convert it to |write it in )?lower ?case`,
		func(w string, _ []string) string { return strings.ToLower(w) }),
	newStep(`(?:apply )?rot ?(\d+)`,
		func(w string, m []string) string {
			amount, _ := strconv.ParseInt(m[1], 10, 64)
			return shift(w, amount)
		}),
	newStep(`shift every letter (forward|back(?:ward)?) by (\d+)`,
		func(w string, m []string) string {
			amount, _ := strconv.ParseInt(m[2], 10, 64)
			if !strings.EqualFold(m[1], "forward") {
				amount = -amount
			}
			return shift(w, amount)
		}),
	newStep(`sort (?:its |the )?letters(?: alphabetically)?`,
		func(w string, _ []string) string {
			chars := []rune(w)
			slices.Sort(chars)
			return string(chars)
		}),
	newStep(`double every letter`,
		func(w string, _ []string) string {
			var b strings.Builder
			for _, c := range w {
				b.WriteRune(c)
				b.WriteRune(c)
			}
			return b.String()
		}),
}

func pipeline(word, rest string) (string, bool) {
	for _, clause := range clauseSplitRe.Split(rest, -1) {
		clause = trimDots(strings.TrimSpace(clause))
		if clause == "" {
			continue
		}
		clause = clausePrefixRe.ReplaceAllString(clause, "")
		applied := false
		for _, s := range steps {
			if m := s.pattern.FindStringSubmatch(clause); m != nil {
				word = s.apply(word, m)
				applied = true
				break
			}
		}
		if !applied {
			return "", false
		}
	}
	return word, true
}

// nesting is `depth` or `depth,position` (1-based) of the first bracket at
// that depth.
func nesting(brackets string, withPosition bool) (string, bool) {
	depth, best, at, index := 0, 0, 0, 0
	for _, c := range brackets {
		if unicode.IsSpace(c) {
			continue
		}
		switch c {
		case '(', '[', '{':
			depth++
			if depth > best {
				best, at = depth, index+1
			}
		case ')', ']', '}':
			if depth == 0 {
				return "", false
			}
			depth--
		default:
			return "", false
		}
		index++
	}
	if depth != 0 || best == 0 {
		return "", false
	}
	if withPosition {
		return strconv.Itoa(best) + "," + strconv.Itoa(at), true
	}
	return strconv.Itoa(best), true
}

// grid is the grid after the transform, rows concatenated. Only square grids.
func grid(text, transform string) (string, bool) {
	var rows [][]rune
	for _, r := range strings.Split(text, "/") {
		rows = append(rows, []rune(strings.TrimSpace(r)))
	}
	n := len(rows)
	for _, r := range rows {
		if len(r) != n {
			return "", false
		}
	}
	if n == 0 {
		return "", false
	}
	transform = strings.ToLower(transform)
	cell := func(r, c int) rune {
		switch {
		case strings.Contains(transform, "180"):
			return rows[n-1-r][n-1-c]
		case strings.Contains(transform, "counter") || strings.Contains(transform, "anti"):
			return rows[c][n-1-r]
		case strings.Contains(transform, "clockwise"):
			return rows[n-1-c][r]
		case strings.Contains(transform, "transpose"):
			return rows[c][r]
		case strings.Contains(transform, "horizontal") || strings.Contains(transform, "left to right"):
			return rows[r][n-1-c]
		}
		return rows[n-1-r][c]
	}
	out := make([]rune, 0, n*n)
	for r := range n {
		for c := range n {
			out = append(out, cell(r, c))
		}
	}
	return string(out), true
}

// tokens does the token bookkeeping: "you hold 7 red tokens ... You do NOT
// take 1 blue token. You take 3 red tokens." Negated sentences change nothing.
func tokens(start, colour string) (string, bool) {
	var sentences []string
	for _, s := range strings.FieldsFunc(start, func(c rune) bool { return c == '.' || c == ';' }) {
		if s = strings.TrimSpace(s); s != "" {
			sentences = append(sentences, s)
		}
	}
	if len(sentences) == 0 {
		return "", false
	}
	held := map[string]int64{}
	for _, m := range tokenCountRe.FindAllStringSubmatch(sentences[0], -1) {
		n, err := strconv.ParseInt(m[1], 10, 64)
		if err != nil {
			return "", false
		}
		held[strings.ToLower(m[2])] = n
	}
	if len(held) == 0 {
		return "", false
	}
	for _, sentence := range sentences[1:] {
		counts := tokenCountRe.FindAllStringSubmatch(sentence, -1)
		// "You give away no blue tokens" moves nothing.
		if tokenNoneRe.MatchString(sentence) && len(counts) == 0 {
			continue
		}
		if len(counts) != 1 {
			return "", false
		}
		if tokenNegRe.MatchString(sentence) {
			continue
		}
		amount, err := strconv.ParseInt(counts[0][1], 10, 64)
		if err != nil {
			return "", false
		}
		switch {
		case tokenLossRe.MatchString(sentence):
			amount = -amount
		case tokenGainRe.MatchString(sentence):
		default:
			return "", false
		}
		held[strings.ToLower(counts[0][2])] += amount
	}
	if colour != "" {
		n, ok := held[strings.ToLower(colour)]
		return strconv.FormatInt(n, 10), ok
	}
	var total int64
	for _, n := range held {
		total += n
	}
	return strconv.FormatInt(total, 10), true
}

func substrings(words []string, minLen, maxLen int) []string {
	var found []string
	seen := map[string]bool{}
	for _, word := range words {
		chars := []rune(word)
		for length := minLen; length <= maxLen; length++ {
			for start := 0; start+length <= len(chars); start++ {
				piece := string(chars[start : start+length])
				if !seen[piece] {
					seen[piece] = true
					found = append(found, piece)
				}
			}
		}
	}
	return found
}

// tally counts candidate answers in the order they were first found.
type tally struct {
	order []string
	count map[string]int
}

func newTally() *tally { return &tally{count: map[string]int{}} }

func (t *tally) add(answer string) {
	if t.count[answer] == 0 {
		t.order = append(t.order, answer)
	}
	t.count[answer]++
}

func (t *tally) best() (string, bool) {
	if len(t.order) == 0 {
		return "", false
	}
	best := t.order[0]
	for _, answer := range t.order[1:] {
		if t.count[answer] > t.count[best] {
			best = answer
		}
	}
	return best, true
}

type pair struct{ from, to string }

// letterThenDouble covers every hidden-rules prompt seen in races: two
// ordered rules, one letter becomes two (`b -> xd`), then a doubled letter
// becomes one (`dd -> h`), so the first rule feeds the second. This shape is
// tried before smaller rule sets: `dbd -> dxh ; bdc -> xhc` also fit
// `bd -> xh` alone, whose `aabbd -> aabxh` the server rejected; the two rules
// give `aaxdxh`.
func letterThenDouble(pairs []pair, query string) *tally {
	var letters []rune
	for _, p := range pairs {
		letters = append(letters, []rune(p.from+p.to)...)
	}
	slices.Sort(letters)
	letters = slices.Compact(letters)
	answers := newTally()
	for _, from := range letters {
		fromText := string(from)
		used := false
		for _, p := range pairs {
			if strings.ContainsRune(p.from, from) {
				used = true
				break
			}
		}
		if !used {
			continue
		}
		for _, p := range letters {
			for _, q := range letters {
				to := string([]rune{p, q})
				mids := make([]string, len(pairs))
				for i, pr := range pairs {
					mids[i] = strings.ReplaceAll(pr.from, fromText, to)
				}
				for _, s := range letters {
					double := string([]rune{s, s})
					// `to == double` only renames `from`: that is a one-rule set.
					if to == double || !slices.ContainsFunc(mids, func(mid string) bool {
						return strings.Contains(mid, double)
					}) {
						continue
					}
					for _, r := range letters {
						single := string(r)
						fit := true
						for i, pr := range pairs {
							if strings.ReplaceAll(mids[i], double, single) != pr.to {
								fit = false
								break
							}
						}
						if fit {
							answers.add(strings.ReplaceAll(strings.ReplaceAll(query, fromText, to), double, single))
						}
					}
				}
			}
		}
	}
	return answers
}

// hiddenRules finds up to two ordered `replace(lhs, rhs)` rules that turn
// every example input into its output, and applies them to `query`. The
// generator's shape (letterThenDouble) comes first; otherwise, among the
// smallest rule sets that fit, the most common answer wins.
func hiddenRules(examples, query string) (string, bool) {
	var pairs []pair
	for _, text := range strings.Split(examples, ";") {
		from, to, ok := strings.Cut(text, "->")
		if !ok {
			continue
		}
		p := pair{strings.TrimSpace(from), strings.TrimSpace(to)}
		if p.from == "" || strings.IndexFunc(p.to, unicode.IsSpace) >= 0 {
			return "", false
		}
		pairs = append(pairs, p)
	}
	if len(pairs) == 0 {
		return "", false
	}
	inputs := make([]string, len(pairs))
	outputs := make([]string, len(pairs))
	for i, p := range pairs {
		inputs[i], outputs[i] = p.from, p.to
	}
	rhs := substrings(outputs, 0, 3)
	answers := letterThenDouble(pairs, query)

	if len(answers.order) == 0 {
		// One rule.
		for _, lhs := range substrings(inputs, 1, 3) {
			for _, r := range rhs {
				if r == lhs {
					continue
				}
				fit := true
				for _, p := range pairs {
					if strings.ReplaceAll(p.from, lhs, r) != p.to {
						fit = false
						break
					}
				}
				if fit {
					answers.add(strings.ReplaceAll(query, lhs, r))
				}
			}
		}
	}
	if len(answers.order) == 0 {
		// Two rules, applied in order.
		mids := make([]string, len(pairs))
		for _, lhs1 := range substrings(inputs, 1, 2) {
			for _, r1 := range rhs {
				if r1 == lhs1 {
					continue
				}
				for i, p := range pairs {
					mids[i] = strings.ReplaceAll(p.from, lhs1, r1)
				}
				for _, lhs2 := range substrings(mids, 1, 3) {
					for _, r2 := range rhs {
						if r2 == lhs2 {
							continue
						}
						fit := true
						for i, p := range pairs {
							n := strings.Count(mids[i], lhs2)
							if len(mids[i])+n*len(r2)-n*len(lhs2) != len(p.to) ||
								strings.ReplaceAll(mids[i], lhs2, r2) != p.to {
								fit = false
								break
							}
						}
						if fit {
							answers.add(strings.ReplaceAll(strings.ReplaceAll(query, lhs1, r1), lhs2, r2))
						}
					}
				}
			}
		}
	}
	return answers.best()
}

func finalPosition(prompt string, parts map[string]string) (string, bool) {
	start := startAtRe.FindStringSubmatch(prompt)
	movesText, ok := parts["MOVES"]
	if start == nil || !ok {
		return "", false
	}
	moves := movesRe.FindStringSubmatch(movesText)
	if moves == nil {
		return "", false
	}
	x, errX := strconv.ParseInt(start[1], 10, 64)
	y, errY := strconv.ParseInt(start[2], 10, 64)
	if errX != nil || errY != nil {
		return "", false
	}
	type rule struct {
		isX   bool
		delta int64
	}
	rules := map[byte]rule{}
	for _, m := range moveRuleRe.FindAllStringSubmatch(moves[2], -1) {
		amount, err := strconv.ParseInt(m[3], 10, 64)
		if err != nil {
			return "", false
		}
		if m[2] != "adds" {
			amount = -amount
		}
		rules[m[1][0]] = rule{m[4] == "x", amount}
	}
	for i := 0; i < len(moves[1]); i++ {
		r, ok := rules[moves[1][i]]
		if !ok {
			return "", false
		}
		if r.isX {
			x += r.delta
		} else {
			y += r.delta
		}
	}
	return strconv.FormatInt(x, 10) + "," + strconv.FormatInt(y, 10), true
}

// Solve answers a prompt exactly, or returns false.
func Solve(prompt string) (string, bool) {
	parts := Segments(prompt)
	taskText, ok := parts["TASK"]
	if !ok {
		return "", false
	}
	task := trimDots(strings.TrimSpace(taskText))
	if task == "" {
		return "", false
	}
	text, hasData := data(parts)

	if finalPosRe.MatchString(task) {
		return finalPosition(prompt, parts)
	}

	if m := oddOneRe.FindStringSubmatch(task); m != nil && hasData {
		kind := strings.ReplaceAll(strings.ToLower(m[1]), " ", "")
		wanted := func(c rune) bool {
			switch kind {
			case "digit", "number":
				return c >= '0' && c <= '9'
			case "lowercaseletter":
				return unicode.IsLower(c)
			case "uppercaseletter":
				return unicode.IsUpper(c)
			case "vowel":
				return isVowel(c)
			case "consonant":
				return unicode.IsLetter(c) && !isVowel(c)
			case "letter":
				return unicode.IsLetter(c)
			case "space":
				return c == ' '
			}
			return !unicode.IsLetter(c) && !unicode.IsNumber(c) && !unicode.IsSpace(c)
		}
		var found []int
		for i, c := range []rune(text) {
			if wanted(c) {
				found = append(found, i)
			}
		}
		base, _ := strconv.Atoi(m[2])
		if len(found) != 1 {
			return "", false
		}
		return strconv.Itoa(found[0] + base), true
	}

	if m := tokensTaskRe.FindStringSubmatch(task); m != nil {
		if start, ok := parts["START"]; ok {
			colour := m[1]
			if strings.EqualFold(colour, "total") {
				colour = ""
			}
			return tokens(start, colour)
		}
	}

	if m := rulesTaskRe.FindStringSubmatch(task); m != nil {
		if examples, ok := parts["EXAMPLES"]; ok {
			return hiddenRules(examples, m[1])
		}
	}

	if m := gridTaskRe.FindStringSubmatch(task); m != nil {
		// The label carries a note ("GRID (three rows)"), so it is not in `parts`.
		rows := gridRowsRe.FindStringSubmatch(prompt)
		if rows == nil {
			return "", false
		}
		return grid(rows[1], m[1])
	}

	if m := nestingRe.FindStringSubmatch(task); m != nil {
		if brackets, ok := parts["BRACKETS"]; ok {
			rest := strings.TrimSpace(m[1])
			if rest == "" || strings.HasPrefix(rest, "(") && !strings.Contains(rest, "then") {
				return nesting(brackets, false)
			}
			if !firstReachedRe.MatchString(m[1]) {
				return "", false
			}
			return nesting(brackets, true)
		}
	}

	if m := computeRe.FindStringSubmatch(task); m != nil {
		value, ok := Arithmetic(m[1])
		if !ok {
			return "", false
		}
		return value.String(), true
	}

	if m := countLetterRe.FindStringSubmatch(task); m != nil && hasData {
		return strconv.Itoa(strings.Count(strings.ToUpper(text), strings.ToUpper(m[1]))), true
	}

	if m := countKindRe.FindStringSubmatch(task); m != nil && hasData {
		count := 0
		kind := strings.ToLower(m[1])
		if kind == "words" {
			count = len(words(text))
		} else {
			for _, c := range text {
				if !unicode.IsLetter(c) {
					continue
				}
				if kind == "letters" || (kind == "vowels") == isVowel(c) {
					count++
				}
			}
		}
		return strconv.Itoa(count), true
	}

	if m := rotRe.FindStringSubmatch(task); m != nil && hasData {
		var amount int64
		var err error
		if m[1] != "" {
			amount, err = strconv.ParseInt(m[1], 10, 64)
		} else {
			amount, err = strconv.ParseInt(m[3], 10, 64)
			if !strings.EqualFold(m[2], "forward") {
				amount = -amount
			}
		}
		if err != nil {
			return "", false
		}
		return shift(text, amount), true
	}

	if m := positionRe.FindStringSubmatch(task); m != nil {
		if !hasData {
			return "", false
		}
		list := words(text)
		number, err := strconv.Atoi(m[1])
		if err != nil || number == 0 || number > len(list) {
			return "", false
		}
		word := list[number-1]
		if strings.Contains(strings.ToLower(m[2]), "end") {
			word = list[len(list)-number]
		}
		rest := strings.TrimLeft(strings.TrimSpace(m[3]), ",")
		if rest == "" {
			return word, true
		}
		return pipeline(word, rest)
	}

	if m := extremeRe.FindStringSubmatch(task); m != nil {
		if !hasData {
			return "", false
		}
		list := words(text)
		if len(list) == 0 {
			return "", false
		}
		lengths := make([]int, len(list))
		for i, w := range list {
			lengths[i] = len([]rune(w))
		}
		target := slices.Min(lengths)
		if strings.EqualFold(m[3], "longest") {
			target = slices.Max(lengths)
		}
		var found []int
		for i, n := range lengths {
			if n == target {
				found = append(found, i)
			}
		}
		if len(found) != 1 {
			return "", false // a tie makes "the longest word" ambiguous
		}
		index := found[0]
		switch strings.ToLower(m[2]) {
		case "before":
			index--
		case "after":
			index++
		}
		if index < 0 || index >= len(list) {
			return "", false
		}
		return list[index], true
	}

	if reverseRe.MatchString(task) {
		if !hasData {
			return "", false
		}
		return reverse(text), true
	}
	return "", false
}

// WarmupCases exercise every solver family.
var WarmupCases = [][2]string{
	{"TASK: compute (309 * 12 + 64) mod 97", "86"},
	{"TASK: compute 2 ^ 10 - 7 x 3", "1003"},
	{"TASK: compute -7 // 2 + 8 / 2", "0"},
	{"TEXT: ABACA | TASK: how many times does the letter A appear", "3"},
	{"TEXT: ABACA | TASK: how many vowels are there", "3"},
	{"TEXT: ABACA | TASK: how many consonants are there", "2"},
	{"TEXT: AB ACA | TASK: how many letters are there", "5"},
	{"TEXT: AB, ACA; DEF | TASK: how many words are there", "3"},
	{"TEXT: AbZ | TASK: apply rot13", "NoM"},
	{"TEXT: AbZ | TASK: shift every letter backward by 1", "ZaY"},
	{"TEXT: ABC | TASK: reverse the text", "CBA"},
	{"LIST: AB CDEF G HIJ | TASK: the longest word", "CDEF"},
	{"LIST: AB CDEF G HIJ | TASK: the word before the shortest word", "CDEF"},
	{"LIST: AB CDEF G HIJ | TASK: the word after the shortest word", "HIJ"},
	{"WORDS: AB CDE F | TASK: take word number 2, counting from the end", "CDE"},
	{"WORDS: AbC | TASK: take word number 1, counting from 1, reverse it, drop every vowel, " +
		"uppercase, lowercase, apply rot1, shift every letter backward by 1, sort its letters, double every letter", "bbcc"},
	{"WORDS: AbC | TASK: take word number 1, counting from 1, drop every consonant", "A"},
	{"START at 0,0 | MOVES: URDL (U adds 1 to y, R adds 1 to x, D subtracts 1 from y, " +
		"L subtracts 1 from x) | TASK: the final position", "0,0"},
	{"TEXT: AB1CD | TASK: exactly one character is a digit, give its position, counting from 1", "3"},
	{"START: you hold 5 red tokens and 7 blue tokens. You take 3 blue tokens. You give away 1 blue token. " +
		"You do NOT take 2 red tokens. You give away no blue tokens. | TASK: how many blue tokens do you hold", "9"},
	{"EXAMPLES: adac -> ycdyg ; aac -> ycyg ; aaac -> ycycyg ; bbb -> bbb | " +
		"TASK: the same hidden rules transform ddac into what", "ddyg"},
	{"GRID (two rows): AB / CD | TASK: rotate the grid 90 degrees clockwise, then read the rows left to right", "CADB"},
	{"GRID (two rows): AB / CD | TASK: rotate the grid 90 degrees counterclockwise, then read the rows left to right", "BDAC"},
	{"GRID (two rows): AB / CD | TASK: rotate the grid 180 degrees, then read the rows left to right", "DCBA"},
	{"GRID (two rows): AB / CD | TASK: transpose the grid, then read the rows left to right", "ACBD"},
	{"GRID (two rows): AB / CD | TASK: flip the grid horizontally, then read the rows left to right", "BADC"},
	{"GRID (two rows): AB / CD | TASK: flip the grid vertically, then read the rows left to right", "CDAB"},
	{"BRACKETS: (()) | TASK: the maximum nesting depth", "2"},
	{"BRACKETS: (()) | TASK: the maximum nesting depth, then the position of the bracket where " +
		"that depth is first reached, counting from 1", "2,2"},
}

var sink string

// Warm runs every solver family once, moving first-use initialization off
// the race clock.
func Warm() {
	for _, c := range WarmupCases {
		answer, _ := Solve(c[0])
		sink = answer
	}
}
