#include "http.hpp"

#include <arpa/inet.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <openssl/err.h>
#include <openssl/ssl.h>
#include <openssl/x509_vfy.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <csignal>
#include <cstring>
#include <map>
#include <thread>

namespace http {

namespace {

std::string lowercase(std::string_view s) {
    std::string out(s);
    for (char& c : out)
        if (c >= 'A' && c <= 'Z') c = char(c + 32);
    return out;
}

std::string ssl_error_text() {
    unsigned long code = ERR_get_error();
    if (!code) return errno ? std::strerror(errno) : "connection closed";
    char text[256];
    ERR_error_string_n(code, text, sizeof text);
    ERR_clear_error();
    return text;
}

/// Sessions to resume, by host: a new connection skips a full handshake.
std::map<std::string, SSL_SESSION*>& sessions() {
    static std::map<std::string, SSL_SESSION*> cache;
    return cache;
}

int remember_session(SSL* ssl, SSL_SESSION* session) {
    auto* host = static_cast<const char*>(SSL_get_app_data(ssl));
    if (!host) return 0;
    auto& slot = sessions()[host];
    if (slot) SSL_SESSION_free(slot);
    slot = session;
    return 1;  // we keep the reference
}

SSL_CTX* tls_context() {
    static SSL_CTX* context = [] {
        SSL_CTX* ctx = SSL_CTX_new(TLS_client_method());
        if (!ctx) throw TransportError("cannot create the TLS context");
        SSL_CTX_set_min_proto_version(ctx, TLS1_2_VERSION);
        SSL_CTX_set_verify(ctx, SSL_VERIFY_PEER, nullptr);
        SSL_CTX_set_default_verify_paths(ctx);
        // Like rustls: any certificate in the trust store is an anchor, even
        // an intermediate (TLS-inspecting proxies send only their leaf).
        X509_STORE_set_flags(SSL_CTX_get_cert_store(ctx), X509_V_FLAG_PARTIAL_CHAIN);
        SSL_CTX_set_options(ctx, SSL_OP_IGNORE_UNEXPECTED_EOF | SSL_OP_NO_COMPRESSION);
        static const unsigned char alpn[] = "\x08http/1.1";
        SSL_CTX_set_alpn_protos(ctx, alpn, sizeof alpn - 1);
        SSL_CTX_set_session_cache_mode(ctx, SSL_SESS_CACHE_CLIENT | SSL_SESS_CACHE_NO_INTERNAL_STORE);
        SSL_CTX_sess_set_new_cb(ctx, remember_session);
        return ctx;
    }();
    return context;
}

std::string address_text(const sockaddr* address) {
    char host[INET6_ADDRSTRLEN] = {};
    if (address->sa_family == AF_INET6) {
        auto* a = reinterpret_cast<const sockaddr_in6*>(address);
        inet_ntop(AF_INET6, &a->sin6_addr, host, sizeof host);
        return "[" + std::string(host) + "]:" + std::to_string(ntohs(a->sin6_port));
    }
    auto* a = reinterpret_cast<const sockaddr_in*>(address);
    inet_ntop(AF_INET, &a->sin_addr, host, sizeof host);
    return std::string(host) + ":" + std::to_string(ntohs(a->sin_port));
}

struct Reactor {
    std::vector<std::shared_ptr<Exchange>> active;
};

Reactor& reactor() {
    static Reactor r;
    return r;
}

}  // namespace

struct Connection {
    int fd = -1;
    SSL* ssl = nullptr;
    std::string host;  // kept alive for the session callback
    std::string peer;
    Clock::time_point idle_since;

    ~Connection() {
        if (ssl) {
            SSL_set_app_data(ssl, nullptr);
            SSL_free(ssl);
        }
        if (fd >= 0) ::close(fd);
    }

    /// An idle connection the server has not closed: nothing to read yet.
    bool alive() {
        pollfd p{fd, POLLIN, 0};
        int ready = ::poll(&p, 1, 0);
        if (ready == 0) return true;
        if (ready < 0 || (p.revents & (POLLERR | POLLHUP | POLLNVAL))) return false;
        if (!ssl) {
            char byte;
            return ::recv(fd, &byte, 1, MSG_PEEK | MSG_DONTWAIT) < 0 && errno == EAGAIN;
        }
        // Readable: either post-handshake TLS records (tickets) or a close.
        char byte;
        ERR_clear_error();
        int n = SSL_peek(ssl, &byte, 1);
        if (n > 0) return false;  // unsolicited bytes: unusable
        return SSL_get_error(ssl, n) == SSL_ERROR_WANT_READ;
    }
};

struct PoolState {
    Origin origin;
    std::string headers;
    Client::Options options;
    std::vector<std::unique_ptr<Connection>> idle;
    std::vector<sockaddr_storage> addresses;
    std::vector<socklen_t> lengths;
    Clock::time_point resolved_at;

    void resolve() {
        if (!addresses.empty() && Clock::now() - resolved_at < std::chrono::seconds(60)) return;
        addrinfo hints{};
        hints.ai_family = AF_UNSPEC;
        hints.ai_socktype = SOCK_STREAM;
        if (!options.connect_ip.empty()) hints.ai_flags = AI_NUMERICHOST;
        const std::string& host = options.connect_ip.empty() ? origin.host : options.connect_ip;
        addrinfo* found = nullptr;
        int status = ::getaddrinfo(host.c_str(), std::to_string(origin.port).c_str(), &hints, &found);
        if (status != 0)
            throw TransportError("cannot resolve " + origin.host + ": " + gai_strerror(status));
        addresses.clear();
        lengths.clear();
        for (auto* a = found; a; a = a->ai_next) {
            sockaddr_storage s{};
            std::memcpy(&s, a->ai_addr, a->ai_addrlen);
            addresses.push_back(s);
            lengths.push_back(a->ai_addrlen);
        }
        ::freeaddrinfo(found);
        resolved_at = Clock::now();
        if (addresses.empty()) throw TransportError("no address for " + origin.host);
    }
};

Origin Origin::parse(std::string_view url, std::string* path) {
    Origin o;
    if (url.substr(0, 8) == "https://") {
        url.remove_prefix(8);
    } else if (url.substr(0, 7) == "http://") {
        o.tls = false;
        o.port = 80;
        url.remove_prefix(7);
    } else {
        throw TransportError("unsupported URL: " + std::string(url));
    }
    size_t slash = url.find_first_of("/?#");
    std::string_view authority = url.substr(0, slash);
    if (path) {
        *path = slash == std::string_view::npos ? "/" : std::string(url.substr(slash));
        if (path->front() != '/') path->insert(0, "/");
    }
    o.authority = authority;
    std::string_view host = authority;
    if (host.size() && host.front() == '[') {
        size_t close = host.find(']');
        if (close == std::string_view::npos) throw TransportError("bad IPv6 host");
        if (close + 1 < host.size() && host[close + 1] == ':')
            o.port = uint16_t(std::stoi(std::string(host.substr(close + 2))));
        host = host.substr(1, close - 1);
    } else if (size_t colon = host.rfind(':'); colon != std::string_view::npos) {
        o.port = uint16_t(std::stoi(std::string(host.substr(colon + 1))));
        host = host.substr(0, colon);
    }
    o.host = host;
    if (o.host.empty()) throw TransportError("URL without a host");
    o.url = (o.tls ? "https://" : "http://") + o.authority;
    return o;
}

const std::string* Response::header(std::string_view name) const {
    for (auto& [k, v] : headers)
        if (k == name) return &v;
    return nullptr;
}

// --- Exchange ---

Exchange::~Exchange() = default;

int Exchange::fd() const { return conn_ ? conn_->fd : -1; }

void Exchange::fail(std::string message) {
    error_ = std::move(message);
    state_ = State::Failed;
    want_ = 0;
    conn_.reset();
    response_.at.done = Clock::now();
}

/// A reused connection died before any answer: with `retry_stale`, resend
/// once on a fresh connection.
void Exchange::reconnect_or_fail(std::string message) {
    if (reused_ && retry_stale_ && buffer_.empty()) {
        reused_ = false;
        retry_stale_ = false;
        conn_.reset();
        written_ = 0;
        address_ = 0;
        state_ = State::Connecting;
        connect_deadline_ = std::min(deadline_, Clock::now() + pool_->options.connect_timeout);
        try {
            pool_->resolve();
        } catch (const TransportError& e) {
            fail(e.what());
            return;
        }
        advance();
        return;
    }
    fail(std::move(message));
}

void Exchange::finish(bool reusable) {
    state_ = State::Done;
    want_ = 0;
    response_.at.done = Clock::now();
    if (reusable && keep_alive_ && conn_ && pool_->idle.size() < pool_->options.max_idle) {
        conn_->idle_since = response_.at.done;
        pool_->idle.push_back(std::move(conn_));
    }
    conn_.reset();
}

void Exchange::expire() {
    if (finished()) return;
    if (state_ == State::Connecting || state_ == State::Handshaking)
        fail(Clock::now() >= connect_deadline_ ? "connect timed out" : "timed out");
    else
        fail("timed out");
}

bool Exchange::open_socket() {
    while (address_ < pool_->addresses.size()) {
        auto& address = pool_->addresses[address_];
        socklen_t length = pool_->lengths[address_];
        ++address_;
        int fd = ::socket(address.ss_family, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if (fd < 0) continue;
        int one = 1;
        ::setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
        conn_ = std::make_unique<Connection>();
        conn_->fd = fd;
        int r = ::connect(fd, reinterpret_cast<const sockaddr*>(&address), length);
        if (r == 0) {
            connected();
            return true;
        }
        if (errno == EINPROGRESS) {
            want_ = POLLOUT;
            return true;
        }
        conn_.reset();
    }
    return false;
}

void Exchange::connected() {
    response_.at.connected = Clock::now();
    sockaddr_storage peer{};
    socklen_t length = sizeof peer;
    if (::getpeername(conn_->fd, reinterpret_cast<sockaddr*>(&peer), &length) == 0)
        conn_->peer = address_text(reinterpret_cast<sockaddr*>(&peer));
    if (!pool_->origin.tls) {
        state_ = State::Writing;
        return;
    }
    SSL* ssl = SSL_new(tls_context());
    conn_->ssl = ssl;
    conn_->host = pool_->origin.host;
    SSL_set_fd(ssl, conn_->fd);
    SSL_set_tlsext_host_name(ssl, conn_->host.c_str());
    SSL_set1_host(ssl, conn_->host.c_str());
    SSL_set_app_data(ssl, conn_->host.c_str());
    if (auto found = sessions().find(conn_->host); found != sessions().end())
        SSL_set_session(ssl, found->second);
    state_ = State::Handshaking;
}

/// After an SSL call returned `ssl_error`: wait for the socket (true) or not.
bool Exchange::io_wait(int ssl_error) {
    if (ssl_error == SSL_ERROR_WANT_READ) {
        want_ = POLLIN;
        return true;
    }
    if (ssl_error == SSL_ERROR_WANT_WRITE) {
        want_ = POLLOUT;
        return true;
    }
    return false;
}

void Exchange::on_ready() {
    if (finished()) return;
    if (state_ == State::Connecting && conn_) {
        int error = 0;
        socklen_t length = sizeof error;
        ::getsockopt(conn_->fd, SOL_SOCKET, SO_ERROR, &error, &length);
        if (error) {
            conn_.reset();
            if (!open_socket())
                return fail(std::string("connect failed: ") + std::strerror(error));
            if (state_ == State::Connecting) return;  // waiting on the next address
        } else {
            connected();
        }
    }
    advance();
}

void Exchange::advance() {
    while (!finished()) {
        switch (state_) {
            case State::Connecting:
                if (!conn_) {
                    if (!open_socket()) return fail("connect failed: " + std::string(std::strerror(errno)));
                    if (state_ == State::Connecting) return;  // in progress
                    continue;
                }
                return;  // waiting for POLLOUT
            case State::Handshaking: {
                ERR_clear_error();
                int r = SSL_connect(conn_->ssl);
                if (r == 1) {
                    response_.at.secured = Clock::now();
                    state_ = State::Writing;
                    continue;
                }
                int e = SSL_get_error(conn_->ssl, r);
                if (io_wait(e)) return;
                return fail("TLS handshake failed: " + ssl_error_text());
            }
            case State::Writing: {
                while (written_ < request_.size()) {
                    const char* data = request_.data() + written_;
                    size_t left = request_.size() - written_;
                    if (conn_->ssl) {
                        ERR_clear_error();
                        int n = SSL_write(conn_->ssl, data, int(left));
                        if (n <= 0) {
                            int e = SSL_get_error(conn_->ssl, n);
                            if (io_wait(e)) return;
                            return reconnect_or_fail("write failed: " + ssl_error_text());
                        }
                        written_ += size_t(n);
                    } else {
                        ssize_t n = ::send(conn_->fd, data, left, MSG_NOSIGNAL);
                        if (n < 0) {
                            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                                want_ = POLLOUT;
                                return;
                            }
                            return reconnect_or_fail(std::string("write failed: ") + std::strerror(errno));
                        }
                        written_ += size_t(n);
                    }
                }
                response_.at.written = Clock::now();
                state_ = State::Reading;
                continue;
            }
            case State::Reading: {
                char chunk[16384];
                while (true) {
                    ssize_t n;
                    bool closed = false;
                    if (conn_->ssl) {
                        ERR_clear_error();
                        int r = SSL_read(conn_->ssl, chunk, sizeof chunk);
                        if (r > 0) {
                            n = r;
                        } else {
                            int e = SSL_get_error(conn_->ssl, r);
                            if (io_wait(e)) return;
                            if (e != SSL_ERROR_ZERO_RETURN && !(e == SSL_ERROR_SYSCALL && errno == 0))
                                return reconnect_or_fail("read failed: " + ssl_error_text());
                            closed = true;
                            n = 0;
                        }
                    } else {
                        n = ::recv(conn_->fd, chunk, sizeof chunk, 0);
                        if (n < 0) {
                            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                                want_ = POLLIN;
                                return;
                            }
                            return reconnect_or_fail(std::string("read failed: ") + std::strerror(errno));
                        }
                        closed = n == 0;
                    }
                    if (closed) {
                        if (head_end_ && body_ == Body::Close) {
                            response_.body = buffer_.substr(head_end_);
                            return finish(false);
                        }
                        return reconnect_or_fail(head_end_ ? "body lost: connection closed"
                                                           : "connection closed before a response");
                    }
                    buffer_.append(chunk, size_t(n));
                    try {
                        if (parse()) return;
                    } catch (const TransportError& e) {
                        return fail(e.what());
                    }
                }
            }
            default: return;
        }
    }
}

/// True once the exchange is finished (complete response parsed).
bool Exchange::parse() {
    while (!head_end_) {
        size_t end = buffer_.find("\r\n\r\n");
        if (end == std::string::npos) {
            if (buffer_.size() > 65536) throw TransportError("response head too large");
            return false;
        }
        std::string_view head(buffer_.data(), end);
        size_t line_end = head.find("\r\n");
        std::string_view status_line = head.substr(0, line_end);
        if (status_line.substr(0, 5) != "HTTP/" || status_line.size() < 12)
            throw TransportError("malformed status line");
        int status = std::atoi(std::string(status_line.substr(9, 3)).c_str());
        if (status >= 100 && status < 200 && status != 101) {  // interim: skip
            buffer_.erase(0, end + 4);
            continue;
        }
        response_.status = status;
        response_.version = std::string(status_line.substr(0, 8));
        response_.headers.clear();
        while (line_end != std::string_view::npos) {
            size_t start = line_end + 2;
            line_end = head.find("\r\n", start);
            std::string_view line = head.substr(start, line_end == std::string_view::npos ? std::string_view::npos : line_end - start);
            size_t colon = line.find(':');
            if (colon == std::string_view::npos) continue;
            std::string_view value = line.substr(colon + 1);
            while (!value.empty() && (value.front() == ' ' || value.front() == '\t')) value.remove_prefix(1);
            while (!value.empty() && (value.back() == ' ' || value.back() == '\t')) value.remove_suffix(1);
            response_.headers.emplace_back(lowercase(line.substr(0, colon)), std::string(value));
        }
        head_end_ = end + 4;
        response_.at.headers = Clock::now();
        response_.peer = conn_->peer;
        const std::string* connection = response_.header("connection");
        std::string connection_value = connection ? lowercase(*connection) : "";
        keep_alive_ = response_.version == "HTTP/1.1" ? connection_value.find("close") == std::string::npos
                                                      : connection_value.find("keep-alive") != std::string::npos;
        const std::string* encoding = response_.header("transfer-encoding");
        const std::string* length = response_.header("content-length");
        bool head_request = request_.compare(0, 5, "HEAD ") == 0;
        if (head_request || status == 204 || status == 304) {
            body_ = Body::None;
        } else if (encoding && lowercase(*encoding).find("chunked") != std::string::npos) {
            body_ = Body::Chunked;
        } else if (length) {
            body_ = Body::Length;
            content_length_ = std::strtoull(length->c_str(), nullptr, 10);
        } else {
            body_ = Body::Close;
            keep_alive_ = false;
        }
    }
    std::string_view body(buffer_.data() + head_end_, buffer_.size() - head_end_);
    switch (body_) {
        case Body::None:
            finish(body.empty());
            return true;
        case Body::Length:
            if (body.size() < content_length_) return false;
            response_.body = std::string(body.substr(0, content_length_));
            finish(body.size() == content_length_);
            return true;
        case Body::Chunked:
            if (auto decoded = decode_chunked(body)) {
                response_.body = std::move(*decoded);
                finish(true);
                return true;
            }
            return false;
        default:
            return false;
    }
}

std::optional<std::string> decode_chunked(std::string_view data) {
    std::string out;
    size_t at = 0;
    while (true) {
        size_t line_end = data.find("\r\n", at);
        if (line_end == std::string_view::npos) return std::nullopt;
        std::string_view size_text = data.substr(at, line_end - at);
        size_t semicolon = size_text.find(';');
        if (semicolon != std::string_view::npos) size_text = size_text.substr(0, semicolon);
        while (!size_text.empty() && (size_text.back() == ' ' || size_text.back() == '\t')) size_text.remove_suffix(1);
        if (size_text.empty()) throw TransportError("malformed chunk size");
        size_t size = 0;
        for (char c : size_text) {
            int digit = c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1;
            if (digit < 0 || size > (SIZE_MAX >> 4)) throw TransportError("malformed chunk size");
            size = size * 16 + size_t(digit);
        }
        at = line_end + 2;
        if (size == 0) {
            // Trailers end with an empty line.
            while (true) {
                size_t end = data.find("\r\n", at);
                if (end == std::string_view::npos) return std::nullopt;
                if (end == at) return out;
                at = end + 2;
            }
        }
        if (data.size() < at + size + 2) return std::nullopt;
        out.append(data.substr(at, size));
        if (data.substr(at + size, 2) != "\r\n") throw TransportError("malformed chunk");
        at += size + 2;
    }
}

// --- Client ---

Client::Client(Origin origin, std::string headers, Options options)
    : pool_(std::make_shared<PoolState>()) {
    // A write to a connection the server closed must fail, not kill the process.
    static const bool ignore_sigpipe = (std::signal(SIGPIPE, SIG_IGN), true);
    (void)ignore_sigpipe;
    pool_->origin = std::move(origin);
    pool_->headers = std::move(headers);
    pool_->options = options;
}

const Origin& Client::origin() const { return pool_->origin; }
size_t Client::idle() const { return pool_->idle.size(); }

std::shared_ptr<Exchange> Client::start(std::string_view method, std::string_view path,
                                        std::string_view body, std::optional<Duration> timeout,
                                        bool retry_stale) {
    std::shared_ptr<Exchange> ex(new Exchange());
    ex->pool_ = pool_;
    ex->retry_stale_ = retry_stale;
    auto now = Clock::now();
    ex->response_.at.start = now;
    ex->deadline_ = now + timeout.value_or(pool_->options.timeout);
    ex->connect_deadline_ = now + pool_->options.connect_timeout;

    std::string& r = ex->request_;
    r.reserve(64 + path.size() + pool_->origin.authority.size() + pool_->headers.size() + body.size());
    r.append(method).append(" ").append(path).append(" HTTP/1.1\r\nhost: ");
    r.append(pool_->origin.authority).append("\r\n").append(pool_->headers);
    if (!body.empty() || method == "POST" || method == "PUT")
        r.append("content-length: ").append(std::to_string(body.size())).append("\r\n");
    r.append("\r\n").append(body);

    // Most recently used first: the warmest connection.
    while (!pool_->idle.empty()) {
        auto conn = std::move(pool_->idle.back());
        pool_->idle.pop_back();
        if (conn->alive()) {
            ex->conn_ = std::move(conn);
            ex->reused_ = true;
            ex->state_ = Exchange::State::Writing;
            break;
        }
    }
    if (!ex->conn_) {
        ex->response_.at.fresh = true;
        try {
            pool_->resolve();
        } catch (const TransportError& e) {
            ex->fail(e.what());
            return ex;
        }
        ex->response_.at.resolved = Clock::now();
        ex->connect_deadline_ = std::min(ex->connect_deadline_, ex->deadline_);
    }
    ex->advance();
    if (!ex->finished()) reactor().active.push_back(ex);
    return ex;
}

Response Client::send(std::string_view method, std::string_view path, std::string_view body,
                      std::optional<Duration> timeout, bool retry_stale) {
    auto ex = start(method, path, body, timeout, retry_stale);
    wait(ex);
    if (!ex->ok()) throw TransportError(ex->error());
    return std::move(ex->response());
}

// --- Reactor ---

size_t in_flight() { return reactor().active.size(); }

std::shared_ptr<Exchange> wait_any(const std::vector<std::shared_ptr<Exchange>>& wanted,
                                   std::optional<Clock::time_point> until) {
    auto& active = reactor().active;
    std::vector<pollfd> fds;
    std::vector<Exchange*> owners;
    while (true) {
        for (auto& ex : wanted)
            if (ex->finished()) return ex;
        auto now = Clock::now();
        if (until && now >= *until) return nullptr;
        // Expire, then collect what to poll.
        Clock::time_point next = until.value_or(now + std::chrono::hours(1));
        fds.clear();
        owners.clear();
        for (auto& ex : active) {
            if (ex->finished()) continue;
            auto limit = ex->deadline();
            if (now >= limit) {
                ex->expire();
                continue;
            }
            next = std::min(next, limit);
            if (ex->fd() >= 0 && ex->wanted()) {
                fds.push_back(pollfd{ex->fd(), ex->wanted(), 0});
                owners.push_back(ex.get());
            }
        }
        std::erase_if(active, [](auto& ex) { return ex->finished(); });
        bool done = false;
        for (auto& ex : wanted) done = done || ex->finished();
        if (done) continue;
        if (fds.empty() && active.empty()) {
            // Nothing in flight: a plain sleep.
            if (until) std::this_thread::sleep_until(*until);
            return nullptr;
        }
        auto wait = std::chrono::duration_cast<std::chrono::microseconds>(next - now).count();
        timespec ts{time_t(wait / 1000000), long(wait % 1000000) * 1000};
        int ready = ::ppoll(fds.data(), fds.size(), &ts, nullptr);
        if (ready < 0 && errno != EINTR) throw TransportError(std::string("poll: ") + std::strerror(errno));
        for (size_t i = 0; ready > 0 && i < fds.size(); ++i)
            if (fds[i].revents) owners[i]->on_ready();
        std::erase_if(active, [](auto& ex) { return ex->finished(); });
    }
}

void wait(const std::shared_ptr<Exchange>& exchange) {
    while (!exchange->finished()) wait_any({exchange});
}

}  // namespace http
