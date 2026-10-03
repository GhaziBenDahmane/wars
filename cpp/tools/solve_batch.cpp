// Differential testing helper: one prompt per stdin line -> one answer per line.
#include <iostream>
#include <string>

#include "../src/solvers.hpp"

int main() {
    std::ios::sync_with_stdio(false);
    for (std::string line; std::getline(std::cin, line);) {
        auto answer = solvers::solve(line);
        std::cout << (answer ? "=" + *answer : std::string("!")) << "\n";
    }
}
