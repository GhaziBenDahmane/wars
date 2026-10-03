#include <cstring>
#include <iostream>

#include "check.hpp"

int main(int argc, char** argv) {
    const char* filter = argc > 1 ? argv[1] : nullptr;
    size_t passed = 0, failed = 0;
    for (auto& test : testing::registry()) {
        if (filter && !std::strstr(test.name, filter)) continue;
        try {
            test.body();
            ++passed;
            std::cout << "ok   " << test.name << "\n";
        } catch (const testing::Failure& failure) {
            ++failed;
            std::cout << "FAIL " << test.name << "\n  " << failure.message << "\n";
        } catch (const std::exception& error) {
            ++failed;
            std::cout << "FAIL " << test.name << "\n  exception: " << error.what() << "\n";
        }
    }
    std::cout << passed << " passed, " << failed << " failed\n";
    return failed ? 1 : 0;
}
