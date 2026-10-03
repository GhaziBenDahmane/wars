// agentwars (C++): the race path of the Rust racer — race, dry-run, bench,
// cookie-bench, solve. Every setting is a flag or its environment variable,
// with the same names and defaults as the Rust binary.
#include <algorithm>
#include <cstdlib>
#include <iostream>
#include <map>
#include <optional>
#include <set>
#include <string>
#include <vector>

#include "browser.hpp"
#include "http.hpp"
#include "llm.hpp"
#include "race.hpp"
#include "report.hpp"
#include "rpc.hpp"
#include "solvers.hpp"

using http::Clock;
using http::Duration;
using json = nlohmann::json;

namespace {

/// Used when no browser is involved (bench, or a token passed in).
const char* USER_AGENT =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 "
    "Safari/537.36";

struct UsageError : std::runtime_error {
    using std::runtime_error::runtime_error;
};

/// `--name value`, `--name=value`, then the environment variable, then the default.
class Args {
public:
    Args(int argc, char** argv, int first) {
        for (int i = first; i < argc; ++i) {
            std::string arg = argv[i];
            if (arg.rfind("--", 0) != 0) {
                positional_.push_back(arg);
                continue;
            }
            std::string name = arg.substr(2);
            size_t eq = name.find('=');
            if (eq != std::string::npos) {
                values_[name.substr(0, eq)] = name.substr(eq + 1);
            } else if (i + 1 < argc && std::string(argv[i + 1]).rfind("--", 0) != 0) {
                values_[name] = argv[++i];
            } else {
                values_[name] = "";
            }
        }
    }

    std::optional<std::string> get(const std::string& name, const char* env = nullptr) {
        used_.insert(name);
        if (auto found = values_.find(name); found != values_.end()) return found->second;
        if (env)
            if (const char* value = std::getenv(env)) return std::string(value);
        return std::nullopt;
    }
    std::string text(const std::string& name, const char* env, const std::string& fallback) {
        return get(name, env).value_or(fallback);
    }
    /// Unset and empty are both "not given".
    std::optional<std::string> optional_text(const std::string& name, const char* env) {
        auto value = get(name, env);
        if (value && value->empty()) return std::nullopt;
        return value;
    }
    uint64_t number(const std::string& name, const char* env, uint64_t fallback, uint64_t min, uint64_t max) {
        auto value = get(name, env);
        if (!value || value->empty()) return fallback;
        char* end = nullptr;
        unsigned long long n = std::strtoull(value->c_str(), &end, 10);
        if (*end || value->front() == '-' || n < min || n > max)
            throw UsageError("--" + name + " must be a number in " + std::to_string(min) + ".." + std::to_string(max));
        return n;
    }
    const std::vector<std::string>& positional() const { return positional_; }
    void reject_unknown() const {
        for (auto& [name, _] : values_)
            if (!used_.count(name)) throw UsageError("unknown option --" + name);
    }

private:
    std::map<std::string, std::string> values_;
    std::vector<std::string> positional_;
    std::set<std::string> used_;
};

struct Common {
    std::string url, chrome;
    std::optional<std::string> cdp_url, profile;
    uint64_t turnstile_timeout_s, connections, network_probes;

    explicit Common(Args& a)
        : url(a.text("url", "QUIZ_SC_URL", "https://superchallenge.io/play/SUPERCHALLENGE-JAWVUX")),
          chrome(a.text("chrome", "QUIZ_SC_CHROME", "chromium")),
          cdp_url(a.optional_text("cdp-url", "QUIZ_SC_CDP_URL")),
          profile(a.optional_text("profile", "QUIZ_SC_PROFILE")),
          turnstile_timeout_s(a.number("turnstile-timeout-s", "QUIZ_SC_TURNSTILE_TIMEOUT_S", 60, 1, 3600)),
          connections(a.number("connections", "QUIZ_SC_CONNECTIONS", 2, 2, 16)),
          network_probes(a.number("network-probes", "QUIZ_SC_NETWORK_PROBES", 3, 0, 10)) {}
};

struct RaceArgs {
    Common common;
    std::optional<std::string> email, nickname, turnstile_token;
    std::string locale, runs_dir;
    uint64_t hedge_ms, max_requests;

    explicit RaceArgs(Args& a)
        : common(a),
          email(a.optional_text("email", "QUIZ_SC_EMAIL")),
          nickname(a.optional_text("nickname", "QUIZ_SC_NICKNAME")),
          turnstile_token(a.optional_text("turnstile-token", "QUIZ_SC_TURNSTILE_TOKEN")),
          locale(a.text("locale", "QUIZ_SC_LOCALE", "fr")),
          runs_dir(a.text("runs-dir", "QUIZ_SC_RUNS_DIR", "runs")),
          hedge_ms(a.number("hedge-ms", "QUIZ_SC_HEDGE_MS", 150, 0, 60000)),
          max_requests(a.number("max-requests", "QUIZ_SC_MAX_REQUESTS", 4, 0, 64)) {}
};

browser::Credentials credentials(const Common& common) {
    auto started = Clock::now();
    auto c = browser::credentials(common.chrome, common.cdp_url, common.profile, common.url,
                                  std::chrono::seconds(common.turnstile_timeout_s));
    say(format("turnstile token in %.1fs (%zu chars, %zu cookie bytes)",
               std::chrono::duration<double>(Clock::now() - started).count(), c.turnstile_token.size(),
               c.cookie.size()));
    return c;
}

void report_competition(const json& competition) {
    auto field = [&](const char* key) {
        return competition.is_object() && competition.contains(key) ? competition[key] : json();
    };
    json title = field("title");
    say((title.is_string() ? title.get<std::string>() : "?") + " | plays left: " + field("playsLeft").dump() +
        " | agentWars: " + field("agentWars").dump());
}

void warm_solvers() {
    auto started = Clock::now();
    solvers::warm();
    say(format("solvers warmed in %.2f ms", ms(Clock::now() - started)));
}

/// Fresh connections, timed step by step: DNS, TCP, TLS, then the response.
void network_bench(const std::string& code, size_t rounds) {
    if (rounds == 0) {
        say("network setup diagnostics disabled (--network-probes 0)");
        return;
    }
    say(format("network setup diagnostics: %zu fresh connections; separate from the racer's pools", rounds));
    say("TCP/TLS setup is not endpoint RTT. TTFB and post-setup wait include network and server work.");
    std::vector<double> connects;
    for (size_t i = 0; i < rounds; ++i) {
        // Its own pool: a new connection every time (TLS may resume a session).
        rpc::Rpc probe(USER_AGENT, "");
        auto exchange = probe.start("getCompetition", rpc::envelope({{"productId", rpc::PRODUCT_ID}, {"code", code}}));
        http::wait(exchange);
        if (!exchange->ok()) throw std::runtime_error("network diagnostic failed: " + exchange->error());
        auto& r = exchange->response();
        auto& at = r.at;
        double dns = ms(at.resolved - at.start), tcp = ms(at.connected - at.resolved);
        double tls = ms(at.secured - at.connected), setup = ms(at.secured - at.start);
        connects.push_back(tcp);
        say(format("network probe %zu: DNS %.2f ms | TCP connect %.2f ms | TLS %.2f ms | setup %.2f ms", i + 1,
                   dns, tcp, tls, setup));
        say(format("  getCompetition: TTFB %.2f ms | post-setup wait %.2f ms | total %.2f ms",
                   ms(at.headers - at.start), ms(at.headers - at.secured), ms(at.done - at.start)));
        auto header = [&](const char* name) { return r.header(name) ? *r.header(name) : std::string("None"); };
        say(format("  %s peer=%s status=%d x-vercel-id=%s server-timing=%s", r.version.c_str(), r.peer.c_str(),
                   r.status, header("x-vercel-id").c_str(), header("server-timing").c_str()));
        if (r.status < 200 || r.status >= 300)
            throw std::runtime_error(format("network diagnostic returned HTTP %d; stopping before further probes", r.status));
    }
    std::sort(connects.begin(), connects.end());
    double median = (connects[(connects.size() - 1) / 2] + connects[connects.size() / 2]) / 2.0;
    say(format("TCP connect summary: min %.2f p50 %.2f max %.2f ms (DNS/TLS excluded)", connects.front(), median,
               connects.back()));
}

void bench(const Common& common, size_t rounds) {
    std::string code = race::competition_code(common.url);
    network_bench(code, common.network_probes);
    race::Session session(code, USER_AGENT, "", common.connections);
    report_competition(session.warm());
    say("warm getCompetition endpoint RTT: includes server work; not network-only or submitAnswerV2 latency");
    auto all = session.bench(std::max<size_t>(rounds, 1));
    for (size_t index = 0; index < all.size(); ++index) {
        auto& timings = all[index];
        if (timings.empty()) continue;
        std::sort(timings.begin(), timings.end());
        say(format("getCompetition route %zu: min %.1f p50 %.1f p90 %.1f max %.1f ms", index, ms(timings.front()),
                   ms(timings[timings.size() / 2]), ms(timings[timings.size() * 9 / 10]), ms(timings.back())));
    }
}

void cookie_bench(const Common& common, size_t rounds) {
    auto c = credentials(common);
    std::string code = race::competition_code(common.url);
    std::string body = rpc::envelope({{"productId", rpc::PRODUCT_ID}, {"code", code}});
    rpc::Rpc with_cookie(c.user_agent, c.cookie), without_cookie(c.user_agent, "");
    auto pair = [&] {
        auto started = Clock::now();
        auto a = with_cookie.start("getCompetition", body), b = without_cookie.start("getCompetition", body);
        std::optional<Duration> da, db;
        while (!da || !db) {
            auto done = http::wait_any({a, b});
            if (done == a && !da) da = Clock::now() - started;
            if (done == b && !db) db = Clock::now() - started;
            if (a->finished() && !da) da = Clock::now() - started;
            if (b->finished() && !db) db = Clock::now() - started;
        }
        rpc::Rpc::read("getCompetition", *a);
        rpc::Rpc::read("getCompetition", *b);
        return std::pair{*da, *db};
    };
    pair();
    std::vector<Duration> with, without;
    double delta = 0;
    for (size_t i = 0; i < rounds; ++i) {
        auto [a, b] = pair();
        with.push_back(a);
        without.push_back(b);
        delta += ms(a) - ms(b);
    }
    auto stats = [](std::vector<Duration> t) {
        std::sort(t.begin(), t.end());
        Duration sum{};
        for (auto d : t) sum += d;
        return std::tuple{ms(sum / t.size()), ms(t[t.size() / 2]), ms(t[t.size() * 9 / 10])};
    };
    auto [wm, w50, w90] = stats(with);
    auto [nm, n50, n90] = stats(without);
    say(format("cookie A/B: %zu paired getCompetition calls; no attempt started", rounds));
    say(format("with cookie:    mean %.1f p50 %.1f p90 %.1f ms", wm, w50, w90));
    say(format("without cookie: mean %.1f p50 %.1f p90 %.1f ms", nm, n50, n90));
    say(format("paired mean delta (with - without): %+.1f ms", delta / double(rounds)));
}

void dry_run(const Common& common) {
    warm_solvers();
    auto c = credentials(common);
    race::Session session(race::competition_code(common.url), c.user_agent, c.cookie, 2);
    report_competition(session.warm());
    say("dry run OK: user agent " + c.user_agent);
}

void run_race(const RaceArgs& args) {
    if (!args.email) throw std::runtime_error("QUIZ_SC_EMAIL is not set");
    warm_solvers();
    std::string token, user_agent = USER_AGENT, cookie;
    if (args.turnstile_token) {
        token = *args.turnstile_token;
    } else {
        auto c = credentials(args.common);
        token = c.turnstile_token, user_agent = c.user_agent, cookie = c.cookie;
    }
    std::string code = race::competition_code(args.common.url);
    race::Session session(code, user_agent, cookie, 2);
    report_competition(session.warm());
    auto llm = Llm::from_env();
    race::Config config{code,
                        *args.email,
                        args.nickname,
                        args.locale,
                        std::chrono::milliseconds(args.hedge_ms),
                        std::max<size_t>(args.max_requests, 1),
                        args.runs_dir};
    race::race(session, config, token, llm.get());
}

const char* USAGE = R"(SuperChallenge Agents War racer (C++)

Usage: agentwars <command> [options]

Commands:
  race          Run the race. Spends one attempt.
  dry-run       Everything up to the race without starting it: Turnstile token,
                warm connections, plays left. Spends no attempt.
  bench         Network setup diagnostics and API endpoint timings. [--rounds N]
  cookie-bench  Compare warmed requests with and without Chrome's cookies. [--rounds N]
  solve PROMPT  Solve one prompt offline.

Options (each also read from its environment variable):
  --url QUIZ_SC_URL            --chrome QUIZ_SC_CHROME       --cdp-url QUIZ_SC_CDP_URL
  --profile QUIZ_SC_PROFILE    --turnstile-timeout-s QUIZ_SC_TURNSTILE_TIMEOUT_S
  --connections QUIZ_SC_CONNECTIONS (bench, 2..16)
  --network-probes QUIZ_SC_NETWORK_PROBES (bench, 0..10)
  --email QUIZ_SC_EMAIL        --nickname QUIZ_SC_NICKNAME   --locale QUIZ_SC_LOCALE
  --hedge-ms QUIZ_SC_HEDGE_MS  --max-requests QUIZ_SC_MAX_REQUESTS
  --runs-dir QUIZ_SC_RUNS_DIR  --turnstile-token QUIZ_SC_TURNSTILE_TOKEN
)";

}  // namespace

int main(int argc, char** argv) {
    std::ios::sync_with_stdio(false);
    if (argc < 2 || std::string(argv[1]) == "--help" || std::string(argv[1]) == "help") {
        std::cout << USAGE;
        return argc < 2 ? 2 : 0;
    }
    std::string command = argv[1];
    try {
        if (command == "solve") {
            // Everything after `solve` is the prompt (a `--` is skipped).
            int first = argc > 2 && std::string(argv[2]) == "--" ? 3 : 2;
            if (argc != first + 1) throw UsageError("usage: agentwars solve PROMPT");
            auto answer = solvers::solve(argv[first]);
            if (!answer) throw std::runtime_error("no exact solver for this prompt");
            std::cout << *answer << "\n";
            return 0;
        }
        Args args(argc, argv, 2);
        if (!args.positional().empty()) throw UsageError("unexpected argument " + args.positional()[0]);
        if (command == "race") {
            RaceArgs race_args(args);
            args.reject_unknown();
            run_race(race_args);
        } else if (command == "dry-run") {
            Common common(args);
            args.reject_unknown();
            dry_run(common);
        } else if (command == "bench") {
            Common common(args);
            size_t rounds = args.number("rounds", nullptr, 20, 0, 100000);
            args.reject_unknown();
            bench(common, rounds);
        } else if (command == "cookie-bench") {
            Common common(args);
            size_t rounds = args.number("rounds", nullptr, 30, 1, 100);
            args.reject_unknown();
            cookie_bench(common, rounds);
        } else {
            throw UsageError("unknown command " + command + "\n\n" + USAGE);
        }
    } catch (const UsageError& error) {
        std::cerr << "error: " << error.what() << "\n";
        return 2;
    } catch (const std::exception& error) {
        std::cerr << "Error: " << error.what() << "\n";
        return 1;
    }
    return 0;
}
