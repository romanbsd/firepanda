#pragma once
#include "json.hpp"
#include <atomic>
#include <chrono>
#include <csignal>
#include <cstddef>
#include <future>
#include <memory>
#include <optional>
#include <string>
#include <vector>

namespace firepanda {
inline constexpr size_t max_links = 10000;
// The signal handler, deadline worker, and request loops share this flag. Only
// lock-free atomics are safe to use from a signal handler.
static_assert(std::atomic<bool>::is_always_lock_free);
extern std::atomic<bool> stop_requested;
// Self-pipe shared by the signal handler and deadline workers; create it before
// installing signal handlers, then let workers poll read end [0].
extern int stop_wakeup[2];
bool init_stop_wakeup() noexcept;
void wake_stop_waiters() noexcept;
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
  // Fetch one page on a worker thread over a pooled CDP connection. The main
  // thread connects first (launching the browser if needed), so only one
  // thread ever spawns the process; failures surface through the returned
  // future without releasing the browser, which the crawl loop reaps after
  // draining sibling pages.
  std::future<Json> scrape_async(PageRequest request);
  // Record completed page work for the idle deadline. Main thread only.
  void page_completed();
  // Reap the managed browser and every pooled connection; the next operation
  // cold-starts a new child. Only call when no page worker is running.
  void restart();
  std::optional<std::chrono::steady_clock::time_point> idle_deadline() const;
  void stop_if_idle();

private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};
const Json &tools();
Json execute(Browser &browser, const std::string &name, const Json &arguments);
} // namespace firepanda
