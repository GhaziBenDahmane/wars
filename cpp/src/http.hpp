// A small HTTP/1.1 client built for one job: tiny JSON requests on warm
// keep-alive connections, as fast as the kernel allows.
//
// - One `Client` = one connection pool to one origin (like a reqwest::Client),
//   so two clients give two independent connections for hedging.
// - Requests are written synchronously from `start()`: the bytes leave
//   before the call returns, with no scheduler hop.
// - A single-threaded `poll()` reactor drives every in-flight exchange.
//   Exchanges nobody waits for any more (the losers of a hedge) keep running
//   in the background and give their connection back to the pool, instead of
//   killing a warm connection.
// - TLS is OpenSSL with ALPN `http/1.1`, session resumption for new
//   connections, and certificate + hostname verification.
#pragma once

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace http {

using Clock = std::chrono::steady_clock;
using Duration = Clock::duration;

struct Origin {
    bool tls = true;
    std::string host;       // without brackets for IPv6
    uint16_t port = 443;
    std::string authority;  // the Host header
    std::string url;        // scheme://authority

    /// `http[s]://host[:port][/path]`; `path` receives the rest (default "/").
    static Origin parse(std::string_view url, std::string* path = nullptr);
};

/// When each step of an exchange happened. Connection steps are only set on a
/// fresh connection.
struct Phases {
    Clock::time_point start, resolved, connected, secured, written, headers, done;
    bool fresh = false;
};

struct Response {
    int status = 0;
    std::string version;  // "HTTP/1.1"
    std::vector<std::pair<std::string, std::string>> headers;  // lowercase names
    std::string body;
    std::string peer;  // "ip:port"
    Phases at;

    const std::string* header(std::string_view lowercase_name) const;
};

struct TransportError : std::runtime_error {
    using std::runtime_error::runtime_error;
};

struct PoolState;
struct Connection;

class Exchange {
public:
    enum class State { Connecting, Handshaking, Writing, Reading, Done, Failed };

    ~Exchange();
    bool finished() const { return state_ == State::Done || state_ == State::Failed; }
    bool ok() const { return state_ == State::Done; }
    const Response& response() const { return response_; }
    Response& response() { return response_; }
    const std::string& error() const { return error_; }

    // Reactor interface.
    int fd() const;
    short wanted() const { return want_; }
    Clock::time_point deadline() const {
        bool connecting = state_ == State::Connecting || state_ == State::Handshaking;
        return connecting ? std::min(deadline_, connect_deadline_) : deadline_;
    }
    void on_ready();
    void expire();

private:
    friend class Client;
    Exchange() = default;
    void advance();
    bool open_socket();
    void connected();
    void reconnect_or_fail(std::string message);
    void fail(std::string message);
    void finish(bool reusable);
    bool parse();
    bool io_wait(int ssl_error);

    std::shared_ptr<PoolState> pool_;
    std::unique_ptr<Connection> conn_;
    State state_ = State::Connecting;
    std::string request_;
    size_t written_ = 0;
    std::string buffer_;
    size_t head_end_ = 0;
    enum class Body { Unknown, None, Length, Chunked, Close } body_ = Body::Unknown;
    size_t content_length_ = 0;
    bool keep_alive_ = true;
    bool reused_ = false;
    bool retry_stale_ = false;
    size_t address_ = 0;
    short want_ = 0;
    Clock::time_point deadline_, connect_deadline_;
    Response response_;
    std::string error_;
};

class Client {
public:
    struct Options {
        Duration connect_timeout = std::chrono::seconds(5);
        Duration timeout = std::chrono::seconds(10);
        size_t max_idle = 4;
        /// Numeric address to connect to instead of resolving the host
        /// (the host still goes in SNI and `Host`). Empty: DNS.
        std::string connect_ip;
    };

    /// `headers`: extra header lines sent with every request, each ending in CRLF.
    Client(Origin origin, std::string headers, Options options);
    Client(Origin origin, std::string headers) : Client(std::move(origin), std::move(headers), Options{}) {}

    /// Writes the request before returning (or starts connecting). With
    /// `retry_stale`, a reused connection that dies before answering is
    /// replaced once by a fresh one: only for requests safe to repeat.
    std::shared_ptr<Exchange> start(std::string_view method, std::string_view path,
                                    std::string_view body, std::optional<Duration> timeout = {},
                                    bool retry_stale = false);

    /// `start` and wait. Throws TransportError.
    Response send(std::string_view method, std::string_view path, std::string_view body,
                  std::optional<Duration> timeout = {}, bool retry_stale = false);

    const Origin& origin() const;
    size_t idle() const;

private:
    std::shared_ptr<PoolState> pool_;
};

/// Drive every in-flight exchange until one of `wanted` has finished (it is
/// returned) or `until` passes (nullptr).
std::shared_ptr<Exchange> wait_any(const std::vector<std::shared_ptr<Exchange>>& wanted,
                                   std::optional<Clock::time_point> until = {});
void wait(const std::shared_ptr<Exchange>& exchange);

/// Every exchange still running, waited for or not.
size_t in_flight();

/// Incremental chunked decoding: the body when `data` holds a complete one,
/// nullopt while more bytes are needed. Throws TransportError when malformed.
std::optional<std::string> decode_chunked(std::string_view data);

}  // namespace http
