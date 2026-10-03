// The race loop. Everything off the hot path happens before `startRunV2`
// (Turnstile, two warm routes, solver warmup, Chrome killed); in the loop a
// question costs one solve (microseconds) plus one hedged round trip. Logs stay
// in memory as plain records and become JSON once the run is over.
#pragma once

#include <optional>
#include <string>
#include <vector>

#include "../vendor/json.hpp"
#include "http.hpp"
#include "llm.hpp"
#include "rpc.hpp"

namespace race {

using json = nlohmann::json;

/// Logged right before `startRunV2`; a failure without it spent no attempt.
inline constexpr const char* STARTING_RUN = "starting the run";

struct Config {
    std::string code;
    std::string email;
    std::optional<std::string> nickname;
    std::string locale;
    http::Duration hedge_after;
    size_t max_requests;
    std::string runs_dir;
    /// When positive: give up a race whose first `abort_after` answers took
    /// longer than `abort_limit`. It will not be a best time, and the next
    /// race starts sooner.
    size_t abort_after = 0;
    http::Duration abort_limit{};
};

class Session {
public:
    Session(const std::string& code, const std::string& user_agent, const std::string& cookie,
            size_t candidates, const std::string& origin = rpc::ORIGIN);

    /// Open TLS on every route and return the competition.
    json warm();
    /// Round trips of the (harmless) `getCompetition` call on each route.
    std::vector<std::vector<http::Duration>> bench(size_t rounds);

    std::vector<rpc::Rpc> routes;
    json base;
};

/// `/play/SUPERCHALLENGE-JAWVUX` -> `JAWVUX` (the slug is cosmetic).
std::string competition_code(const std::string& play_url);
/// Per-question deadline; mirrors the client's linear ramp exactly.
double deadline_ms(const json& agent_wars, size_t answered);
/// Drills of an RPC response: `drills`, `next`, or nested one level down.
std::vector<json> find_drills(const json& value);
std::string drill_prompt(const json& drill);

void race(Session& session, const Config& config, const std::string& turnstile_token, Llm* llm);

}  // namespace race
