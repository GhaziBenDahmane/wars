#include "solvers.hpp"

#include <algorithm>
#include <initializer_list>
#include <unordered_map>
#include <unordered_set>

#include "text.hpp"

namespace solvers {

using namespace text;
using std::nullopt;
using std::optional;
using std::string;

namespace {

constexpr sv DATA_LABELS[] = {"TEXT", "LIST", "WORDS", "WORD", "INPUT", "STRING", "NUMBERS"};

/// An anchored, case-insensitive cursor: the hand-written stand-in for the
/// `(?i)^...$` patterns. Every alternation used here is decided by its first
/// differing character, so no backtracking into the continuation is needed;
/// the few patterns where it is (optional words) try both branches explicitly.
struct Cursor {
    sv s;
    size_t at = 0;

    bool lit(sv l) {
        if (!istarts(s.substr(at), l)) return false;
        at += l.size();
        return true;
    }
    bool opt(sv l) {
        lit(l);
        return true;
    }
    bool one(std::initializer_list<sv> alts, sv* got = nullptr) {
        for (sv alt : alts)
            if (istarts(s.substr(at), alt)) {
                if (got) *got = s.substr(at, alt.size());
                at += alt.size();
                return true;
            }
        return false;
    }
    bool digits(sv& out) {
        out = digits_at(s, at);
        at += out.size();
        return !out.empty();
    }
    bool word(sv& out) {
        out = word_at(s, at);
        at += out.size();
        return !out.empty();
    }
    bool end() const { return at == s.size(); }
    sv rest() const { return s.substr(at); }
    /// `.*` up to the end: anything but a newline.
    bool rest_is_line() const { return rest().find('\n') == sv::npos; }
};

optional<sv> get(const Segments& parts, sv label) {
    for (auto& [l, t] : parts)
        if (l == label) return t;
    return nullopt;
}

optional<sv> data_segment(const Segments& parts) {
    for (sv label : DATA_LABELS)
        if (auto found = get(parts, label)) return found;
    return nullopt;
}

bool is_label(sv label) {
    if (label.empty() || !ascii_upper(label[0])) return false;
    for (char c : label)
        if (!ascii_upper(c) && c != '_' && c != ' ') return false;
    return true;
}

/// Split on runs of `[\s,;]+`, dropping empty pieces.
std::vector<sv> words(sv s) {
    std::vector<sv> out;
    size_t start = 0, i = 0;
    while (i < s.size()) {
        unsigned char c = s[i];
        size_t len = 1;
        bool separator = c == ',' || c == ';' || ascii_space(c);
        if (c >= 0x80) {
            sv ch = chars(s.substr(i, 4)).front();
            len = ch.size();
            separator = is_space(ch);
        }
        if (separator) {
            if (i > start) out.push_back(s.substr(start, i - start));
            start = i + len;
        }
        i += len;
    }
    if (start < s.size()) out.push_back(s.substr(start));
    return out;
}

string shift(sv s, int64_t amount) {
    int r = int(((amount % 26) + 26) % 26);
    string out(s);
    for (char& c : out) {
        if (ascii_upper(c)) c = char('A' + (c - 'A' + r) % 26);
        else if (ascii_lower(c)) c = char('a' + (c - 'a' + r) % 26);
    }
    return out;
}

string join(const std::vector<sv>& pieces) {
    string out;
    for (sv p : pieces) out += p;
    return out;
}

/// Any of `alts` (case-insensitive) bounded by `\b` on both sides. Every
/// alternative starts and ends with a word character.
bool has_word(sv s, std::initializer_list<sv> alts) {
    for (size_t i = 0; i < s.size(); ++i) {
        if (!word_byte(s[i]) || (i > 0 && word_byte(s[i - 1]))) continue;
        for (sv alt : alts)
            if (istarts(s.substr(i), alt) &&
                (i + alt.size() == s.size() || !word_byte(s[i + alt.size()])))
                return true;
    }
    return false;
}

// --- arithmetic: integer-only, Python semantics (floor `%` and `//`). ---

using i128 = __int128;

optional<i128> floor_div(i128 left, i128 right) {
    if (right == 0) return nullopt;
    i128 min = i128(1) << 127;
    if (left == min && right == -1) return nullopt;
    i128 q = left / right;
    if (left % right != 0 && ((left < 0) != (right < 0))) q -= 1;
    return q;
}

struct Parser {
    std::vector<sv> tokens;
    size_t at = 0;

    optional<sv> peek() const {
        if (at < tokens.size()) return tokens[at];
        return nullopt;
    }
    bool take(sv token) {
        if (at < tokens.size() && tokens[at] == token) {
            ++at;
            return true;
        }
        return false;
    }

    optional<i128> sum() {
        auto value = product();
        if (!value) return nullopt;
        i128 v = *value;
        while (true) {
            if (take("+")) {
                auto r = product();
                if (!r || __builtin_add_overflow(v, *r, &v)) return nullopt;
            } else if (take("-")) {
                auto r = product();
                if (!r || __builtin_sub_overflow(v, *r, &v)) return nullopt;
            } else {
                return v;
            }
        }
    }

    optional<i128> product() {
        auto value = unary();
        if (!value) return nullopt;
        i128 v = *value;
        while (true) {
            if (take("*")) {
                auto r = unary();
                if (!r || __builtin_mul_overflow(v, *r, &v)) return nullopt;
            } else if (take("//")) {
                auto r = unary();
                if (!r) return nullopt;
                auto q = floor_div(v, *r);
                if (!q) return nullopt;
                v = *q;
            } else if (take("/")) {
                auto r = unary();
                if (!r || *r == 0) return nullopt;
                if (*r == -1) {  // `%` would overflow on i128::MIN; the quotient is -v
                    if (__builtin_sub_overflow(i128(0), v, &v)) return nullopt;
                    continue;
                }
                if (v % *r != 0) return nullopt;  // inexact division
                v /= *r;
            } else if (take("%")) {
                auto r = unary();
                if (!r) return nullopt;
                auto q = floor_div(v, *r);
                i128 m;
                if (!q || __builtin_mul_overflow(*q, *r, &m) || __builtin_sub_overflow(v, m, &v))
                    return nullopt;
            } else {
                return v;
            }
        }
    }

    optional<i128> unary() {
        if (take("-")) {
            auto v = unary();
            i128 out;
            if (!v || __builtin_sub_overflow(i128(0), *v, &out)) return nullopt;
            return out;
        }
        if (take("+")) return unary();
        return power();
    }

    optional<i128> power() {
        auto base = atom();
        if (!base) return nullopt;
        if (take("**")) {
            auto exponent = unary();  // right-associative, binds unary minus
            if (!exponent || *exponent < 0 || *exponent > 64) return nullopt;
            i128 acc = 1;
            for (int i = 0; i < int(*exponent); ++i)
                if (__builtin_mul_overflow(acc, *base, &acc)) return nullopt;
            return acc;
        }
        return base;
    }

    optional<i128> atom() {
        if (take("(")) {
            auto v = sum();
            if (!v || !take(")")) return nullopt;
            return v;
        }
        auto token = peek();
        if (!token || !ascii_digit((*token)[0])) return nullopt;
        i128 v = 0;
        for (char c : *token)
            if (__builtin_mul_overflow(v, 10, &v) || __builtin_add_overflow(v, c - '0', &v))
                return nullopt;
        ++at;
        return v;
    }
};

// --- "take word N, then ..." pipelines ---

string reversed(sv w) {
    auto cs = chars(w);
    std::reverse(cs.begin(), cs.end());
    return join(cs);
}

optional<string> step(sv w, sv clause) {
    auto full = [&](std::initializer_list<sv> alts) {
        for (sv alt : alts)
            if (iequals(clause, alt)) return true;
        return false;
    };
    if (full({"write it backward", "write it backwards", "spell it backward", "spell it backwards",
              "read it backward", "read it backwards", "reverse it", "reverse the word",
              "reverse the letters"}))
        return reversed(w);
    {
        Cursor m{clause};
        if (m.one({"drop", "remove", "delete"}) && m.lit(" ") && m.one({"every", "all", "the"}) &&
            m.lit(" vowel") && m.opt("s") && m.opt(" (aeiou)") && m.end()) {
            string out;
            for (sv c : chars(w))
                if (!is_vowel(c)) out += c;
            return out;
        }
    }
    {
        Cursor m{clause};
        if (m.one({"drop", "remove", "delete"}) && m.lit(" ") && m.one({"every", "all", "the"}) &&
            m.lit(" consonant") && m.opt("s") && m.end()) {
            string out;
            for (sv c : chars(w))
                if (is_vowel(c) || !is_alpha(c)) out += c;
            return out;
        }
    }
    for (sv kind : {sv("upper"), sv("lower")}) {
        Cursor m{clause};
        m.one({"make it ", "convert it to ", "write it in "});
        bool matched = m.lit(kind) && m.opt(" ") && m.lit("case") && m.end();
        if (kind == "upper") matched = matched || full({"capitalise it", "capitalize it"});
        if (matched) return kind == "upper" ? to_upper(w) : to_lower(w);
    }
    {
        Cursor m{clause};
        sv n;
        m.opt("apply ");
        if (m.lit("rot") && m.opt(" ") && m.digits(n) && m.end()) {
            int64_t amount = 0;
            if (!parse_i64(n, amount)) amount = 0;
            return shift(w, amount);
        }
    }
    {
        Cursor m{clause};
        sv dir, n;
        if (m.lit("shift every letter ") && m.one({"forward", "backward", "back"}, &dir) &&
            m.lit(" by ") && m.digits(n) && m.end()) {
            int64_t amount = 0;
            if (!parse_i64(n, amount)) amount = 0;
            return shift(w, iequals(dir, "forward") ? amount : -amount);
        }
    }
    {
        Cursor m{clause};
        if (m.lit("sort ") && (m.one({"its ", "the "}), true) && m.lit("letters") &&
            m.opt(" alphabetically") && m.end()) {
            auto cs = chars(w);
            std::sort(cs.begin(), cs.end());
            return join(cs);
        }
    }
    if (full({"double every letter"})) {
        string out;
        for (sv c : chars(w)) {
            out += c;
            out += c;
        }
        return out;
    }
    return nullopt;
}

optional<string> pipeline(sv word, sv rest) {
    string current(word);
    size_t start = 0;
    while (start <= rest.size()) {
        size_t comma = rest.find(',', start);
        sv clause = rest.substr(start, comma == sv::npos ? sv::npos : comma - start);
        start = comma == sv::npos ? rest.size() + 1 : comma + 1;
        clause = trim_end_char(trim(clause), '.');
        if (clause.empty()) continue;
        for (sv prefix : {sv("and then"), sv("then"), sv("and")}) {
            if (istarts(clause, prefix) && clause.size() > prefix.size() &&
                ascii_space(clause[prefix.size()])) {
                clause.remove_prefix(prefix.size());
                while (!clause.empty() && ascii_space(clause.front())) clause.remove_prefix(1);
                break;
            }
        }
        auto next = step(current, clause);
        if (!next) return nullopt;
        current = std::move(*next);
    }
    return current;
}

/// `depth` or `depth,position` (1-based) of the first bracket at that depth.
optional<string> nesting(sv brackets, bool with_position) {
    size_t depth = 0, best = 0, at = 0, index = 0;
    for (sv c : chars(brackets)) {
        if (is_space(c)) continue;
        if (c == "(" || c == "[" || c == "{") {
            ++depth;
            if (depth > best) best = depth, at = index + 1;
        } else if (c == ")" || c == "]" || c == "}") {
            if (depth == 0) return nullopt;
            --depth;
        } else {
            return nullopt;
        }
        ++index;
    }
    if (depth != 0 || best == 0) return nullopt;
    return with_position ? std::to_string(best) + "," + std::to_string(at) : std::to_string(best);
}

/// The grid after the transform, rows concatenated. Only square grids.
optional<string> grid(sv rows_text, sv transform_text) {
    std::vector<std::vector<sv>> rows;
    size_t start = 0;
    while (true) {
        size_t slash = rows_text.find('/', start);
        rows.push_back(chars(trim(rows_text.substr(start, slash == sv::npos ? sv::npos : slash - start))));
        if (slash == sv::npos) break;
        start = slash + 1;
    }
    size_t n = rows.size();
    for (auto& r : rows)
        if (r.size() != n) return nullopt;
    string t = to_lower(transform_text);
    auto has = [&](sv needle) { return t.find(needle) != string::npos; };
    string out;
    for (size_t r = 0; r < n; ++r)
        for (size_t c = 0; c < n; ++c) {
            if (has("180")) out += rows[n - 1 - r][n - 1 - c];
            else if (has("counter") || has("anti")) out += rows[c][n - 1 - r];
            else if (has("clockwise")) out += rows[n - 1 - c][r];
            else if (has("transpose")) out += rows[c][r];
            else if (has("horizontal") || has("left to right")) out += rows[r][n - 1 - c];
            else out += rows[n - 1 - r][c];
        }
    return out;
}

// --- token bookkeeping ---

struct TokenCount {
    sv amount, colour;
    size_t end;
};

/// `(?i)(\d+) (\w+) tokens?`, leftmost match at or after `from`.
optional<TokenCount> token_count(sv s, size_t from) {
    for (size_t i = from; i < s.size(); ++i) {
        if (!ascii_digit(s[i])) continue;
        sv amount = digits_at(s, i);
        size_t j = i + amount.size();
        if (j >= s.size() || s[j] != ' ') continue;
        sv colour = word_at(s, j + 1);
        size_t k = j + 1 + colour.size();
        if (colour.empty() || k >= s.size() || s[k] != ' ' || !istarts(s.substr(k + 1), "token"))
            continue;
        size_t end = k + 6;
        if (end < s.size() && lower(s[end]) == 's') ++end;
        return TokenCount{amount, colour, end};
    }
    return nullopt;
}

size_t token_counts(sv s) {
    size_t n = 0, at = 0;
    while (auto m = token_count(s, at)) {
        ++n;
        at = m->end;
    }
    return n;
}

/// `(?i)\b(?:no|zero) \w+ tokens?\b`
bool token_none(sv s) {
    for (size_t i = 0; i < s.size(); ++i) {
        if (i > 0 && word_byte(s[i - 1])) continue;
        size_t at;
        if (istarts(s.substr(i), "no ")) at = i + 3;
        else if (istarts(s.substr(i), "zero ")) at = i + 5;
        else continue;
        sv w = word_at(s, at);
        at += w.size();
        if (w.empty() || !istarts(s.substr(at), " token")) continue;
        at += 6;
        if (at < s.size() && lower(s[at]) == 's' && (at + 1 == s.size() || !word_byte(s[at + 1])))
            return true;
        if (at == s.size() || !word_byte(s[at])) return true;
    }
    return false;
}

/// "you hold 7 red tokens ... You do NOT take 1 blue token. You take 3 red
/// tokens." Negated sentences change nothing.
optional<string> tokens(sv start, optional<sv> colour) {
    std::vector<sv> sentences;
    size_t from = 0;
    while (from <= start.size()) {
        size_t end = start.find_first_of(".;", from);
        sv sentence = trim(start.substr(from, end == sv::npos ? sv::npos : end - from));
        if (!sentence.empty()) sentences.push_back(sentence);
        if (end == sv::npos) break;
        from = end + 1;
    }
    if (sentences.empty()) return nullopt;
    std::vector<std::pair<string, int64_t>> held;
    auto slot = [&](const string& key) -> int64_t* {
        for (auto& [k, v] : held)
            if (k == key) return &v;
        return nullptr;
    };
    size_t at = 0;
    while (auto m = token_count(sentences[0], at)) {
        int64_t amount;
        if (!parse_i64(m->amount, amount)) return nullopt;
        string key = to_lower(m->colour);
        if (auto* v = slot(key)) *v = amount;
        else held.emplace_back(key, amount);
        at = m->end;
    }
    if (held.empty()) return nullopt;
    for (size_t i = 1; i < sentences.size(); ++i) {
        sv sentence = sentences[i];
        // "You give away no blue tokens" moves nothing.
        auto m = token_count(sentence, 0);
        if (token_none(sentence) && !m) continue;
        if (!m || token_counts(sentence) != 1) return nullopt;
        if (has_word(sentence, {"do not", "don't", "did not", "didn't", "never", "not", "except",
                                "ignore", "skip", "cancelled", "canceled", "cancel", "imagine",
                                "pretend"}))
            continue;
        int64_t amount;
        if (!parse_i64(m->amount, amount)) return nullopt;
        bool loss = has_word(sentence, {"give away", "gives away", "gave away", "give", "gives",
                                        "gave", "lose", "loses", "lost", "drop", "drops", "dropped",
                                        "spend", "spends", "spent", "remove", "discard", "return"});
        bool gain = has_word(sentence, {"take", "takes", "took", "receive", "receives", "received",
                                        "get", "gets", "got", "gain", "gains", "find", "finds",
                                        "found", "pick up", "are given", "win", "wins", "won", "add"});
        int64_t delta;
        if (loss) delta = -amount;
        else if (gain) delta = amount;
        else return nullopt;
        string key = to_lower(m->colour);
        if (auto* v = slot(key)) *v = int64_t(uint64_t(*v) + uint64_t(delta));
        else held.emplace_back(key, delta);
    }
    if (colour) {
        if (auto* v = slot(to_lower(*colour))) return std::to_string(*v);
        return nullopt;
    }
    int64_t total = 0;
    for (auto& [_, v] : held) total = int64_t(uint64_t(total) + uint64_t(v));
    return std::to_string(total);
}

// --- hidden rewrite rules ---

/// Distinct substrings (by character) of every word, lengths `lo..=hi`, in
/// discovery order.
std::vector<string> substrings(const std::vector<sv>& ws, size_t lo, size_t hi) {
    std::vector<string> found;
    std::unordered_set<string> seen;
    for (sv w : ws) {
        auto cs = chars(w);
        for (size_t len = lo; len <= hi; ++len) {
            if (len > cs.size()) continue;
            for (size_t start = 0; start + len <= cs.size(); ++start) {
                string piece;
                for (size_t k = start; k < start + len; ++k) piece += cs[k];
                if (seen.insert(piece).second) found.push_back(std::move(piece));
            }
        }
    }
    return found;
}

/// `replace_all(s, from, to) == expected` without building the string.
bool replaced_equals(sv s, sv from, sv to, sv expected) {
    size_t at = 0, out = 0;
    while (true) {
        size_t found = s.find(from, at);
        size_t keep = (found == sv::npos ? s.size() : found) - at;
        if (expected.substr(out, keep) != s.substr(at, keep) || expected.size() - out < keep)
            return false;
        out += keep;
        if (found == sv::npos) return out == expected.size();
        if (expected.substr(out, to.size()) != to) return false;
        out += to.size();
        at = found + from.size();
    }
}

struct Tally {
    std::vector<std::pair<string, int>> entries;
    std::unordered_map<string, size_t> index;

    void add(string answer) {
        auto [it, inserted] = index.try_emplace(std::move(answer), entries.size());
        if (inserted) entries.emplace_back(it->first, 1);
        else entries[it->second].second += 1;
    }
    bool empty() const { return entries.empty(); }
    optional<string> best() const {
        const std::pair<string, int>* top = nullptr;
        for (auto& e : entries)
            if (!top || e.second > top->second) top = &e;
        if (!top) return nullopt;
        return top->first;
    }
};

using Pairs = std::vector<std::pair<string, string>>;

/// Every hidden-rules prompt seen in races uses two ordered rules: one letter
/// becomes two (`b -> xd`), then a doubled letter becomes one (`dd -> h`), so
/// the first rule feeds the second. This shape is tried before smaller rule
/// sets: `dbd -> dxh ; bdc -> xhc` also fit `bd -> xh` alone, whose `aabbd ->
/// aabxh` the server rejected; the two rules give `aaxdxh`.
void letter_then_double(const Pairs& pairs, sv query, Tally& answers) {
    std::vector<sv> letters;
    for (auto& [a, b] : pairs) {
        for (sv c : chars(a)) letters.push_back(c);
        for (sv c : chars(b)) letters.push_back(c);
    }
    std::sort(letters.begin(), letters.end());
    letters.erase(std::unique(letters.begin(), letters.end()), letters.end());
    std::vector<string> mids(pairs.size());
    for (sv from : letters) {
        bool used = false;
        for (auto& [a, _] : pairs) used = used || a.find(from) != string::npos;
        if (!used) continue;
        for (sv p : letters)
            for (sv q : letters) {
                string to = string(p) + string(q);
                for (size_t i = 0; i < pairs.size(); ++i) mids[i] = replace_all(pairs[i].first, from, to);
                for (sv s : letters) {
                    string twice = string(s) + string(s);
                    // `to == twice` only renames `from`: that is a one-rule set.
                    if (to == twice) continue;
                    bool any = false;
                    for (auto& mid : mids) any = any || mid.find(twice) != string::npos;
                    if (!any) continue;
                    for (sv r : letters) {
                        bool fits = true;
                        for (size_t i = 0; fits && i < pairs.size(); ++i)
                            fits = replaced_equals(mids[i], twice, r, pairs[i].second);
                        if (fits) answers.add(replace_all(replace_all(query, from, to), twice, r));
                    }
                }
            }
    }
}

/// Find up to two ordered `replace(lhs, rhs)` rules that turn every example
/// input into its output, and apply them to `query`. The generator's shape
/// (`letter_then_double`) comes first; otherwise, among the smallest rule sets
/// that fit, the most common answer wins.
optional<string> hidden_rules(sv examples, sv query) {
    Pairs pairs;
    size_t from = 0;
    while (from <= examples.size()) {
        size_t semi = examples.find(';', from);
        sv pair = examples.substr(from, semi == sv::npos ? sv::npos : semi - from);
        size_t arrow = pair.find("->");
        if (arrow != sv::npos)
            pairs.emplace_back(string(trim(pair.substr(0, arrow))), string(trim(pair.substr(arrow + 2))));
        if (semi == sv::npos) break;
        from = semi + 1;
    }
    if (pairs.empty()) return nullopt;
    for (auto& [a, b] : pairs) {
        if (a.empty()) return nullopt;
        for (sv c : chars(b))
            if (is_space(c)) return nullopt;
    }
    std::vector<sv> inputs, outputs;
    for (auto& [a, b] : pairs) inputs.push_back(a), outputs.push_back(b);
    auto rhs = substrings(outputs, 0, 3);
    Tally answers;
    letter_then_double(pairs, query, answers);

    if (answers.empty()) {
        // One rule.
        for (auto& lhs : substrings(inputs, 1, 3))
            for (auto& r : rhs) {
                if (r == lhs) continue;
                bool fits = true;
                for (size_t i = 0; fits && i < pairs.size(); ++i)
                    fits = replaced_equals(pairs[i].first, lhs, r, pairs[i].second);
                if (fits) answers.add(replace_all(query, lhs, r));
            }
    }
    if (answers.empty()) {
        // Two rules, applied in order.
        std::vector<string> mids(pairs.size());
        for (auto& lhs1 : substrings(inputs, 1, 2))
            for (auto& r1 : rhs) {
                if (r1 == lhs1) continue;
                for (size_t i = 0; i < pairs.size(); ++i) mids[i] = replace_all(pairs[i].first, lhs1, r1);
                std::vector<sv> mid_views(mids.begin(), mids.end());
                for (auto& lhs2 : substrings(mid_views, 1, 3))
                    for (auto& r2 : rhs) {
                        if (r2 == lhs2) continue;
                        bool fits = true;
                        for (size_t i = 0; fits && i < pairs.size(); ++i)
                            fits = replaced_equals(mids[i], lhs2, r2, pairs[i].second);
                        if (fits) answers.add(replace_all(replace_all(query, lhs1, r1), lhs2, r2));
                    }
            }
    }
    return answers.best();
}

// --- the final position ---

/// `\bSTART at (-?\d+)\s*,\s*(-?\d+)`, leftmost.
bool start_at(sv s, int64_t& x, int64_t& y) {
    auto number = [&](size_t& at, sv& out) {
        size_t begin = at;
        if (at < s.size() && s[at] == '-') ++at;
        sv d = digits_at(s, at);
        if (d.empty()) return false;
        at += d.size();
        out = s.substr(begin, at - begin);
        return true;
    };
    for (size_t i = s.find("START at "); i != sv::npos; i = s.find("START at ", i + 1)) {
        if (i > 0 && word_byte(s[i - 1])) continue;
        size_t at = i + 9;
        sv a, b;
        if (!number(at, a)) continue;
        while (at < s.size() && ascii_space(s[at])) ++at;
        if (at >= s.size() || s[at] != ',') continue;
        ++at;
        while (at < s.size() && ascii_space(s[at])) ++at;
        if (!number(at, b)) continue;
        if (!parse_i64(a, x) || !parse_i64(b, y)) return false;
        return true;
    }
    return false;
}

optional<string> final_position(sv prompt, const Segments& parts) {
    int64_t x, y;
    if (!start_at(prompt, x, y)) return nullopt;
    auto moves_text = get(parts, "MOVES");
    if (!moves_text) return nullopt;
    // `^([A-Z]+)\s*\((.*)\)$`
    sv moves = *moves_text;
    size_t at = 0;
    while (at < moves.size() && ascii_upper(moves[at])) ++at;
    sv steps = moves.substr(0, at);
    while (at < moves.size() && ascii_space(moves[at])) ++at;
    if (steps.empty() || at >= moves.size() || moves[at] != '(' || moves.back() != ')' ||
        moves.size() < at + 2)
        return nullopt;
    sv body = moves.substr(at + 1, moves.size() - at - 2);
    if (body.find('\n') != sv::npos) return nullopt;
    // `\b([A-Z]) (adds|subtracts) (\d+) (?:to|from) ([xy])\b`, every match.
    struct Rule {
        bool is_x;
        int64_t delta;
    };
    std::unordered_map<char, Rule> rules;
    for (size_t i = 0; i < body.size();) {
        auto try_rule = [&]() -> size_t {
            if (!ascii_upper(body[i]) || (i > 0 && word_byte(body[i - 1]))) return 0;
            size_t p = i + 1;
            if (p >= body.size() || body[p] != ' ') return 0;
            ++p;
            bool adds;
            if (body.substr(p, 5) == "adds ") adds = true, p += 5;
            else if (body.substr(p, 10) == "subtracts ") adds = false, p += 10;
            else return 0;
            sv d = digits_at(body, p);
            if (d.empty()) return 0;
            p += d.size();
            if (body.substr(p, 4) == " to ") p += 4;
            else if (body.substr(p, 6) == " from ") p += 6;
            else return 0;
            if (p >= body.size() || (body[p] != 'x' && body[p] != 'y')) return 0;
            if (p + 1 < body.size() && word_byte(body[p + 1])) return 0;
            int64_t amount;
            if (!parse_i64(d, amount)) return sv::npos;
            rules[body[i]] = Rule{body[p] == 'x', adds ? amount : -amount};
            return p + 1;
        };
        size_t end = try_rule();
        if (end == sv::npos) return nullopt;
        i = end ? end : i + 1;
    }
    for (char c : steps) {
        auto rule = rules.find(c);
        if (rule == rules.end()) return nullopt;
        int64_t& axis = rule->second.is_x ? x : y;
        axis = int64_t(uint64_t(axis) + uint64_t(rule->second.delta));
    }
    return std::to_string(x) + "," + std::to_string(y);
}

/// `(?:^|\|)\s*GRID[^:|]*:\s*([^|]+)` on the whole prompt.
optional<sv> grid_rows(sv prompt) {
    auto at_start = [&](size_t i) -> optional<sv> {
        while (i < prompt.size() && ascii_space(prompt[i])) ++i;
        if (prompt.substr(i, 4) != "GRID") return nullopt;
        size_t colon = prompt.find_first_of(":|", i + 4);
        if (colon == sv::npos || prompt[colon] != ':') return nullopt;
        size_t begin = colon + 1, k = begin;
        while (k < prompt.size() && ascii_space(prompt[k])) ++k;
        if (k == prompt.size() || prompt[k] == '|') {
            if (k == begin) return nullopt;
            return prompt.substr(k - 1, 1);  // `\s*` gives one back to `[^|]+`
        }
        size_t bar = prompt.find('|', k);
        return prompt.substr(k, bar == sv::npos ? sv::npos : bar - k);
    };
    if (auto found = at_start(0)) return found;
    for (size_t bar = prompt.find('|'); bar != sv::npos; bar = prompt.find('|', bar + 1))
        if (auto found = at_start(bar + 1)) return found;
    return nullopt;
}

}  // namespace

std::string to_string(__int128 value) {
    if (value == 0) return "0";
    bool negative = value < 0;
    unsigned __int128 u = negative ? (unsigned __int128)0 - (unsigned __int128)value : value;
    string out;
    while (u) out += char('0' + int(u % 10)), u /= 10;
    if (negative) out += '-';
    std::reverse(out.begin(), out.end());
    return out;
}

Segments segments(sv prompt) {
    Segments found;
    size_t start = 0;
    while (start <= prompt.size()) {
        size_t bar = prompt.find('|', start);
        sv part = trim(prompt.substr(start, bar == sv::npos ? sv::npos : bar - start));
        size_t colon = part.find(':');
        if (colon != sv::npos) {
            sv label = trim(part.substr(0, colon));
            if (is_label(label) && !get(found, label))
                found.emplace_back(label, trim(part.substr(colon + 1)));
        }
        if (bar == sv::npos) break;
        start = bar + 1;
    }
    return found;
}

optional<__int128> arithmetic(sv expression) {
    sv trimmed = trim_end_char(trim(expression), '.');
    // `\bmod(?:ulo)?\b` -> `%`, then `×`/`x` -> `*`, `÷` -> `/`, `^` -> `**`.
    string text;
    text.reserve(trimmed.size() + 8);
    for (size_t i = 0; i < trimmed.size();) {
        if ((i == 0 || !word_byte(trimmed[i - 1])) && istarts(trimmed.substr(i), "mod")) {
            size_t len = 0;
            if (istarts(trimmed.substr(i), "modulo") && boundary(trimmed, i + 6)) len = 6;
            else if (boundary(trimmed, i + 3)) len = 3;
            if (len) {
                text += '%';
                i += len;
                continue;
            }
        }
        if (trimmed.substr(i, 2) == "\xC3\x97") text += '*', i += 2;
        else if (trimmed.substr(i, 2) == "\xC3\xB7") text += '/', i += 2;
        else if (trimmed[i] == 'x') text += '*', ++i;
        else if (trimmed[i] == '^') text += "**", ++i;
        else text += trimmed[i++];
    }
    if (text.empty()) return nullopt;
    for (sv c : chars(text)) {
        bool ok = c.size() == 1 ? (ascii_digit(c[0]) || ascii_space(c[0]) ||
                                   sv("+-*/%()").find(c[0]) != sv::npos)
                                : is_space(c);
        if (!ok) return nullopt;
    }
    Parser parser;
    sv t = text;
    for (size_t i = 0; i < t.size();) {
        if (ascii_digit(t[i])) {
            sv d = digits_at(t, i);
            parser.tokens.push_back(d);
            i += d.size();
        } else if (t.substr(i, 2) == "**" || t.substr(i, 2) == "//") {
            parser.tokens.push_back(t.substr(i, 2));
            i += 2;
        } else if (sv("+-*/%()").find(t[i]) != sv::npos) {
            parser.tokens.push_back(t.substr(i, 1));
            ++i;
        } else {
            ++i;
        }
    }
    auto value = parser.sum();
    if (!value || parser.at != parser.tokens.size()) return nullopt;
    return value;
}

optional<string> solve(sv prompt) {
    auto parts = segments(prompt);
    auto task_segment = get(parts, "TASK");
    if (!task_segment) return nullopt;
    sv task = trim_end_char(trim(*task_segment), '.');
    if (task.empty()) return nullopt;
    auto text = data_segment(parts);

    if (iequals(task, "the final position")) return final_position(prompt, parts);

    if (text) {
        Cursor m{task};
        sv kind_text, base;
        if (m.lit("exactly one character is ") && (m.one({"a ", "an "}), true) &&
            m.one({"digit", "number", "lowercase letter", "lower case letter", "uppercase letter",
                   "upper case letter", "vowel", "consonant", "letter", "symbol",
                   "punctuation mark", "space"},
                  &kind_text) &&
            m.opt("s") && m.opt(",") && m.lit(" ") && m.one({"give", "what is", "return", "find"}) &&
            m.lit(" its position") && m.opt(",") && m.lit(" counting from ") &&
            m.one({"1", "0"}, &base) && m.end()) {
            string kind;
            for (char c : to_lower(kind_text))
                if (c != ' ') kind += c;
            auto wanted = [&](sv c) {
                if (kind == "digit" || kind == "number") return c.size() == 1 && ascii_digit(c[0]);
                if (kind == "lowercaseletter") return is_lower(c);
                if (kind == "uppercaseletter") return is_upper(c);
                if (kind == "vowel") return is_vowel(c);
                if (kind == "consonant") return is_alpha(c) && !is_vowel(c);
                if (kind == "letter") return is_alpha(c);
                if (kind == "space") return c == " ";
                return !is_alnum(c) && !is_space(c);
            };
            size_t count = 0, found = 0, index = 0;
            for (sv c : chars(*text)) {
                if (wanted(c)) ++count, found = index;
                ++index;
            }
            if (count != 1) return nullopt;
            return std::to_string(found + size_t(base[0] - '0'));
        }
    }

    if (auto start = get(parts, "START")) {
        // `^how many (?:(\w+) )?tokens do you (?:hold|have)(?: at the end| in total| now)*$`
        auto tail = [&](size_t at) {
            Cursor m{task, at};
            if (!m.lit("tokens do you ") || !m.one({"hold", "have"})) return false;
            while (!m.end())
                if (!m.one({" at the end", " in total", " now"})) return false;
            return true;
        };
        Cursor m{task};
        if (m.lit("how many ")) {
            sv colour = word_at(task, m.at);
            size_t after = m.at + colour.size();
            bool with_colour = !colour.empty() && after < task.size() && task[after] == ' ' &&
                               tail(after + 1);
            if (with_colour || tail(m.at)) {
                optional<sv> wanted;
                if (with_colour && !iequals(colour, "total")) wanted = colour;
                return tokens(*start, wanted);
            }
        }
    }

    if (auto examples = get(parts, "EXAMPLES")) {
        Cursor m{task};
        sv query;
        if (m.lit("the same ") && m.opt("hidden ") && m.lit("rule") && m.opt("s") &&
            m.lit(" transform") && m.opt("s") && m.lit(" ") && m.word(query) &&
            m.lit(" into what") && m.end())
            return hidden_rules(*examples, query);
    }

    for (sv transform : {"rotate the grid 90 degrees clockwise",
                         "rotate the grid 90 degrees counterclockwise",
                         "rotate the grid 90 degrees counter-clockwise",
                         "rotate the grid 90 degrees anticlockwise",
                         "rotate the grid 90 degrees anti-clockwise", "rotate the grid 180 degrees",
                         "transpose the grid", "flip the grid horizontally",
                         "flip the grid left to right", "flip the grid vertically",
                         "flip the grid upside down"}) {
        Cursor m{task};
        if (!m.lit(transform) || !m.opt(",") || !m.lit(" then read the ")) continue;
        auto rows_tail = [&](size_t at) { return iequals(task.substr(at), "rows left to right"); };
        sv w = word_at(task, m.at);
        size_t after = m.at + w.size();
        bool matched = (!w.empty() && after < task.size() && task[after] == ' ' && rows_tail(after + 1)) ||
                       rows_tail(m.at);
        if (!matched) continue;
        // The label carries a note ("GRID (three rows)"), so it is not in `parts`.
        auto rows = grid_rows(prompt);
        if (!rows) return nullopt;
        return grid(*rows, task.substr(0, transform.size()));
    }

    if (auto brackets = get(parts, "BRACKETS")) {
        Cursor m{task};
        if (m.lit("the maximum nesting depth") && boundary(task, m.at) && m.rest_is_line()) {
            sv raw = m.rest();
            sv rest = trim(raw);
            if (rest.empty() || (rest[0] == '(' && rest.find("then") == sv::npos))
                return nesting(*brackets, false);
            // `^(?: \(.*?\))?,? then the position of the bracket where that depth
            // is first reached, counting from 1$`
            auto reached = [&](size_t at) {
                Cursor t{raw, at};
                t.opt(",");
                return t.lit(" then the position of the bracket where that depth is first reached, "
                             "counting from 1") &&
                       t.end();
            };
            bool first = reached(0);
            if (!first && raw.substr(0, 2) == " (")
                for (size_t close = raw.find(')', 2); !first && close != sv::npos;
                     close = raw.find(')', close + 1))
                    first = reached(close + 1);
            if (!first) return nullopt;
            return nesting(*brackets, true);
        }
    }

    {
        Cursor m{task};
        if (m.lit("compute ") && !m.end() && m.rest_is_line()) {
            auto value = arithmetic(m.rest());
            if (!value) return nullopt;
            return to_string(*value);
        }
    }

    if (text) {
        Cursor m{task};
        sv letter;
        if (m.lit("how many times does the letter ") && (m.one({"'", "\""}), true) &&
            m.at < task.size() && word_byte(task[m.at])) {
            letter = task.substr(m.at++, 1);
            if ((m.one({"'", "\""}), true) && m.lit(" appear") && m.opt(" in the text") && m.end()) {
                char wanted = upper(letter[0]);
                size_t count = 0;
                for (sv c : chars(*text))
                    if (c.size() == 1 && upper(c[0]) == wanted) ++count;
                return std::to_string(count);
            }
        }
    }

    if (text) {
        Cursor m{task};
        sv kind;
        if (m.lit("how many ") && m.one({"vowels", "consonants", "letters", "words"}, &kind) &&
            m.lit(" ") && m.one({"are there", "does it contain", "appear"}) && m.end()) {
            size_t count = 0;
            if (iequals(kind, "words")) {
                count = words(*text).size();
            } else {
                for (sv c : chars(*text)) {
                    if (!is_alpha(c)) continue;
                    if (iequals(kind, "letters") || (iequals(kind, "vowels") == is_vowel(c))) ++count;
                }
            }
            return std::to_string(count);
        }
    }

    if (text) {
        // `^apply rot ?(\d+)(?: \(.*\))?$|^shift every letter (forward|back(?:ward)?) by (\d+)(?: \(.*\))?$`
        auto note_tail = [](const Cursor& m) {
            sv r = m.rest();
            return m.rest_is_line() &&
                   (r.empty() || (r.size() >= 3 && r.substr(0, 2) == " (" && r.back() == ')'));
        };
        Cursor m{task};
        sv n, dir;
        if (m.lit("apply rot") && m.opt(" ") && m.digits(n) && note_tail(m)) {
            int64_t amount;
            if (!parse_i64(n, amount)) return nullopt;
            return shift(*text, amount);
        }
        m = Cursor{task};
        if (m.lit("shift every letter ") && m.one({"forward", "backward", "back"}, &dir) &&
            m.lit(" by ") && m.digits(n) && note_tail(m)) {
            int64_t amount;
            if (!parse_i64(n, amount)) return nullopt;
            return shift(*text, iequals(dir, "forward") ? amount : -amount);
        }
    }

    {
        // `^(?:the |take )?word (?:at position|number) (\d+),? counting from (1|the end|the start)(.*)$`
        Cursor m{task};
        sv n, from;
        m.one({"the ", "take "});
        if (m.lit("word ") && m.one({"at position", "number"}) && m.lit(" ") && m.digits(n) &&
            m.opt(",") && m.lit(" counting from ") && m.one({"1", "the end", "the start"}, &from) &&
            m.rest_is_line()) {
            if (!text) return nullopt;
            auto list = words(*text);
            uint64_t number;
            if (!parse_u64(n, number) || number == 0 || number > list.size()) return nullopt;
            bool from_end = to_lower(from).find("end") != string::npos;
            sv word = from_end ? list[list.size() - number] : list[number - 1];
            sv rest = trim_start_char(trim(m.rest()), ',');
            if (rest.empty()) return string(word);
            return pipeline(word, rest);
        }
    }

    {
        // `^the (?:word (immediately |directly |just )?(before|after) the )?(longest|shortest) word$`
        Cursor m{task};
        sv direction, extreme;
        bool matched = m.lit("the ");
        if (matched && m.lit("word ")) {
            m.one({"immediately ", "directly ", "just "});
            matched = m.one({"before", "after"}, &direction) && m.lit(" the ");
        }
        if (matched && m.one({"longest", "shortest"}, &extreme) && m.lit(" word") && m.end()) {
            if (!text) return nullopt;
            auto list = words(*text);
            if (list.empty()) return nullopt;
            std::vector<size_t> lengths;
            for (sv w : list) lengths.push_back(chars(w).size());
            size_t target = iequals(extreme, "longest") ? *std::max_element(lengths.begin(), lengths.end())
                                                        : *std::min_element(lengths.begin(), lengths.end());
            size_t count = 0, found = 0;
            for (size_t i = 0; i < list.size(); ++i)
                if (lengths[i] == target) ++count, found = i;
            if (count != 1) return nullopt;  // a tie makes "the longest word" ambiguous
            size_t index = found;
            if (iequals(direction, "before")) {
                if (found == 0) return nullopt;
                index = found - 1;
            } else if (!direction.empty()) {
                index = found + 1;
            }
            if (index >= list.size()) return nullopt;
            return string(list[index]);
        }
    }

    for (sv form : {"write it backward", "write it backwards", "write the text backward",
                    "write the text backwards", "spell it backward", "spell it backwards",
                    "spell the text backward", "spell the text backwards", "reverse it",
                    "reverse the text"})
        if (iequals(task, form)) {
            if (!text) return nullopt;
            return reversed(*text);
        }
    return nullopt;
}

const std::vector<Case> WARMUP_CASES = {
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
    {"WORDS: AbC | TASK: take word number 1, counting from 1, reverse it, drop every vowel, "
     "uppercase, lowercase, apply rot1, shift every letter backward by 1, sort its letters, double "
     "every letter",
     "bbcc"},
    {"WORDS: AbC | TASK: take word number 1, counting from 1, drop every consonant", "A"},
    {"START at 0,0 | MOVES: URDL (U adds 1 to y, R adds 1 to x, D subtracts 1 from y, L subtracts "
     "1 from x) | TASK: the final position",
     "0,0"},
    {"TEXT: AB1CD | TASK: exactly one character is a digit, give its position, counting from 1", "3"},
    {"START: you hold 5 red tokens and 7 blue tokens. You take 3 blue tokens. You give away 1 blue "
     "token. You do NOT take 2 red tokens. You give away no blue tokens. | TASK: how many blue "
     "tokens do you hold",
     "9"},
    {"EXAMPLES: adac -> ycdyg ; aac -> ycyg ; aaac -> ycycyg ; bbb -> bbb | TASK: the same hidden "
     "rules transform ddac into what",
     "ddyg"},
    {"GRID (two rows): AB / CD | TASK: rotate the grid 90 degrees clockwise, then read the rows "
     "left to right",
     "CADB"},
    {"GRID (two rows): AB / CD | TASK: rotate the grid 90 degrees counterclockwise, then read the "
     "rows left to right",
     "BDAC"},
    {"GRID (two rows): AB / CD | TASK: rotate the grid 180 degrees, then read the rows left to "
     "right",
     "DCBA"},
    {"GRID (two rows): AB / CD | TASK: transpose the grid, then read the rows left to right", "ACBD"},
    {"GRID (two rows): AB / CD | TASK: flip the grid horizontally, then read the rows left to right",
     "BADC"},
    {"GRID (two rows): AB / CD | TASK: flip the grid vertically, then read the rows left to right",
     "CDAB"},
    {"BRACKETS: (()) | TASK: the maximum nesting depth", "2"},
    {"BRACKETS: (()) | TASK: the maximum nesting depth, then the position of the bracket where that "
     "depth is first reached, counting from 1",
     "2,2"},
};

void warm() {
    for (auto& c : WARMUP_CASES) {
        auto answer = solve(c.prompt);
        asm volatile("" : : "r"(&answer) : "memory");
    }
}

}  // namespace solvers
