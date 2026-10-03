// Exact solvers for Agents War prompts (`agent_wars_text_v1`), ported from
// src/solvers.rs. Every regex there is a hand-written matcher here with the
// same leftmost-first semantics; tests/corpus replays every verified prompt.
#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace solvers {

/// Labelled segments in prompt order, first occurrence of each label only.
using Segments = std::vector<std::pair<std::string_view, std::string_view>>;
Segments segments(std::string_view prompt);

std::optional<__int128> arithmetic(std::string_view expression);
std::string to_string(__int128 value);

/// `nullopt` for anything not fully understood (never a guess).
std::optional<std::string> solve(std::string_view prompt);

struct Case {
    const char* prompt;
    const char* expected;
};
extern const std::vector<Case> WARMUP_CASES;

/// Runs every solver family once, off the clock.
void warm();

}  // namespace solvers
