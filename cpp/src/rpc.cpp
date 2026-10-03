#include "rpc.hpp"

#include <exception>

namespace rpc {

namespace {

std::string dump(const json& value) {
    return value.dump(-1, ' ', false, json::error_handler_t::replace);
}

std::string describe(const std::string& procedure, int status, const json& body) {
    auto text = [&](const char* key) -> std::string {
        if (body.is_object() && body.contains(key) && body[key].is_string()) return body[key];
        return "";
    };
    std::string data = body.is_object() && body.contains("data") ? dump(body["data"]) : "null";
    return procedure + " failed (" + std::to_string(status) + " " + text("code") + "): " +
           text("message") + " data=" + data;
}

}  // namespace

RpcError::RpcError(std::string procedure_, int status_, json body_)
    : std::runtime_error(describe(procedure_, status_, body_)),
      procedure(std::move(procedure_)),
      status(status_),
      body(std::move(body_)) {}

std::string envelope(const json& input) { return dump(json{{"json", input}}); }

Rpc::Rpc(const std::string& user_agent, const std::string& cookie) : Rpc(ORIGIN, user_agent, cookie) {}

Rpc::Rpc(const std::string& origin, const std::string& user_agent, const std::string& cookie,
         const std::string& connect_ip) {
    std::string headers = "content-type: application/json\r\n";
    headers += std::string("x-product-id: ") + PRODUCT_ID + "\r\n";
    headers += std::string("origin: ") + ORIGIN + "\r\n";
    headers += std::string("referer: ") + ORIGIN + "/\r\n";
    if (!cookie.empty()) headers += "cookie: " + cookie + "\r\n";
    headers += "user-agent: " + user_agent + "\r\n";
    headers += "accept: */*\r\n";
    http::Client::Options options;
    options.connect_ip = connect_ip;
    client_ = std::make_shared<http::Client>(http::Origin::parse(origin), std::move(headers), options);
    prefix_ = RPC_PREFIX;
}

std::shared_ptr<http::Exchange> Rpc::start(const std::string& procedure, const std::string& body,
                                           bool retry_stale) {
    return client_->start("POST", prefix_ + procedure, body, std::nullopt, retry_stale);
}

json Rpc::read(const std::string& procedure, http::Exchange& exchange) {
    if (!exchange.ok()) throw http::TransportError(procedure + ": request failed: " + exchange.error());
    auto& response = exchange.response();
    json parsed = json::parse(response.body, nullptr, false);
    if (parsed.is_discarded()) parsed = nullptr;
    json value = parsed.is_object() && parsed.contains("json") ? std::move(parsed["json"]) : std::move(parsed);
    if (response.status < 200 || response.status >= 300)
        throw RpcError(procedure, response.status, std::move(value));
    return value;
}

CallTiming Rpc::timing(const http::Response& response) {
    CallTiming t;
    t.headers = response.at.headers - response.at.start;
    t.body = response.at.done - response.at.headers;
    t.total = response.at.done - response.at.start;
    t.version = response.version;
    if (!response.peer.empty()) t.peer = response.peer;
    if (auto* v = response.header("x-vercel-id")) t.vercel_id = *v;
    if (auto* v = response.header("server-timing")) t.server_timing = *v;
    return t;
}

json Rpc::call(const std::string& procedure, const json& input) {
    auto exchange = start(procedure, envelope(input), procedure == "getCompetition");
    http::wait(exchange);
    return read(procedure, *exchange);
}

std::pair<json, CallTiming> Rpc::inspect(const std::string& procedure, const json& input) {
    auto exchange = start(procedure, envelope(input), procedure == "getCompetition");
    http::wait(exchange);
    json value = read(procedure, *exchange);
    return {std::move(value), timing(exchange->response())};
}

HedgedResponse hedged(std::vector<Rpc>& routes, const std::string& procedure, const json& input,
                      http::Duration hedge_after, size_t max_requests) {
    return hedged_raw(routes, procedure, envelope(input), hedge_after, max_requests);
}

HedgedResponse hedged_raw(std::vector<Rpc>& routes, const std::string& procedure,
                          const std::string& body, http::Duration hedge_after, size_t max_requests) {
    struct Flight {
        size_t route;
        std::shared_ptr<http::Exchange> exchange;
    };
    std::vector<Flight> in_flight;
    std::vector<std::shared_ptr<http::Exchange>> waiting;
    size_t sent = 0;
    std::exception_ptr last_error;
    // Set by a 429: no more duplicates, and the requests still in flight decide.
    bool throttled = false;
    while (true) {
        if (sent < max_requests && (!throttled || in_flight.empty())) {
            if (throttled) http::wait_any({}, http::Clock::now() + THROTTLE_PAUSE);
            size_t route = sent % routes.size();
            in_flight.push_back({route, routes[route].start(procedure, body)});
            ++sent;
        } else if (in_flight.empty()) {
            if (last_error) std::rethrow_exception(last_error);
            throw http::TransportError(procedure + ": no route answered");
        }
        waiting.clear();
        for (auto& f : in_flight) waiting.push_back(f.exchange);
        std::shared_ptr<http::Exchange> next;
        if (sent < max_requests && !throttled) {
            next = http::wait_any(waiting, http::Clock::now() + hedge_after);
            if (!next) continue;  // slow: send a duplicate
        } else {
            next = http::wait_any(waiting);
            if (!next) continue;
        }
        size_t route = 0;
        for (size_t i = 0; i < in_flight.size(); ++i)
            if (in_flight[i].exchange == next) {
                route = in_flight[i].route;
                in_flight.erase(in_flight.begin() + long(i));
                break;
            }
        try {
            json value = Rpc::read(procedure, *next);
            return HedgedResponse{std::move(value), sent, route, Rpc::timing(next->response())};
        } catch (const RpcError& error) {
            if (error.status != 429) throw;
            throttled = true;
            last_error = std::current_exception();
        } catch (const http::TransportError&) {
            last_error = std::current_exception();  // transport failure: next route now
        }
    }
}

}  // namespace rpc
