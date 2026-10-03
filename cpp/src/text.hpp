// Character helpers shared by the solvers. Prompts are ASCII in practice; any
// other UTF-8 character is kept whole (one "char", like Rust's `chars()`), and
// classified with a small Latin/Greek/Cyrillic approximation.
#pragma once

#include <cstdint>
#include <string>
#include <string_view>
#include <vector>

namespace text {

using sv = std::string_view;

inline bool ascii_space(unsigned char c) { return c == ' ' || (c >= 9 && c <= 13); }
inline bool ascii_digit(unsigned char c) { return c >= '0' && c <= '9'; }
inline bool ascii_upper(unsigned char c) { return c >= 'A' && c <= 'Z'; }
inline bool ascii_lower(unsigned char c) { return c >= 'a' && c <= 'z'; }
inline bool ascii_alpha(unsigned char c) { return ascii_upper(c) || ascii_lower(c); }
/// Regex `\w` on ASCII.
inline bool word_byte(unsigned char c) { return ascii_alpha(c) || ascii_digit(c) || c == '_'; }
inline char lower(char c) { return ascii_upper(c) ? char(c + 32) : c; }
inline char upper(char c) { return ascii_lower(c) ? char(c - 32) : c; }

/// One UTF-8 character per element (a malformed byte is one character).
std::vector<sv> chars(sv s);
uint32_t code_point(sv ch);

bool is_space(sv ch);
bool is_alpha(sv ch);
bool is_lower(sv ch);
bool is_upper(sv ch);
bool is_alnum(sv ch);
bool is_vowel(sv ch);
std::string to_upper(sv s);
std::string to_lower(sv s);

/// Rust `str::trim` (Unicode whitespace).
sv trim(sv s);
sv trim_end_char(sv s, char c);
sv trim_start_char(sv s, char c);

/// Case-insensitive (ASCII) prefix/equality.
bool istarts(sv s, sv prefix);
bool iequals(sv a, sv b);

/// `str::replace`: non-overlapping, left to right. `from` must not be empty.
std::string replace_all(sv s, sv from, sv to);
size_t count_matches(sv s, sv needle);

/// Regex `\b` at byte offset `at` (ASCII word characters).
inline bool boundary(sv s, size_t at) {
    bool before = at > 0 && word_byte(s[at - 1]);
    bool after = at < s.size() && word_byte(s[at]);
    return before != after;
}

/// Leading run of ASCII digits, as a string view.
inline sv digits_at(sv s, size_t at) {
    size_t end = at;
    while (end < s.size() && ascii_digit(s[end])) ++end;
    return s.substr(at, end - at);
}

/// Leading run of `\w`.
inline sv word_at(sv s, size_t at) {
    size_t end = at;
    while (end < s.size() && word_byte(s[end])) ++end;
    return s.substr(at, end - at);
}

/// Decimal parse with overflow detection (Rust `str::parse::<i64>`; digits only).
bool parse_i64(sv digits, int64_t& out);
bool parse_u64(sv digits, uint64_t& out);

}  // namespace text
