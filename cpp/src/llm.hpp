// Optional last-resort fallback for a prompt no exact solver understands.
// Configured only through the environment; any OpenAI-compatible
// `chat/completions` endpoint works. Unset = unknown prompts answer "?".
#pragma once

#include <memory>
#include <optional>
#include <string>

#include "http.hpp"

class Llm {
public:
    /// `QUIZ_LLM_URL` (full chat/completions URL), `QUIZ_LLM_API_KEY`,
    /// `QUIZ_LLM_MODEL`. No proxy is used.
    static std::unique_ptr<Llm> from_env();

    /// The answer, or nullopt on any failure or after `timeout`.
    std::optional<std::string> answer(const std::string& prompt, http::Duration timeout);

private:
    std::unique_ptr<http::Client> client_;
    std::string path_;
    std::string model_;
};
