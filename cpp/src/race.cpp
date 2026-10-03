#include "race.hpp"

#include <algorithm>
#include <cmath>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <stdexcept>
#include <variant>

#include "report.hpp"
#include "solvers.hpp"

namespace race {

using http::Clock;
using http::Duration;
using namespace std::chrono_literals;

namespace {

/// Margin kept between an LLM fallback and the question deadline.
constexpr Duration SAFETY = 250ms;
/// Time an LLM fallback always gets, even past the deadline: the alternative
/// is "?", which is wrong anyway, so a late answer can only help.
constexpr Duration LLM_FLOOR = 4s;

std::string dump(const json& value) { return value.dump(-1, ' ', false, json::error_handler_t::replace); }

const json* member(const json& value, const char* key) {
    if (!value.is_object()) return nullptr;
    auto found = value.find(key);
    return found == value.end() ? nullptr : &*found;
}

json optional_text(const std::optional<std::string>& value) { return value ? json(*value) : json(nullptr); }


/// One answered question, kept as is until the run ends.
struct Answer {
    size_t index;
    json drill;
    std::string submission;
    const char* source;
    Duration solve, round_trip;
    double t;
    rpc::HedgedResponse reply;
};

json answer_record(const Answer& a);

/// Lines in event order; answers are only rendered when the log is saved.
class Log {
public:
    void write(const char* event, json data) {
        if (data.is_object()) {
            data["t"] = unix_now();
            data["event"] = event;
        }
        entries_.emplace_back(dump(data));
    }
    void answer(size_t index) { entries_.emplace_back(index); }

    std::string save(const std::string& dir, const std::vector<Answer>& answers) const {
        std::filesystem::create_directories(dir);
        auto path = std::filesystem::path(dir) / ("race-" + std::to_string(uint64_t(unix_now())) + ".jsonl");
        std::ofstream file(path);
        for (auto& entry : entries_) {
            if (auto* line = std::get_if<std::string>(&entry)) file << *line << "\n";
            else file << dump(answer_record(answers[std::get<size_t>(entry)])) << "\n";
        }
        file.close();
        if (!file) throw std::runtime_error("cannot write " + path.string());
        return path.string();
    }

private:
    std::vector<std::variant<std::string, size_t>> entries_;
};

std::pair<std::string, const char*> answer(Llm* llm, const std::string& prompt, Duration budget) {
    if (auto exact = solvers::solve(prompt)) return {std::move(*exact), "exact"};
    if (llm)
        if (auto text = llm->answer(prompt, std::max(budget, LLM_FLOOR))) return {std::move(*text), "llm"};
    return {"?", "none"};
}

void print_summary(const std::vector<Answer>& answers) {
    if (answers.empty()) return;
    std::vector<double> rtts, headers;
    Duration solve_max{}, body_max{};
    size_t fallbacks = 0;
    for (auto& a : answers) {
        rtts.push_back(ms(a.round_trip));
        headers.push_back(ms(a.reply.timing.headers));
        solve_max = std::max(solve_max, a.solve);
        body_max = std::max(body_max, a.reply.timing.body);
        fallbacks += std::string_view(a.source) != "exact";
    }
    std::sort(rtts.begin(), rtts.end());
    std::sort(headers.begin(), headers.end());
    auto pick = [](const std::vector<double>& v, double q) {
        return v[size_t(std::round(double(v.size() - 1) * q))];
    };
    say(format("rtt ms: p50 %.1f p90 %.1f max %.1f | slowest solve %.3f ms | non-exact answers %zu",
               pick(rtts, 0.5), pick(rtts, 0.9), pick(rtts, 1.0), ms(solve_max), fallbacks));
    say(format("response headers ms: p50 %.1f p90 %.1f | slowest body read %.3f ms", pick(headers, 0.5),
               pick(headers, 0.9), ms(body_max)));
}

json answer_record(const Answer& a) {
    auto& timing = a.reply.timing;
    return json{
        {"index", a.index},
        {"drill", a.drill},
        {"submission", a.submission},
        {"source", a.source},
        {"solve_ms", ms(a.solve)},
        {"rtt_ms", ms(a.round_trip)},
        {"headers_ms", ms(timing.headers)},
        {"body_ms", ms(timing.body)},
        {"request_ms", ms(timing.total)},
        {"requests", a.reply.sent},
        {"winner_route", a.reply.winner_route},
        {"http_version", timing.version},
        {"peer", optional_text(timing.peer)},
        {"x_vercel_id", optional_text(timing.vercel_id)},
        {"server_timing", optional_text(timing.server_timing)},
        {"response", a.reply.value},
        {"t", a.t},
        {"event", "answer"},
    };
}

/// `QUIZ_SC_EDGE_IP=a+b`: route i connects to the i-th address in turn.
std::string edge_ip(size_t route) {
    const char* value = std::getenv("QUIZ_SC_EDGE_IP");
    std::vector<std::string> edges;
    std::string current;
    for (const char* c = value ? value : ""; ; ++c) {
        if (*c == '+' || *c == '\0') {
            if (!current.empty()) edges.push_back(current);
            current.clear();
            if (*c == '\0') break;
        } else if (*c != ' ') {
            current += *c;
        }
    }
    return edges.empty() ? "" : edges[route % edges.size()];
}

}  // namespace

Session::Session(const std::string& code, const std::string& user_agent, const std::string& cookie,
                 size_t candidates, const std::string& origin) {
    if (candidates < 2 || candidates > 16)
        throw std::invalid_argument("connection candidates must be between 2 and 16");
    for (size_t i = 0; i < candidates; ++i)
        routes.emplace_back(origin, user_agent, cookie, origin == rpc::ORIGIN ? edge_ip(i) : "");
    base = json{{"productId", rpc::PRODUCT_ID}, {"code", code}};
}

json Session::warm() {
    std::optional<json> competition;
    for (size_t i = 0; i < routes.size(); ++i) {
        auto [value, info] = routes[i].inspect("getCompetition", base);
        say(format("warmup route %zu: %s peer=%s x-vercel-id=%s server-timing=%s", i, info.version.c_str(),
                   info.peer.value_or("None").c_str(), info.vercel_id.value_or("None").c_str(),
                   info.server_timing.value_or("None").c_str()));
        if (!competition) competition = std::move(value);
    }
    if (!competition) throw std::runtime_error("no route");
    return *competition;
}

std::vector<std::vector<Duration>> Session::bench(size_t rounds) {
    std::vector<std::vector<Duration>> timings(routes.size());
    for (size_t round = 0; round < rounds; ++round)
        for (size_t offset = 0; offset < routes.size(); ++offset) {
            size_t index = (round + offset) % routes.size();
            auto started = Clock::now();
            try {
                routes[index].call("getCompetition", base);
                timings[index].push_back(Clock::now() - started);
            } catch (const rpc::RpcError& error) {
                if (error.status == 429)
                    throw std::runtime_error(std::string("connection probing throttled; no attempt started: ") + error.what());
                say(format("route %zu: failed after %.1f ms: %s", index, ms(Clock::now() - started), error.what()));
            } catch (const std::exception& error) {
                say(format("route %zu: failed after %.1f ms: %s", index, ms(Clock::now() - started), error.what()));
            }
        }
    return timings;
}

std::string competition_code(const std::string& play_url) {
    std::string path = play_url.substr(0, play_url.find_first_of("?#"));
    while (!path.empty() && path.back() == '/') path.pop_back();
    std::string segment = path.substr(path.rfind('/') == std::string::npos ? 0 : path.rfind('/') + 1);
    size_t dash = segment.rfind('-');
    return dash == std::string::npos ? segment : segment.substr(dash + 1);
}

double deadline_ms(const json& agent_wars, size_t answered) {
    auto number = [&](const char* key) -> std::optional<double> {
        const json* v = member(agent_wars, key);
        if (v && v->is_number()) return v->get<double>();
        return std::nullopt;
    };
    double floor = 1000.0 * number("questionDeadlineSec").value_or(2.0);
    auto start_sec = number("questionDeadlineStartSec");
    double start = start_sec ? 1000.0 * *start_sec : floor;
    const json* ramp_value = member(agent_wars, "questionDeadlineRampQuestions");
    double ramp = 0;
    if (ramp_value && ramp_value->is_number_unsigned()) ramp = double(ramp_value->get<uint64_t>());
    else if (ramp_value && ramp_value->is_number_integer() && ramp_value->get<int64_t>() >= 0)
        ramp = double(ramp_value->get<int64_t>());
    if (ramp <= 0.0 || start <= floor) return std::round(floor);
    return std::round(start - (start - floor) * std::min(double(answered), ramp) / ramp);
}

std::vector<json> find_drills(const json& value) {
    std::vector<json> found;
    auto take = [&](const json& holder) {
        if (const json* drills = member(holder, "drills"); drills && drills->is_array())
            for (auto& d : *drills)
                if (d.is_object()) found.push_back(d);
    };
    take(value);
    if (const json* next = member(value, "next"); next && next->is_object()) found.push_back(*next);
    if (found.empty() && value.is_object())
        for (auto& nested : value) take(nested);
    return found;
}

std::string drill_prompt(const json& drill) {
    const json* data = member(drill, "patternData");
    if (!data) return "null";
    if (const json* prompt = member(*data, "prompt"); prompt && prompt->is_string()) return *prompt;
    return dump(*data);
}

void race(Session& session, const Config& config, const std::string& turnstile_token, Llm* llm) {
    Log log;
    std::vector<Answer> answers;
    answers.reserve(256);
    std::exception_ptr failure;
    try {
        json start_input = {
            {"productId", rpc::PRODUCT_ID}, {"code", config.code},
            {"locale", config.locale},      {"uiLocale", config.locale},
            {"turnstileToken", turnstile_token}, {"email", config.email},
        };
        // From here on the attempt counts as spent, even if the call fails.
        say(STARTING_RUN);
        auto race_started = Clock::now();
        // Never duplicated: a second startRunV2 could spend a second attempt.
        json start = session.routes[0].call("startRunV2", start_input);
        auto received = Clock::now();
        json start_event{{"response", start}};
        // Set by the Rust `serve` platform: what this race tries.
        if (const char* variant = std::getenv("QUIZ_SC_VARIANT"))
            start_event["variant"] = json::parse(variant, nullptr, false);
        log.write("start", start_event);
        const json* token = member(start, "runToken");
        if (!token || !token->is_string()) throw std::runtime_error("no runToken");
        std::string run_token = *token;
        json agent_wars;
        if (const json* setup = member(start, "setup"))
            if (const json* aw = member(*setup, "agentWars")) agent_wars = *aw;
        auto queue = find_drills(start);
        size_t answered = 0;
        Duration rtt = 150ms;
        // Everything in a submission but the drill id and the answer.
        std::string body_prefix = R"({"json":{"productId":"superchallenge","code":)" + dump(config.code) +
                                  R"(,"runToken":)" + dump(run_token) + R"(,"drillId":)";
        std::string body;
        std::string ended;
        while (true) {
            if (answered >= queue.size()) {
                ended = "no drill left";
                break;
            }
            const json& drill = queue[answered];
            std::string prompt = drill_prompt(drill);
            auto deadline = received + std::chrono::milliseconds(int64_t(deadline_ms(agent_wars, answered)));
            auto now = Clock::now();
            Duration budget = deadline > now + rtt + SAFETY ? deadline - (now + rtt + SAFETY) : Duration::zero();
            auto thought = Clock::now();
            auto [submission, source] = answer(llm, prompt, budget);
            auto solved = Clock::now();
            const json* id = member(drill, "id");
            body.assign(body_prefix);
            body += id ? dump(*id) : "null";
            body += R"(,"submission":)";
            body += dump(submission);
            body += "}}";
            rpc::HedgedResponse reply;
            try {
                reply = rpc::hedged_raw(session.routes, "submitAnswerV2", body, config.hedge_after,
                                        config.max_requests);
            } catch (const rpc::RpcError& error) {
                if (error.status != 409) throw;
                // Too late: the run is over but its score can still be saved.
                say(format("[%zu] %s (%s answer \"%s\" for: %s)", answered + 1, error.what(), source,
                           submission.c_str(), prompt.c_str()));
                log.write("answer", json{{"index", answered}, {"drill", drill}, {"submission", submission},
                                         {"source", source}, {"error", error.what()}});
                ended = error.what();
                break;
            }
            received = Clock::now();
            Duration round_trip = received - solved;
            rtt = (rtt * 7 + round_trip * 3) / 10;
            const json* is_correct = member(reply.value, "isCorrect");
            bool correct = is_correct && is_correct->is_boolean() && is_correct->get<bool>();
            const json* ended_value = member(reply.value, "ended");
            bool over = ended_value && !ended_value->is_null();
            std::vector<json> next = find_drills(reply.value);
            answers.push_back(Answer{answered, drill, submission, source, solved - thought, round_trip,
                                     unix_now(), std::move(reply)});
            log.answer(answers.size() - 1);
            const json& response = answers.back().reply.value;
            if (std::string_view(source) != "exact") {
                say(format("[%zu] UNKNOWN (%s, %s) answered \"%s\" for: %s", answered + 1, source,
                           correct ? "right" : "wrong", submission.c_str(), prompt.c_str()));
            } else if (!correct) {
                say(format("[%zu] WRONG \"%s\" for: %s", answered + 1, submission.c_str(), prompt.c_str()));
            }
            if (!correct || over) {
                answered += correct;
                const json* score = member(response, "runningScore");
                ended = (ended_value ? dump(*ended_value) : "null") + " (score " + (score ? dump(*score) : "null") + ")";
                break;
            }
            ++answered;
            for (auto& n : next) {
                const json* next_id = member(n, "id");
                bool known = std::any_of(queue.begin(), queue.end(), [&](const json& d) {
                    const json* known_id = member(d, "id");
                    return (known_id ? *known_id : json()) == (next_id ? *next_id : json());
                });
                if (!known) queue.push_back(std::move(n));
            }
        }
        auto elapsed = Clock::now() - race_started;
        say(format("run ended: %s after %zu correct in %.3fs", ended.c_str(), answered,
                   std::chrono::duration<double>(elapsed).count()));
        print_summary(answers);
        if (config.nickname && !config.nickname->empty()) {
            json input = session.base;
            input["runToken"] = run_token;
            input["email"] = config.email;
            input["nickname"] = *config.nickname;
            json saved = session.routes[0].call("submitScoreV2", input);
            say("score saved: " + dump(saved));
            log.write("score", json{{"response", saved}});
        } else {
            say("no nickname: score not submitted to the leaderboard");
        }
    } catch (const std::exception& error) {
        json data = nullptr;
        if (auto* rpc_error = dynamic_cast<const rpc::RpcError*>(&error)) data = rpc_error->body;
        log.write("error", json{{"error", error.what()}, {"data", data}});
        failure = std::current_exception();
    }
    try {
        say("log: " + log.save(config.runs_dir, answers));
    } catch (const std::exception& e) {
        say(std::string("could not write the log: ") + e.what());
    }
    if (failure) std::rethrow_exception(failure);
}

}  // namespace race
