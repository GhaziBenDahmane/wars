// Headless Chrome, used for one thing only: the Cloudflare Turnstile token that
// `startRunV2` requires. Turnstile rejects stock headless Chrome (error
// 600010); with the `HeadlessChrome` user agent and the automation flag masked
// it issues a token in a few seconds without any click.
#pragma once

#include <chrono>
#include <optional>
#include <string>

namespace browser {

inline constexpr const char* TURNSTILE_SITE_KEY = "0x4AAAAAAEnsuR9S0axQ8Ifl";

struct Credentials {
    std::string turnstile_token;
    std::string user_agent;
    std::string cookie;
};

/// Launch (or attach to `cdp_url`) Chrome, open the play page and get a token.
/// A launched Chrome is killed before this returns.
Credentials credentials(const std::string& chrome, const std::optional<std::string>& cdp_url,
                        const std::optional<std::string>& profile, const std::string& play_url,
                        bool stub, std::chrono::seconds timeout);

}  // namespace browser
