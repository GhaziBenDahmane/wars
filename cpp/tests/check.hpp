// Minimal test runner: `TEST(name) { CHECK(...); }`.
#pragma once

#include <functional>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>

namespace testing {

struct Test {
    const char* name;
    std::function<void()> body;
};

inline std::vector<Test>& registry() {
    static std::vector<Test> tests;
    return tests;
}

struct Failure {
    std::string message;
};

struct Register {
    Register(const char* name, std::function<void()> body) {
        registry().push_back({name, std::move(body)});
    }
};

}  // namespace testing

#define TEST_CAT2(a, b) a##b
#define TEST_CAT(a, b) TEST_CAT2(a, b)
#define TEST(name)                                                              \
    static void name();                                                         \
    static testing::Register TEST_CAT(register_, name)(#name, name);            \
    static void name()

#define CHECK(condition)                                                                   \
    do {                                                                                   \
        if (!(condition)) {                                                                \
            std::ostringstream message_;                                                   \
            message_ << __FILE__ << ":" << __LINE__ << ": CHECK(" #condition ") failed";   \
            throw testing::Failure{message_.str()};                                        \
        }                                                                                  \
    } while (0)

#define CHECK_EQ(actual, expected)                                                         \
    do {                                                                                   \
        auto actual_ = (actual);                                                         \
        auto expected_ = (expected);                                                     \
        if (!(actual_ == expected_)) {                                                     \
            std::ostringstream message_;                                                   \
            message_ << __FILE__ << ":" << __LINE__ << ": " #actual " == " #expected       \
                     << " failed";                                                         \
            throw testing::Failure{message_.str()};                                        \
        }                                                                                  \
    } while (0)
