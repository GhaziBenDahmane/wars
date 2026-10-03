#include "text.hpp"

namespace text {

std::vector<sv> chars(sv s) {
    std::vector<sv> out;
    out.reserve(s.size());
    size_t i = 0;
    while (i < s.size()) {
        unsigned char c = s[i];
        size_t len = c < 0x80 ? 1 : c >= 0xF0 ? 4 : c >= 0xE0 ? 3 : c >= 0xC0 ? 2 : 1;
        if (i + len > s.size()) len = 1;
        for (size_t k = 1; k < len; ++k)
            if ((static_cast<unsigned char>(s[i + k]) & 0xC0) != 0x80) {
                len = 1;
                break;
            }
        out.push_back(s.substr(i, len));
        i += len;
    }
    return out;
}

uint32_t code_point(sv ch) {
    auto b = [&](size_t i) { return uint32_t(static_cast<unsigned char>(ch[i])); };
    switch (ch.size()) {
        case 1: return b(0);
        case 2: return ((b(0) & 0x1F) << 6) | (b(1) & 0x3F);
        case 3: return ((b(0) & 0x0F) << 12) | ((b(1) & 0x3F) << 6) | (b(2) & 0x3F);
        case 4:
            return ((b(0) & 0x07) << 18) | ((b(1) & 0x3F) << 12) | ((b(2) & 0x3F) << 6) |
                   (b(3) & 0x3F);
        default: return 0xFFFD;
    }
}

static std::string encode(uint32_t cp) {
    std::string out;
    if (cp < 0x80) {
        out += char(cp);
    } else if (cp < 0x800) {
        out += char(0xC0 | (cp >> 6));
        out += char(0x80 | (cp & 0x3F));
    } else if (cp < 0x10000) {
        out += char(0xE0 | (cp >> 12));
        out += char(0x80 | ((cp >> 6) & 0x3F));
        out += char(0x80 | (cp & 0x3F));
    } else {
        out += char(0xF0 | (cp >> 18));
        out += char(0x80 | ((cp >> 12) & 0x3F));
        out += char(0x80 | ((cp >> 6) & 0x3F));
        out += char(0x80 | (cp & 0x3F));
    }
    return out;
}

bool is_space(sv ch) {
    if (ch.size() == 1) return ascii_space(ch[0]);
    uint32_t cp = code_point(ch);
    return cp == 0x85 || cp == 0xA0 || cp == 0x1680 || (cp >= 0x2000 && cp <= 0x200A) ||
           cp == 0x2028 || cp == 0x2029 || cp == 0x202F || cp == 0x205F || cp == 0x3000;
}

// Latin-1 letters, Latin Extended-A/B, Greek, Cyrillic.
static bool latin_upper(uint32_t cp) {
    return (cp >= 0xC0 && cp <= 0xDE && cp != 0xD7) || (cp >= 0x391 && cp <= 0x3AB) ||
           (cp >= 0x410 && cp <= 0x42F) || (cp >= 0x100 && cp <= 0x17F && cp % 2 == 0);
}
static bool latin_lower(uint32_t cp) {
    return (cp >= 0xDF && cp <= 0xFF && cp != 0xF7) || (cp >= 0x3B1 && cp <= 0x3CB) ||
           (cp >= 0x430 && cp <= 0x44F) || (cp >= 0x100 && cp <= 0x17F && cp % 2 == 1);
}

bool is_upper(sv ch) {
    return ch.size() == 1 ? ascii_upper(ch[0]) : latin_upper(code_point(ch));
}
bool is_lower(sv ch) {
    return ch.size() == 1 ? ascii_lower(ch[0]) : latin_lower(code_point(ch));
}
bool is_alpha(sv ch) {
    if (ch.size() == 1) return ascii_alpha(ch[0]);
    uint32_t cp = code_point(ch);
    return latin_upper(cp) || latin_lower(cp) || (cp >= 0x180 && cp <= 0x24F);
}
bool is_alnum(sv ch) { return is_alpha(ch) || (ch.size() == 1 && ascii_digit(ch[0])); }

bool is_vowel(sv ch) {
    if (ch.size() != 1) return false;
    switch (ch[0]) {
        case 'A': case 'E': case 'I': case 'O': case 'U':
        case 'a': case 'e': case 'i': case 'o': case 'u': return true;
        default: return false;
    }
}

static std::string map_case(sv s, bool up) {
    std::string out;
    out.reserve(s.size());
    for (sv ch : chars(s)) {
        if (ch.size() == 1) {
            out += up ? upper(ch[0]) : lower(ch[0]);
            continue;
        }
        uint32_t cp = code_point(ch);
        bool latin1 = cp >= 0xC0 && cp <= 0xFE && cp != 0xD7 && cp != 0xF7 && cp != 0xDF;
        bool greek_cyr = (cp >= 0x391 && cp <= 0x3CB && cp != 0x3A2) || (cp >= 0x410 && cp <= 0x44F);
        if (up && latin1 && cp >= 0xE0) out += encode(cp - 0x20);
        else if (!up && latin1 && cp < 0xE0) out += encode(cp + 0x20);
        else if (up && greek_cyr && latin_lower(cp)) out += encode(cp - 0x20);
        else if (!up && greek_cyr && latin_upper(cp)) out += encode(cp + 0x20);
        else out += ch;
    }
    return out;
}

std::string to_upper(sv s) { return map_case(s, true); }
std::string to_lower(sv s) { return map_case(s, false); }

sv trim(sv s) {
    while (!s.empty()) {
        if (ascii_space(s.front())) { s.remove_prefix(1); continue; }
        if (static_cast<unsigned char>(s.front()) >= 0x80) {
            auto first = chars(s.substr(0, 4)).front();
            if (is_space(first)) { s.remove_prefix(first.size()); continue; }
        }
        break;
    }
    while (!s.empty()) {
        if (ascii_space(s.back())) { s.remove_suffix(1); continue; }
        if (static_cast<unsigned char>(s.back()) >= 0x80) {
            size_t start = s.size() - 1;
            while (start > 0 && s.size() - start < 4 &&
                   (static_cast<unsigned char>(s[start]) & 0xC0) == 0x80)
                --start;
            auto last = s.substr(start);
            if (chars(last).size() == 1 && is_space(last)) { s.remove_suffix(last.size()); continue; }
        }
        break;
    }
    return s;
}

sv trim_end_char(sv s, char c) {
    while (!s.empty() && s.back() == c) s.remove_suffix(1);
    return s;
}

sv trim_start_char(sv s, char c) {
    while (!s.empty() && s.front() == c) s.remove_prefix(1);
    return s;
}

bool istarts(sv s, sv prefix) {
    if (s.size() < prefix.size()) return false;
    for (size_t i = 0; i < prefix.size(); ++i)
        if (lower(s[i]) != lower(prefix[i])) return false;
    return true;
}

bool iequals(sv a, sv b) { return a.size() == b.size() && istarts(a, b); }

std::string replace_all(sv s, sv from, sv to) {
    std::string out;
    out.reserve(s.size());
    size_t at = 0;
    while (true) {
        size_t found = s.find(from, at);
        if (found == sv::npos) break;
        out.append(s, at, found - at);
        out.append(to);
        at = found + from.size();
    }
    out.append(s, at);
    return out;
}

size_t count_matches(sv s, sv needle) {
    size_t n = 0, at = 0;
    while ((at = s.find(needle, at)) != sv::npos) {
        ++n;
        at += needle.size();
    }
    return n;
}

bool parse_u64(sv digits, uint64_t& out) {
    if (digits.empty()) return false;
    uint64_t value = 0;
    for (char c : digits) {
        if (!ascii_digit(c)) return false;
        if (__builtin_mul_overflow(value, 10u, &value) ||
            __builtin_add_overflow(value, uint64_t(c - '0'), &value))
            return false;
    }
    out = value;
    return true;
}

bool parse_i64(sv digits, int64_t& out) {
    bool negative = !digits.empty() && digits[0] == '-';
    if (negative || (!digits.empty() && digits[0] == '+')) digits.remove_prefix(1);
    uint64_t value;
    if (!parse_u64(digits, value)) return false;
    if (negative) {
        if (value > uint64_t(INT64_MAX) + 1) return false;
        out = int64_t(0 - value);
    } else {
        if (value > uint64_t(INT64_MAX)) return false;
        out = int64_t(value);
    }
    return true;
}

}  // namespace text
