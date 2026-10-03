// The DevTools client against a fake Chrome: HTTP endpoints, then a WebSocket
// that answers CDP calls with events, pings and fragments in between.
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>

#include <atomic>
#include <thread>

#include "../src/browser.hpp"
#include "../vendor/json.hpp"
#include "check.hpp"

using json = nlohmann::json;

namespace {

class FakeChrome {
public:
    FakeChrome() {
        listen_ = ::socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
        int one = 1;
        setsockopt(listen_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        socklen_t length = sizeof address;
        bind(listen_, reinterpret_cast<sockaddr*>(&address), length);
        getsockname(listen_, reinterpret_cast<sockaddr*>(&address), &length);
        port_ = ntohs(address.sin_port);
        listen(listen_, 16);
        thread_ = std::thread([this] { run(); });
    }
    ~FakeChrome() {
        ::shutdown(listen_, SHUT_RDWR);
        ::close(listen_);
        thread_.join();
    }
    std::string url() const { return "http://127.0.0.1:" + std::to_string(port_); }
    std::vector<std::string> methods;

private:
    static bool read_until(int fd, std::string& buffer, const std::string& marker) {
        char chunk[4096];
        while (buffer.find(marker) == std::string::npos) {
            ssize_t n = ::recv(fd, chunk, sizeof chunk, 0);
            if (n <= 0) return false;
            buffer.append(chunk, size_t(n));
        }
        return true;
    }
    static void send_all(int fd, const std::string& data) { ::send(fd, data.data(), data.size(), MSG_NOSIGNAL); }

    static std::string frame(int opcode, const std::string& payload, bool fin = true) {
        std::string out;
        out += char((fin ? 0x80 : 0) | opcode);
        if (payload.size() < 126) {
            out += char(payload.size());
        } else if (payload.size() < 65536) {
            out += char(126);
            out += char(payload.size() >> 8);
            out += char(payload.size() & 0xFF);
        } else {
            out += char(127);
            for (int i = 7; i >= 0; --i) out += char((uint64_t(payload.size()) >> (8 * i)) & 0xFF);
        }
        return out + payload;
    }

    /// One masked client frame.
    static bool receive(int fd, std::string& buffer, std::string& payload) {
        char chunk[4096];
        auto need = [&](size_t n) {
            while (buffer.size() < n) {
                ssize_t got = ::recv(fd, chunk, sizeof chunk, 0);
                if (got <= 0) return false;
                buffer.append(chunk, size_t(got));
            }
            return true;
        };
        if (!need(2)) return false;
        uint64_t length = uint8_t(buffer[1]) & 0x7F;
        size_t header = 2;
        if (length == 126) {
            if (!need(4)) return false;
            length = (uint64_t(uint8_t(buffer[2])) << 8) | uint8_t(buffer[3]);
            header = 4;
        }
        if (!need(header + 4 + length)) return false;
        std::string mask = buffer.substr(header, 4);
        payload = buffer.substr(header + 4, length);
        for (size_t i = 0; i < payload.size(); ++i) payload[i] ^= mask[i % 4];
        buffer.erase(0, header + 4 + length);
        return true;
    }

    void run() {
        while (true) {
            int fd = ::accept(listen_, nullptr, nullptr);
            if (fd < 0) return;
            std::string buffer;
            if (!read_until(fd, buffer, "\r\n\r\n")) {
                ::close(fd);
                continue;
            }
            std::string first = buffer.substr(0, buffer.find("\r\n"));
            if (first.find("/json/version") != std::string::npos) {
                std::string body = R"({"User-Agent":"Mozilla/5.0 HeadlessChrome/149.0.0.0"})";
                send_all(fd, "HTTP/1.1 200 OK\r\ncontent-length: " + std::to_string(body.size()) + "\r\n\r\n" + body);
            } else if (first.find("/json/list") != std::string::npos) {
                std::string body = R"([{"type":"service_worker"},{"type":"page","webSocketDebuggerUrl":"ws://127.0.0.1:)" +
                                   std::to_string(port_) + R"(/devtools/page/1"}])";
                send_all(fd, "HTTP/1.1 200 OK\r\ncontent-length: " + std::to_string(body.size()) + "\r\n\r\n" + body);
            } else if (first.find("/devtools/page/1") != std::string::npos) {
                send_all(fd, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
                buffer.erase(0, buffer.find("\r\n\r\n") + 4);
                websocket(fd, buffer);
            }
            ::close(fd);
        }
    }

    void websocket(int fd, std::string& buffer) {
        std::string payload;
        while (receive(fd, buffer, payload)) {
            json call = json::parse(payload, nullptr, false);
            if (!call.is_object()) continue;  // a pong
            std::string method = call["method"];
            methods.push_back(method);
            if (method == "Page.navigate" && paused)
                send_all(fd, frame(0x1, json{{"method", "Fetch.requestPaused"}, {"params", {{"requestId", "r1"}}}}.dump()));
            if (method == "Fetch.fulfillRequest") fulfilled = call["params"];
            json result = json::object();
            if (method == "Runtime.evaluate") {
                std::string expression = call["params"]["expression"];
                if (expression.find("location.href") != std::string::npos)
                    result = {{"result", {{"type", "string"}, {"value", R"(["https://x/play/SC-ABC","complete"])"}}}};
                else
                    result = {{"result", {{"type", "string"}, {"value", "turnstile-token"}}}};
            } else if (method == "Network.getCookies") {
                result = {{"cookies", {{{"name", "a"}, {"value", "1"}}, {{"name", "b"}, {"value", "2"}}}}};
            }
            // An event and a ping first, then the reply in two fragments.
            send_all(fd, frame(0x1, json{{"method", "Network.dataReceived"}, {"params", {{"pad", std::string(300, 'x')}}}}.dump()));
            send_all(fd, frame(0x9, "ping"));
            std::string reply = json{{"id", call["id"]}, {"result", result}}.dump();
            send_all(fd, frame(0x1, reply.substr(0, 5), false) + frame(0x0, reply.substr(5)));
        }
    }

public:
    /// Pause the navigation like `Fetch.enable` does.
    bool paused = false;
    json fulfilled;

private:
    int listen_ = -1;
    uint16_t port_ = 0;
    std::thread thread_;
};

}  // namespace

TEST(credentials_come_from_an_attached_chrome) {
    FakeChrome chrome;
    auto c = browser::credentials("unused", chrome.url(), std::nullopt, "https://x/play/SC-ABC", false,
                                  std::chrono::seconds(5));
    CHECK_EQ(c.turnstile_token, "turnstile-token");
    CHECK_EQ(c.user_agent, "Mozilla/5.0 Chrome/149.0.0.0");
    CHECK_EQ(c.cookie, "a=1; b=2");
    CHECK_EQ(chrome.methods.front(), "Network.enable");
    CHECK_EQ(chrome.methods[1], "Page.navigate");
    CHECK_EQ(chrome.methods.back(), "Network.getCookies");
}

TEST(the_stub_page_answers_the_paused_play_page) {
    FakeChrome chrome;
    chrome.paused = true;
    auto c = browser::credentials("unused", chrome.url(), std::nullopt, "https://x/play/SC-ABC", true,
                                  std::chrono::seconds(5));
    CHECK_EQ(c.turnstile_token, "turnstile-token");
    CHECK_EQ(chrome.methods[1], "Fetch.enable");
    CHECK_EQ(chrome.methods[2], "Page.navigate");
    CHECK_EQ(chrome.methods[3], "Fetch.fulfillRequest");
    CHECK_EQ(chrome.fulfilled["requestId"], "r1");
    CHECK_EQ(chrome.fulfilled["responseCode"], 200);
}
