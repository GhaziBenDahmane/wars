#include "browser.hpp"

#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cstring>
#include <filesystem>
#include <random>
#include <stdexcept>
#include <thread>
#include <vector>

#include "../vendor/json.hpp"
#include "http.hpp"
#include "rpc.hpp"

namespace browser {

using nlohmann::json;
using Clock = std::chrono::steady_clock;

namespace {

constexpr const char* PROXY_NOTICE_MARKER = "notify-Notify_";

constexpr const char* TURNSTILE_JS = R"(
new Promise((resolve, reject) => {
  const render = () => {
    let box = document.getElementById('agentwars-turnstile');
    if (!box) {
      box = document.createElement('div');
      box.id = 'agentwars-turnstile';
      box.style.cssText = 'position:fixed;top:12px;left:12px;z-index:2147483647';
      document.body.appendChild(box);
    }
    box.innerHTML = '';
    window.turnstile.render(box, {
      sitekey: SITE_KEY,
      callback: (token) => resolve(token),
      'error-callback': (code) => reject(new Error('turnstile error ' + code)),
    });
  };
  setTimeout(() => reject(new Error('turnstile timeout')), TIMEOUT_MS);
  if (window.turnstile) return render();
  const script = document.createElement('script');
  script.src = 'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';
  script.onload = render;
  document.head.appendChild(script);
})
)";

std::runtime_error error(const std::string& message) { return std::runtime_error(message); }

std::string replace_all(std::string text, const std::string& from, const std::string& to) {
    for (size_t at = text.find(from); at != std::string::npos; at = text.find(from, at + to.size()))
        text.replace(at, from.size(), to);
    return text;
}

std::string base64(const unsigned char* data, size_t size) {
    static const char* table = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    std::string out;
    for (size_t i = 0; i < size; i += 3) {
        uint32_t n = uint32_t(data[i]) << 16;
        if (i + 1 < size) n |= uint32_t(data[i + 1]) << 8;
        if (i + 2 < size) n |= data[i + 2];
        out += table[(n >> 18) & 63];
        out += table[(n >> 12) & 63];
        out += i + 1 < size ? table[(n >> 6) & 63] : '=';
        out += i + 2 < size ? table[n & 63] : '=';
    }
    return out;
}

/// A blocking WebSocket client, enough for Chrome's DevTools endpoint.
class WebSocket {
public:
    WebSocket(const std::string& url, std::chrono::seconds read_timeout) {
        if (url.rfind("ws://", 0) != 0) throw error("unsupported DevTools URL: " + url);
        std::string rest = url.substr(5);
        size_t slash = rest.find('/');
        std::string authority = rest.substr(0, slash);
        std::string path = slash == std::string::npos ? "/" : rest.substr(slash);
        size_t colon = authority.rfind(':');
        std::string host = authority.substr(0, colon);
        std::string port = colon == std::string::npos ? "80" : authority.substr(colon + 1);
        if (host.size() > 2 && host.front() == '[') host = host.substr(1, host.size() - 2);

        addrinfo hints{};
        hints.ai_socktype = SOCK_STREAM;
        addrinfo* found = nullptr;
        if (getaddrinfo(host.c_str(), port.c_str(), &hints, &found) != 0 || !found)
            throw error("DevTools websocket: cannot resolve " + host);
        for (auto* a = found; a && fd_ < 0; a = a->ai_next) {
            int fd = ::socket(a->ai_family, SOCK_STREAM | SOCK_CLOEXEC, 0);
            if (fd < 0) continue;
            if (::connect(fd, a->ai_addr, a->ai_addrlen) == 0) fd_ = fd;
            else ::close(fd);
        }
        freeaddrinfo(found);
        if (fd_ < 0) throw error("DevTools websocket: cannot connect to " + authority);
        timeval tv{long(read_timeout.count()), 0};
        setsockopt(fd_, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
        int one = 1;
        setsockopt(fd_, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);

        std::array<unsigned char, 16> nonce;
        for (auto& b : nonce) b = static_cast<unsigned char>(random_());
        std::string request = "GET " + path + " HTTP/1.1\r\nHost: " + authority +
                              "\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " +
                              base64(nonce.data(), nonce.size()) + "\r\nSec-WebSocket-Version: 13\r\n\r\n";
        write_all(request.data(), request.size());
        size_t end;
        while ((end = buffer_.find("\r\n\r\n")) == std::string::npos) fill();
        if (buffer_.compare(0, 12, "HTTP/1.1 101") != 0)
            throw error("DevTools websocket refused: " + buffer_.substr(0, buffer_.find("\r\n")));
        buffer_.erase(0, end + 4);
    }
    ~WebSocket() {
        if (fd_ >= 0) ::close(fd_);
    }
    WebSocket(const WebSocket&) = delete;
    WebSocket& operator=(const WebSocket&) = delete;

    void send_text(const std::string& text) { send_frame(0x1, text); }

    /// The next text message (pings answered, binary ignored).
    std::string receive_text() {
        std::string message;
        int message_opcode = -1;
        while (true) {
            need(2);
            unsigned char b0 = buffer_[0], b1 = buffer_[1];
            bool fin = b0 & 0x80;
            int opcode = b0 & 0x0F;
            bool masked = b1 & 0x80;
            uint64_t length = b1 & 0x7F;
            size_t header = 2;
            if (length == 126) {
                need(4);
                length = (uint64_t(uint8_t(buffer_[2])) << 8) | uint8_t(buffer_[3]);
                header = 4;
            } else if (length == 127) {
                need(10);
                length = 0;
                for (int i = 0; i < 8; ++i) length = (length << 8) | uint8_t(buffer_[2 + i]);
                header = 10;
            }
            size_t mask_at = header;
            if (masked) header += 4;
            need(header + length);
            std::string payload = buffer_.substr(header, length);
            if (masked)
                for (size_t i = 0; i < payload.size(); ++i) payload[i] ^= buffer_[mask_at + i % 4];
            buffer_.erase(0, header + length);
            if (opcode == 0x8) throw error("Chrome closed the DevTools connection");
            if (opcode == 0x9) {
                send_frame(0xA, payload);
                continue;
            }
            if (opcode == 0xA) continue;
            if (opcode != 0x0) message_opcode = opcode;
            message += payload;
            if (fin) {
                if (message_opcode == 0x1) return message;
                message.clear();
                message_opcode = -1;
            }
        }
    }

private:
    void send_frame(int opcode, const std::string& payload) {
        std::string frame;
        frame += char(0x80 | opcode);
        if (payload.size() < 126) {
            frame += char(0x80 | payload.size());
        } else if (payload.size() < 65536) {
            frame += char(0x80 | 126);
            frame += char(payload.size() >> 8);
            frame += char(payload.size() & 0xFF);
        } else {
            frame += char(0x80 | 127);
            for (int i = 7; i >= 0; --i) frame += char((uint64_t(payload.size()) >> (8 * i)) & 0xFF);
        }
        unsigned char mask[4];
        for (auto& b : mask) b = static_cast<unsigned char>(random_());
        frame.append(reinterpret_cast<char*>(mask), 4);
        for (size_t i = 0; i < payload.size(); ++i) frame += char(payload[i] ^ mask[i % 4]);
        write_all(frame.data(), frame.size());
    }

    void write_all(const char* data, size_t size) {
        while (size) {
            ssize_t n = ::send(fd_, data, size, MSG_NOSIGNAL);
            if (n <= 0) throw error("DevTools websocket: write failed");
            data += n;
            size -= size_t(n);
        }
    }

    void fill() {
        char chunk[65536];
        ssize_t n = ::recv(fd_, chunk, sizeof chunk, 0);
        if (n == 0) throw error("Chrome closed the DevTools connection");
        if (n < 0) throw error(errno == EAGAIN ? "DevTools websocket: read timed out" : "DevTools websocket: read failed");
        buffer_.append(chunk, size_t(n));
    }

    void need(size_t size) {
        while (buffer_.size() < size) fill();
    }

    int fd_ = -1;
    std::string buffer_;
    std::random_device random_;
};

class Cdp {
public:
    explicit Cdp(const std::string& url, std::chrono::seconds timeout) : socket_(url, timeout) {}

    json send(const std::string& method, json params) {
        int id = ++next_id_;
        socket_.send_text(json{{"id", id}, {"method", method}, {"params", std::move(params)}}.dump());
        while (true) {
            json reply = json::parse(socket_.receive_text(), nullptr, false);
            if (!reply.is_object() || !reply.contains("id") || reply["id"] != id) continue;  // an event
            if (reply.contains("error")) throw error(method + ": " + reply["error"].dump());
            return reply.value("result", json::object());
        }
    }

    json evaluate(const std::string& expression) {
        json result = send("Runtime.evaluate",
                           {{"expression", expression}, {"awaitPromise", true}, {"returnByValue", true}});
        if (result.contains("exceptionDetails")) {
            auto& details = result["exceptionDetails"];
            std::string text = "evaluation failed";
            if (details.contains("exception") && details["exception"].value("description", json()).is_string())
                text = details["exception"]["description"];
            else if (details.value("text", json()).is_string())
                text = details["text"];
            throw error(text);
        }
        return result["result"].value("value", json());
    }

    /// Wait until the play page has loaded and stopped redirecting.
    void settle(std::chrono::milliseconds settle) {
        auto started = Clock::now();
        std::optional<Clock::time_point> stable_since;
        while (Clock::now() - started < std::chrono::seconds(30)) {
            json state;
            try {
                json value = evaluate("JSON.stringify([location.href, document.readyState])");
                if (value.is_string()) state = json::parse(value.get<std::string>(), nullptr, false);
            } catch (const std::exception&) {
            }
            if (state.is_array() && state.size() == 2 && state[0].is_string() && state[1].is_string()) {
                std::string href = state[0], ready = state[1];
                if (href.find(PROXY_NOTICE_MARKER) != std::string::npos)
                    throw error("a proxy disclaimer page is showing instead of the site");
                if (ready == "complete" && href.find("/play/") != std::string::npos) {
                    if (!stable_since) stable_since = Clock::now();
                    if (Clock::now() - *stable_since >= settle) return;
                } else {
                    stable_since.reset();
                }
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(200));
        }
        throw error("the play page did not finish loading");
    }

private:
    WebSocket socket_;
    int next_id_ = 0;
};

/// Chrome's DevTools HTTP endpoints, never through a proxy.
json devtools_get(const std::string& cdp_url, const std::string& path) {
    std::string base_path;
    http::Client::Options options;
    options.timeout = std::chrono::seconds(3);
    options.connect_timeout = std::chrono::seconds(3);
    http::Client client(http::Origin::parse(cdp_url, &base_path), "accept: */*\r\n", options);
    auto response = client.send("GET", path, "");
    json value = json::parse(response.body, nullptr, false);
    if (value.is_discarded()) throw error("DevTools answered something that is not JSON");
    return value;
}

json devtools_ready(const std::string& cdp_url, pid_t child) {
    for (int i = 0; i < 80; ++i) {
        if (child > 0) {
            int status;
            if (waitpid(child, &status, WNOHANG) == child)
                throw error("Chrome exited early with status " + std::to_string(status));
        }
        try {
            return devtools_get(cdp_url, "/json/version");
        } catch (const std::exception&) {
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(250));
    }
    throw error("Chrome did not open its DevTools port at " + cdp_url);
}

/// The user agent of this Chrome without "Headless". It has to be a command
/// line flag: a CDP override does not reach Turnstile's cross-origin iframe.
std::string user_agent(const std::string& chrome) {
    std::string command = "'" + replace_all(chrome, "'", "'\\''") + "' --version 2>/dev/null";
    FILE* pipe = popen(command.c_str(), "r");
    if (!pipe) throw error("cannot start Chrome at \"" + chrome + "\"");
    std::string version;
    char chunk[256];
    while (size_t n = fread(chunk, 1, sizeof chunk, pipe)) version.append(chunk, n);
    pclose(pipe);
    int major = -1;
    size_t at = 0;
    while (at < version.size() && major < 0) {
        size_t start = version.find_first_not_of(" \t\r\n", at);
        if (start == std::string::npos) break;
        size_t end = version.find_first_of(" \t\r\n", start);
        std::string word = version.substr(start, end == std::string::npos ? std::string::npos : end - start);
        std::string head = word.substr(0, word.find('.'));
        if (!head.empty() && head.find_first_not_of("0123456789") == std::string::npos)
            major = std::stoi(head);
        at = end == std::string::npos ? version.size() : end;
    }
    if (major < 0) throw error("unexpected `" + chrome + " --version` output: " + version);
    return "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/" +
           std::to_string(major) + ".0.0.0 Safari/537.36";
}

/// Chrome in its own process group, so killing the group takes every helper.
pid_t launch(const std::string& chrome, int port, const std::string& profile, const std::string& agent) {
    std::vector<std::string> args = {
        chrome,
        "--user-agent=" + agent,
        "--headless=new",
        "--remote-debugging-port=" + std::to_string(port),
        "--user-data-dir=" + profile,
        "--no-sandbox",  // containers rarely allow Chrome's own sandbox
        "--disable-dev-shm-usage",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--window-size=1280,900",
        "--disable-blink-features=AutomationControlled",
        "about:blank",
    };
    std::vector<char*> argv;
    for (auto& a : args) argv.push_back(a.data());
    argv.push_back(nullptr);
    pid_t pid = fork();
    if (pid < 0) throw error("cannot start Chrome at \"" + chrome + "\"");
    if (pid == 0) {
        setpgid(0, 0);
        int null = ::open("/dev/null", O_RDWR);
        dup2(null, 0);
        dup2(null, 1);
        dup2(null, 2);
        execvp(argv[0], argv.data());
        _exit(127);
    }
    setpgid(pid, pid);
    return pid;
}

/// A port nothing listens on, so racers running side by side each get
/// their own Chrome.
int free_port() {
    int fd = ::socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t length = sizeof address;
    if (fd < 0 || bind(fd, reinterpret_cast<sockaddr*>(&address), length) != 0 ||
        getsockname(fd, reinterpret_cast<sockaddr*>(&address), &length) != 0) {
        if (fd >= 0) ::close(fd);
        throw error("no free local port");
    }
    ::close(fd);
    return ntohs(address.sin_port);
}

}  // namespace

Credentials credentials(const std::string& chrome, const std::optional<std::string>& cdp_url_arg,
                        const std::optional<std::string>& profile_arg, const std::string& play_url,
                        std::chrono::seconds timeout) {
    int port = free_port();
    // A throwaway profile unless one is given (kept, e.g. with accepted notices).
    bool temporary = !profile_arg;
    std::string profile = profile_arg.value_or(
        (std::filesystem::temp_directory_path() / ("agentwars-chrome-" + std::to_string(getpid()))).string());
    std::optional<std::string> launched_agent;
    if (!cdp_url_arg) launched_agent = user_agent(chrome);
    pid_t child = launched_agent ? launch(chrome, port, profile, *launched_agent) : -1;
    std::string cdp_url = cdp_url_arg.value_or("http://127.0.0.1:" + std::to_string(port));

    auto cleanup = [&] {
        if (child <= 0) return;
        kill(-child, SIGKILL);  // free the CPU before the race starts
        int status;
        waitpid(child, &status, 0);
        child = -1;
        if (temporary) {
            std::error_code ignored;
            std::filesystem::remove_all(profile, ignored);
        }
    };
    try {
        json version = devtools_ready(cdp_url, child);
        std::string agent = launched_agent.value_or(
            replace_all(version.value("User-Agent", std::string()), "HeadlessChrome", "Chrome"));
        json targets = devtools_get(cdp_url, "/json/list");
        std::string ws_url;
        if (targets.is_array())
            for (auto& t : targets)
                if (t.value("type", "") == "page") {
                    ws_url = t.value("webSocketDebuggerUrl", "");
                    break;
                }
        if (ws_url.empty()) throw error("Chrome has no page target");
        Cdp cdp(ws_url, timeout + std::chrono::seconds(30));
        cdp.send("Network.enable", json::object());
        cdp.send("Page.navigate", {{"url", play_url}});
        cdp.settle(std::chrono::milliseconds(1500));
        std::string script = replace_all(TURNSTILE_JS, "SITE_KEY", json(TURNSTILE_SITE_KEY).dump());
        script = replace_all(script, "TIMEOUT_MS", std::to_string(timeout.count() * 1000));
        json token;
        try {
            token = cdp.evaluate(script);
        } catch (const std::exception& e) {
            throw error(std::string("Turnstile: ") + e.what());
        }
        json cookies = cdp.send("Network.getCookies", {{"urls", {std::string(rpc::ORIGIN) + "/"}}});
        std::string cookie;
        if (cookies.contains("cookies") && cookies["cookies"].is_array())
            for (auto& c : cookies["cookies"]) {
                if (!c.value("name", json()).is_string() || !c.value("value", json()).is_string()) continue;
                if (!cookie.empty()) cookie += "; ";
                cookie += c["name"].get<std::string>() + "=" + c["value"].get<std::string>();
            }
        if (!token.is_string() || token.get<std::string>().empty()) throw error("empty Turnstile token");
        Credentials result{token.get<std::string>(), agent, cookie};
        cleanup();
        return result;
    } catch (...) {
        cleanup();
        throw;
    }
}

}  // namespace browser
