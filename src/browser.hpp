#pragma once
#include "json.hpp"
#include <atomic>
#include <chrono>
#include <csignal>
#include <cstddef>
#include <memory>
#include <optional>
#include <string>
#include <vector>

namespace firepanda {
inline constexpr size_t max_links = 10000;
// The signal handler and deadline worker both access this flag. Only lock-free
// atomics are safe to use from a signal handler.
static_assert(std::atomic<bool>::is_always_lock_free);
extern std::atomic<bool> stop_requested;
struct Options {
  std::string lightpanda = "lightpanda";
  std::string socket;
  std::chrono::seconds idle_timeout{60}; // Zero keeps the managed browser warm indefinitely.
};
// Validated once at the tool boundary, then reused for each page of a crawl.
struct PageRequest {
  std::string url;
  std::vector<std::string> formats{"markdown"};
  bool only_main_content = true;
  std::chrono::milliseconds timeout{30000};
  std::chrono::milliseconds wait_for{0};
  std::string wait_for_selector;
  std::string wait_for_text;
};
class Browser {
public:
  explicit Browser(Options options);
  ~Browser();
  Browser(const Browser &) = delete;
  Browser &operator=(const Browser &) = delete;
  Json scrape(const PageRequest &request);
  std::optional<std::chrono::steady_clock::time_point> idle_deadline() const;
  void stop_if_idle();

private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};
const Json &tools();
Json execute(Browser &browser, const std::string &name, const Json &arguments);
} // namespace firepanda
