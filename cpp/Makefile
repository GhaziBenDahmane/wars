CXX ?= g++
CXXFLAGS ?= -std=c++20 -O3 -march=x86-64-v2 -Wall -Wextra -pipe
LDFLAGS ?=
# OpenSSL linked statically: the binary only needs glibc on the target.
LIBS = -Wl,-Bstatic -lssl -lcrypto -Wl,-Bdynamic -static-libstdc++ -static-libgcc -ldl -lpthread

BUILD = build
SOURCES = src/text.cpp src/solvers.cpp src/http.cpp src/rpc.cpp src/llm.cpp src/browser.cpp src/race.cpp
OBJECTS = $(SOURCES:src/%.cpp=$(BUILD)/%.o)
TEST_SOURCES = tests/main.cpp tests/solvers_test.cpp tests/net_test.cpp tests/browser_test.cpp
TEST_OBJECTS = $(TEST_SOURCES:tests/%.cpp=$(BUILD)/tests/%.o)

all: $(BUILD)/agentwars

$(BUILD)/agentwars: $(OBJECTS) $(BUILD)/main.o
	$(CXX) $(CXXFLAGS) -flto=auto $(LDFLAGS) $^ $(LIBS) -o $@

$(BUILD)/tests/runner: $(OBJECTS) $(TEST_OBJECTS)
	$(CXX) $(CXXFLAGS) $(LDFLAGS) $^ $(LIBS) -o $@

$(BUILD)/solve_batch: $(OBJECTS) tools/solve_batch.cpp
	$(CXX) $(CXXFLAGS) $^ $(LIBS) -o $@

$(BUILD)/%.o: src/%.cpp src/*.hpp
	@mkdir -p $(dir $@)
	$(CXX) $(CXXFLAGS) -flto=auto -c $< -o $@

$(BUILD)/tests/%.o: tests/%.cpp tests/*.hpp src/*.hpp
	@mkdir -p $(dir $@)
	$(CXX) $(CXXFLAGS) -DAGENTWARS_CORPUS='"$(CURDIR)/../data/agentwars_prompts.jsonl"' -c $< -o $@

test: $(BUILD)/tests/runner
	$(BUILD)/tests/runner

# Compares every corpus prompt and random variants with the Rust binary.
difftest: $(BUILD)/solve_batch
	cd .. && cargo build --release
	python3 tools/difftest.py ../target/release/agentwars $(BUILD)/solve_batch 6

clean:
	rm -rf $(BUILD)

.PHONY: all test difftest clean
