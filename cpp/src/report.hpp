// Progress lines on stderr, plus small time helpers.
#pragma once

#include <chrono>
#include <cstdio>
#include <string>

#include "http.hpp"

inline void say(const std::string& line) {
    std::string out = line + "\n";
    std::fwrite(out.data(), 1, out.size(), stderr);
}

inline double ms(http::Duration d) {
    return std::chrono::duration<double, std::milli>(d).count();
}

/// Seconds since the Unix epoch.
inline double unix_now() {
    return std::chrono::duration<double>(std::chrono::system_clock::now().time_since_epoch()).count();
}

/// printf into a std::string.
template <typename... Args>
std::string format(const char* pattern, Args... args) {
    int size = std::snprintf(nullptr, 0, pattern, args...);
    std::string out(size_t(size > 0 ? size : 0), '\0');
    std::snprintf(out.data(), out.size() + 1, pattern, args...);
    return out;
}
