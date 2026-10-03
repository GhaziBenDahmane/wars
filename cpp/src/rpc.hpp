// oRPC client for superchallenge.io: `POST /api/rpc/superchallenge/<proc>` with
// `{"json": input}`, answering `{"json": output}` (non-2xx on errors).
#pragma once

#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <vector>

#include "../vendor/json.hpp"
#include "http.hpp"

namespace rpc {

using json = nlohmann::json;

inline constexpr const char* ORIGIN = "https://superchallenge.io";
inline constexpr const char* PRODUCT_ID = "superchallenge";
inline constexpr const char* RPC_PREFIX = "/api/rpc/superchallenge/";

/// A verdict from the server (e.g. 409 "question deadline passed"), as opposed
/// to a transport failure: never retried.
struct RpcError : std::runtime_error {
    std::string procedure;
    int status;
    json body;
    RpcError(std::string procedure, int status, json body);
};

struct CallTiming {
    http::Duration headers{}, body{}, total{};
    std::string version;
    std::optional<std::string> peer, vercel_id, server_timing;
};

struct HedgedResponse {
    json value;
    size_t sent = 0;
    size_t winner_route = 0;
    CallTiming timing;
};

class Rpc {
public:
    Rpc(const std::string& user_agent, const std::string& cookie);
    /// `connect_ip`: edge address to connect to instead of DNS (empty: DNS).
    Rpc(const std::string& origin, const std::string& user_agent, const std::string& cookie,
        const std::string& connect_ip = "");

    json call(const std::string& procedure, const json& input);
    /// The value and the response metadata (version, peer, routing headers).
    std::pair<json, CallTiming> inspect(const std::string& procedure, const json& input);

    /// Sends `{"json": input}` already serialized; the reply is read later.
    std::shared_ptr<http::Exchange> start(const std::string& procedure, const std::string& body,
                                          bool retry_stale = false);
    /// The `json` member of a finished exchange, or throws RpcError /
    /// TransportError.
    static json read(const std::string& procedure, http::Exchange& exchange);
    static CallTiming timing(const http::Response& response);

private:
    std::shared_ptr<http::Client> client_;
    std::string prefix_;
};

/// `{"json": input}`
std::string envelope(const json& input);

/// Wait before resending after a 429 when nothing else is in flight.
inline constexpr auto THROTTLE_PAUSE = std::chrono::milliseconds(100);

/// Hedged call: the network path sometimes stalls or drops a request, which
/// costs the 2 s deadline. The server dedupes a repeated answer (`isReplay`),
/// so a duplicate goes out on the next route whenever the in-flight ones are
/// slower than `hedge_after` (or failed), and the first response wins.
HedgedResponse hedged(std::vector<Rpc>& routes, const std::string& procedure, const json& input,
                      http::Duration hedge_after, size_t max_requests);
/// The same with the `{"json": input}` body already serialized.
HedgedResponse hedged_raw(std::vector<Rpc>& routes, const std::string& procedure,
                          const std::string& body, http::Duration hedge_after, size_t max_requests);

}  // namespace rpc
