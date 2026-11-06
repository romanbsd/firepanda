#include "browser.hpp"
#include "extraction.hpp"
#include <algorithm>
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <csignal>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <fcntl.h>
#include <filesystem>
#include <httplib.h>
#include <mutex>
#include <optional>
#include <poll.h>
#include <spawn.h>
#include <string_view>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

extern char **environ;
namespace firepanda {
std::atomic<bool> stop_requested{false};
// Self-pipe: the signal handler and destructor write here so deadline workers
// wake for stop/teardown without polling. [0] is the read end, [1] the write end.
int stop_wakeup[2] = {-1, -1};
bool init_stop_wakeup() noexcept {
  if (pipe(stop_wakeup) != 0) {
    stop_wakeup[0] = stop_wakeup[1] = -1;
    return false;
  }
  bool ok = true;
  for (int descriptor : stop_wakeup) {
    const int flags = fcntl(descriptor, F_GETFL);
    ok = ok && flags >= 0 && fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 &&
         fcntl(descriptor, F_SETFD, FD_CLOEXEC) == 0;
  }
  if (!ok) {
    close(stop_wakeup[0]);
    close(stop_wakeup[1]);
    stop_wakeup[0] = stop_wakeup[1] = -1;
  }
  return ok;
}
void wake_stop_waiters() noexcept {
  if (stop_wakeup[1] >= 0) {
    const char byte = 0;
    const auto ignored = write(stop_wakeup[1], &byte, 1); // Full pipe still wakes a poller.
    (void)ignored;
  }
}
using Clock = std::chrono::steady_clock;
using namespace std::chrono_literals;
namespace {
class CdpError : public std::runtime_error {
public:
  using std::runtime_error::runtime_error;
};
// Enforces an operation deadline by shutting the watched socket down once it
// passes or a stop signal arrives. The worker starts when watch() registers a
// socket after connect() succeeds; before that there is nothing to shut down
// and the connect loop enforces the deadline itself. The worker waits on one
// poll: its private wake pipe signals teardown, the shared self-pipe signals
// stop, and the deadline is the timeout. Each guard owns its pipe because
// concurrent workers draining the shared pipe would steal a sibling's
// teardown byte and stall its join.
class SocketDeadline {
  std::mutex mutex_;
  bool done_ = false;
  int descriptor_ = -1;
  int wake_[2] = {-1, -1};
  Clock::time_point deadline_;
  std::thread worker_;

public:
  explicit SocketDeadline(Clock::time_point deadline) : deadline_(deadline) {
    if (pipe(wake_) == 0) {
      for (int descriptor : wake_) {
        const int flags = fcntl(descriptor, F_GETFL);
        if (flags < 0 || fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) != 0 ||
            fcntl(descriptor, F_SETFD, FD_CLOEXEC) != 0) {
          close(wake_[0]);
          close(wake_[1]);
          wake_[0] = wake_[1] = -1;
          break;
        }
      }
    } else {
      wake_[0] = wake_[1] = -1;
    }
  }
  ~SocketDeadline() {
    {
      std::lock_guard lock(mutex_);
      done_ = true;
    }
    if (wake_[1] >= 0) { // Private pipe: a sibling cannot steal this byte.
      const char byte = 0;
      const auto ignored = write(wake_[1], &byte, 1);
      (void)ignored;
    } else {
      wake_stop_waiters(); // Fall back to the shared pipe if the private one failed.
    }
    if (worker_.joinable()) {
      worker_.join();
    }
    if (descriptor_ >= 0) {
      close(descriptor_);
    }
    for (int descriptor : wake_) {
      if (descriptor >= 0) {
        close(descriptor);
      }
    }
  }
  SocketDeadline(const SocketDeadline &) = delete;
  SocketDeadline &operator=(const SocketDeadline &) = delete;

  void watch(int descriptor) {
    std::lock_guard lock(mutex_);
    if (done_ || Clock::now() >= deadline_ || stop_requested.load()) {
      throw std::runtime_error("Lightpanda operation timed out or service is stopping");
    }
    // Own a duplicate so socket closure/reuse cannot make the worker shut down
    // an unrelated descriptor. Never close httplib's fd.
    const int duplicate = fcntl(descriptor, F_DUPFD_CLOEXEC, 0);
    if (duplicate < 0) {
      throw std::runtime_error("Cannot monitor Lightpanda socket deadline");
    }
    if (descriptor_ >= 0) {
      close(descriptor_);
    }
    descriptor_ = duplicate;
    // Start lazily on the first watched socket: a guard created before connect
    // must not spin a thread that polls with nothing to shut down.
    if (!worker_.joinable()) {
      worker_ = std::thread([this] {
        pollfd watches[2]{{stop_wakeup[0], POLLIN, 0}, {wake_[0], POLLIN, 0}};
        for (;;) {
          {
            std::lock_guard lock(mutex_);
            if (done_) {
              return;
            }
            if (Clock::now() >= deadline_ || stop_requested.load()) {
              // SIGTERM/SIGINT must interrupt even a read that is still
              // reassembling fragments; watch() may not have run yet.
              if (descriptor_ >= 0) {
                shutdown(descriptor_, SHUT_RDWR);
              }
              return;
            }
          }
          const auto remaining =
              std::chrono::duration_cast<std::chrono::milliseconds>(deadline_ - Clock::now());
          // Sibling workers may drain the shared stop byte before this worker
          // re-enters poll(). Cap the wait so a missed byte still re-checks
          // stop_requested promptly; teardown uses the private pipe instead.
          const int timeout = static_cast<int>(std::clamp<int64_t>(remaining.count() + 1, 0, 250));
          if (::poll(watches, 2, timeout) > 0) {
            char drained[32];
            if ((watches[0].revents & POLLIN) != 0) {
              while (stop_wakeup[0] >= 0 && ::read(stop_wakeup[0], drained, sizeof drained) > 0) {
              }
            }
            if ((watches[1].revents & POLLIN) != 0) {
              while (wake_[0] >= 0 && ::read(wake_[0], drained, sizeof drained) > 0) {
              }
            }
          }
        }
      });
    }
  }
};

pid_t wait_child(pid_t pid, int flags) noexcept {
  pid_t result;
  do {
    result = waitpid(pid, nullptr, flags);
  } while (result < 0 && errno == EINTR);
  return result;
}

void check_spawn(int error) {
  if (error) {
    throw std::runtime_error("Cannot start Lightpanda: " + std::string(std::strerror(error)) +
                             "; use --lightpanda /path/to/lightpanda");
  }
}

class SpawnActions {
public:
  SpawnActions() {
    check_spawn(posix_spawn_file_actions_init(&actions));
  }
  ~SpawnActions() {
    posix_spawn_file_actions_destroy(&actions);
  }
  SpawnActions(const SpawnActions &) = delete;
  SpawnActions &operator=(const SpawnActions &) = delete;
  posix_spawn_file_actions_t actions;
};
// CDP events arrive on every message of a scrape. Returning a view avoids
// allocating a std::string for each method/session lookup.
std::string_view string_member(const Json &object, const char *key) {
  const auto found = object.find(key);
  return found != object.end() && found->is_string() ? found->get_ref<const std::string &>()
                                                     : std::string_view{};
}

// Per-connection CDP state. Lightpanda's MultiClient mode gives every WebSocket
// connection its own browser instance, so nothing here is shared between
// concurrent page fetches.
struct Connection {
  std::unique_ptr<httplib::ws::WebSocketClient> ws;
  int socket_descriptor = -1;
  uint64_t next_id = 0;
  std::string active_session;
  bool page_loaded = false;
  std::string main_frame;
  std::optional<int> http_status;
  std::vector<std::pair<std::string, int>> early_document_statuses;

  Json receive(Clock::time_point deadline) {
    while (Clock::now() < deadline) {
      if (stop_requested) {
        throw std::runtime_error("Service is stopping");
      }
      const auto us = std::clamp<int64_t>(
          std::chrono::duration_cast<std::chrono::microseconds>(deadline - Clock::now()).count(), 1,
          250000);
      ws->set_read_timeout(us / 1000000, us % 1000000);
      std::string message;
      const auto result = ws->read(message);
      if (Clock::now() >= deadline || stop_requested.load(std::memory_order_relaxed)) {
        throw std::runtime_error("Lightpanda operation timed out or service is stopping");
      }
      if (result == httplib::ws::Timeout) {
        continue;
      }
      if (result != httplib::ws::Text) {
        throw std::runtime_error("Lightpanda CDP disconnected or returned a non-text frame");
      }
      try {
        return parse_json(message);
      } catch (const std::exception &e) {
        throw std::runtime_error("Invalid CDP JSON: " + std::string(e.what()));
      }
    }
    throw std::runtime_error("Lightpanda operation timed out");
  }
  void observe(const Json &message) {
    // CDP mixes replies and events on one socket. We only need one load flag,
    // not a queue retaining arbitrary (potentially multi-megabyte) event bodies.
    const auto method = string_member(message, "method");
    const auto session_id = string_member(message, "sessionId");
    if (method == "Page.loadEventFired" && session_id == active_session) {
      page_loaded = true;
    }
    if (method != "Network.responseReceived" || session_id != active_session) {
      return;
    }
    const auto params = message.find("params");
    if (params == message.end() || !params->is_object() ||
        string_member(*params, "type") != "Document") {
      return;
    }
    const auto frame = params->find("frameId"), response = params->find("response");
    if (frame == params->end() || !frame->is_string() ||
        frame->get_ref<const std::string &>().size() > 256 || response == params->end() ||
        !response->is_object()) {
      return;
    }
    const auto status = response->find("status");
    if (status == response->end() || !status->is_number() || *status < 100 || *status > 599) {
      return;
    }
    const int number = status->get<int>();
    if (*status != number) {
      return;
    }
    const auto &id = frame->get_ref<const std::string &>();
    if (!main_frame.empty()) {
      if (id == main_frame) {
        http_status = number; // Subframe responses cannot overwrite the document.
      }
    } else {
      auto found = std::find_if(early_document_statuses.begin(), early_document_statuses.end(),
                                [&](const auto &entry) { return entry.first == id; });
      if (found != early_document_statuses.end()) {
        found->second = number;
      } else if (early_document_statuses.size() < 16) {
        early_document_statuses.emplace_back(id, number);
      }
    }
  }
  Json call(const std::string &method, Json params, const std::string &session,
            Clock::time_point deadline) {
    const uint64_t id = ++next_id;
    Json request{{"id", id}, {"method", method}, {"params", std::move(params)}};
    if (!session.empty()) {
      request["sessionId"] = session;
    }
    if (!ws->send(request.dump())) {
      throw std::runtime_error("Cannot write Lightpanda CDP command");
    }
    for (;;) {
      auto response = receive(deadline);
      if (!response.is_object()) {
        throw std::runtime_error("CDP response must be an object");
      }
      if (response.contains("id") && !response["id"].is_number_integer()) {
        throw std::runtime_error("CDP response id must be an integer");
      }
      if (response.value("id", uint64_t{0}) != id) {
        observe(response);
        continue;
      }
      if ((!session.empty() && string_member(response, "sessionId") != session) ||
          response.contains("result") == response.contains("error")) {
        throw std::runtime_error("Malformed CDP response envelope");
      }
      if (response.contains("error")) {
        throw CdpError(method + ": " + response["error"].dump());
      }
      if (!response.at("result").is_object()) {
        throw std::runtime_error("CDP result must be an object");
      }
      return std::move(response.at("result"));
    }
  }
  void loaded(Clock::time_point deadline) {
    while (!page_loaded) {
      observe(receive(deadline));
    }
  }
};

// One page fetch on an already connected connection. Errors propagate to the
// caller; only the crawl loop decides whether to reap the browser.
Json run_scrape(Connection &c, const PageRequest &request, Clock::time_point deadline) {
  c.page_loaded = false;
  const std::string context =
      c.call("Target.createBrowserContext", {{"disposeOnDetach", true}}, "", deadline)
          .at("browserContextId");
  const std::string page =
      c.call("Target.createTarget", {{"url", "about:blank"}, {"browserContextId", context}}, "",
             deadline)
          .at("targetId");
  const std::string session =
      c.call("Target.attachToTarget", {{"targetId", page}, {"flatten", true}}, "", deadline)
          .at("sessionId");
  c.active_session = session;
  c.main_frame.clear();
  c.http_status.reset();
  c.early_document_statuses.clear();
  c.call("Page.enable", Json::object(), session, deadline);
  try {
    c.call("Network.enable", Json::object(), session, deadline);
  } catch (const CdpError &) {
    // Older CDP peers may lack Network; transport/timeouts still fail normally.
  }
  c.page_loaded = false;
  const auto navigation = c.call("Page.navigate", {{"url", request.url}}, session, deadline);
  c.main_frame = std::string(string_member(navigation, "frameId"));
  for (const auto &[frame, status] : c.early_document_statuses) {
    if (!c.main_frame.empty() && frame == c.main_frame) {
      c.http_status = status;
    }
  }
  c.early_document_statuses.clear();
  if (navigation.contains("errorText")) {
    throw std::runtime_error(navigation["errorText"].get<std::string>());
  }
  c.loaded(deadline);
  const auto wait = request.wait_for;
  if (Clock::now() + wait >= deadline) {
    throw std::runtime_error("waitFor exceeds remaining operation timeout");
  }
  // Sleep in short intervals so SIGTERM still tears down the owned child promptly.
  const auto wait_end = Clock::now() + wait;
  while (Clock::now() < wait_end) {
    if (stop_requested) {
      throw std::runtime_error("Service is stopping");
    }
    std::this_thread::sleep_until(std::min(wait_end, Clock::now() + 50ms));
  }
  Json extraction{
      {"maxLinks", max_links},
      {"formats", request.formats},
      {"onlyMainContent", request.only_main_content},
      {"waitForSelector", request.wait_for_selector},
      {"waitForText", request.wait_for_text},
      {"readyTimeout", std::max<int64_t>(0, std::chrono::duration_cast<std::chrono::milliseconds>(
                                                deadline - Clock::now())
                                                .count())}};
  auto value = c.call("Runtime.evaluate",
                      {{"expression", std::string(extraction_js) + "(" + extraction.dump() + ")"},
                       {"returnByValue", true},
                       {"awaitPromise", true}},
                      session, deadline);
  if (value.contains("exceptionDetails")) {
    throw std::runtime_error("Page extraction failed: " + value["exceptionDetails"].dump());
  }
  Json result = std::move(value.at("result").at("value"));
  if (!result.is_object() || !result.contains("metadata") || !result["metadata"].is_object() ||
      !result["metadata"].contains("url") || !result["metadata"]["url"].is_string()) {
    throw std::runtime_error("Malformed page extraction metadata");
  }
  for (const auto &format : request.formats) {
    const auto found = result.find(format);
    if (found == result.end() || (format == "links" ? !found->is_array() : !found->is_string())) {
      throw std::runtime_error("Missing or invalid extraction format: " + format);
    }
  }
  if (const auto links = result.find("links"); links != result.end()) {
    if (!links->is_array() || links->size() > max_links) {
      throw std::runtime_error("Invalid extraction links");
    }
    for (const auto &link : *links) {
      if (!link.is_object() || !link.contains("url") || !link["url"].is_string() ||
          (link.contains("title") && !link["title"].is_string())) {
        throw std::runtime_error("Malformed extraction link");
      }
    }
  }
  if (result.contains("linksTruncated") && !result["linksTruncated"].is_boolean()) {
    throw std::runtime_error("Invalid extraction truncation flag");
  }
  result["metadata"]["sourceURL"] = request.url;
  if (result.contains("warnings")) {
    const auto &warnings = result["warnings"];
    if (!warnings.is_array() || warnings.size() > 3) {
      throw std::runtime_error("Malformed extraction warnings");
    }
    for (const auto &warning : warnings) {
      if (!warning.is_object() || !warning.contains("code") || !warning["code"].is_string() ||
          warning["code"].get_ref<const std::string &>().size() > 64 ||
          !warning.contains("message") || !warning["message"].is_string() ||
          warning["message"].get_ref<const std::string &>().size() > 512) {
        throw std::runtime_error("Malformed extraction warning");
      }
    }
  }
  if (c.http_status) {
    result["metadata"]["httpStatus"] = *c.http_status;
    if (*c.http_status >= 400) {
      if (!result.contains("warnings")) {
        result["warnings"] = Json::array();
      }
      result["warnings"].push_back(
          {{"code", "http_error"},
           {"message", "Document returned HTTP " + std::to_string(*c.http_status)}});
    }
  }
  c.call("Target.disposeBrowserContext", {{"browserContextId", context}}, "", deadline);
  return result;
}
// Fixed page-worker pool. Crawl/map submit every fetch here instead of creating
// one OS thread per page; threads are created lazily up to the configured
// window and reused for later pages.
class TaskPool {
  std::mutex mutex_;
  std::condition_variable ready_;
  std::deque<std::packaged_task<void()>> queue_;
  std::vector<std::thread> threads_;
  size_t limit_ = 1;
  bool stop_ = false;

  void run() {
    for (;;) {
      std::packaged_task<void()> task;
      {
        std::unique_lock lock(mutex_);
        ready_.wait(lock, [this] { return stop_ || !queue_.empty(); });
        if (stop_) {
          return; // Shutdown drops queued pages; their futures report it as broken.
        }
        task = std::move(queue_.front());
        queue_.pop_front();
      }
      task();
    }
  }

public:
  TaskPool() = default;
  TaskPool(const TaskPool &) = delete;
  TaskPool &operator=(const TaskPool &) = delete;
  ~TaskPool() {
    shutdown();
  }
  // Main thread only, before dispatch: the crawl window is the pool ceiling.
  void set_limit(size_t limit) {
    std::lock_guard lock(mutex_);
    limit_ = std::max<size_t>(limit, 1);
  }
  void submit(std::packaged_task<void()> task) {
    std::lock_guard lock(mutex_);
    if (stop_) {
      throw std::runtime_error("Service is stopping");
    }
    // Spawn before queueing: if thread creation fails, no task is left behind
    // that nothing is guaranteed to run.
    if (threads_.size() < limit_) {
      threads_.emplace_back([this] { run(); });
    }
    queue_.push_back(std::move(task));
    ready_.notify_one();
  }
  void shutdown() {
    // Move queued tasks out under the lock; destroying them joins deadline
    // watchers and closes connections, which must not block other workers.
    std::deque<std::packaged_task<void()>> dropped;
    {
      std::lock_guard lock(mutex_);
      stop_ = true;
      dropped.swap(queue_);
    }
    ready_.notify_all();
    for (auto &thread : threads_) {
      if (thread.joinable()) {
        thread.join();
      }
    }
    threads_.clear();
  }
};
} // namespace
// Process state (directory, pid, idle_since) is touched only on the main
// thread, and only while no page worker runs: connections are checked out
// during dispatch and every failure path drains workers before release().
// Only the free-connection pool is shared with workers.
struct Browser::Impl {
  // Idle connections wait here between page fetches. Workers return healthy
  // connections through give_back; release() drops them with the browser.
  struct Pool {
    std::mutex mutex;
    std::vector<std::unique_ptr<Connection>> free;
    void give_back(std::unique_ptr<Connection> connection) {
      if (connection && connection->ws && connection->ws->is_open()) {
        std::lock_guard lock(mutex);
        free.push_back(std::move(connection));
      }
    }
  };

  Options options;
  std::string directory;
  pid_t pid = -1;
  std::optional<Clock::time_point> idle_since;
  const bool owns_process;
  std::shared_ptr<Pool> pool = std::make_shared<Pool>();
  TaskPool tasks;
  explicit Impl(Options o) : options(std::move(o)), owns_process(options.socket.empty()) {}
  ~Impl() {
    tasks.shutdown(); // Join page workers before their connections and child go away.
    release();
  }
  void release() {
    {
      std::lock_guard lock(pool->mutex);
      pool->free.clear();
    }
    stop_child();
    idle_since.reset();
    if (!directory.empty()) {
      std::error_code ec;
      std::filesystem::remove_all(directory, ec);
      directory.clear();
      options.socket.clear();
    }
  }
  void stop_child() {
    if (pid <= 0) {
      return;
    }
    // The PID remains ours until reaped; never signal an unrelated process.
    if (wait_child(pid, WNOHANG) == 0) {
      kill(pid, SIGTERM);
      for (int i = 0; i < 50; ++i) {
        if (wait_child(pid, WNOHANG) != 0) {
          pid = -1;
          return;
        }
        std::this_thread::sleep_for(10ms);
      }
      kill(pid, SIGKILL);
      wait_child(pid, 0);
    }
    pid = -1;
  }
  void launch() {
    if (pid > 0) {
      if (wait_child(pid, WNOHANG) == 0) {
        return;
      }
      pid = -1;
    }
    if (directory.empty()) {
      char path[] = "/tmp/firepanda-XXXXXX";
      auto *dir = mkdtemp(path);
      if (!dir) {
        throw std::runtime_error("Cannot create private socket directory");
      }
      directory = dir;
      options.socket = directory + "/cdp.sock";
    }
    unlink(options.socket.c_str());
    std::vector<std::string> args{options.lightpanda, "serve",       "--unix-socket",
                                  options.socket,     "--log-level", "error"};
    std::vector<char *> argv;
    for (auto &arg : args) {
      argv.push_back(arg.data());
    }
    argv.push_back(nullptr);
    SpawnActions actions;
    check_spawn(
        posix_spawn_file_actions_addopen(&actions.actions, STDIN_FILENO, "/dev/null", O_RDONLY, 0));
    // A browser log on stdout would corrupt the MCP byte stream.
    check_spawn(posix_spawn_file_actions_adddup2(&actions.actions, STDERR_FILENO, STDOUT_FILENO));
    pid_t child = -1;
    check_spawn(
        posix_spawnp(&child, args[0].c_str(), &actions.actions, nullptr, argv.data(), environ));
    pid = child;
  }
  bool managed() const {
    return owns_process;
  }
  void connect(Connection &connection, Clock::time_point deadline, SocketDeadline &guard) {
    if (connection.ws && connection.ws->is_open()) {
      guard.watch(connection.socket_descriptor);
      return;
    }
    const bool own = managed();
    if (own) {
      launch();
    }
    auto &ws = connection.ws;
    do {
      if (stop_requested) {
        throw std::runtime_error("Service is stopping");
      }
      // Host header stays localhost; only the connection target becomes a UDS.
      ws = std::make_unique<httplib::ws::WebSocketClient>("ws://localhost/");
      ws->set_address_family(AF_UNIX);
      ws->set_direct_hostname_addr_map({{"localhost", options.socket}});
      ws->set_websocket_ping_interval(0); // no heartbeat thread on a local link
      ws->set_connection_timeout(0, 100000);
      ws->set_read_timeout(0, 100000);
      ws->set_write_timeout(1);
      connection.socket_descriptor =
          -1; // Only the descriptor of the successful attempt is watched.
      ws->set_socket_options([&connection](int descriptor) {
        httplib::default_socket_options(descriptor);
        connection.socket_descriptor = descriptor;
      });
      const auto connected = ws->connect();
      // The callback must not retain a reference to this operation's guard.
      ws->set_socket_options(httplib::default_socket_options);
      if (connected) {
        // Watch only now: shutdown() on a socket that connect() has not
        // completed reports ENOTCONN, and failed attempts may replace it.
        if (connection.socket_descriptor >= 0) {
          guard.watch(connection.socket_descriptor);
        }
        return;
      }
      ws.reset();
      if (!own) {
        break;
      }
      if (wait_child(pid, WNOHANG) != 0) {
        pid = -1;
        throw std::runtime_error("Lightpanda exited before opening its Unix socket");
      }
      std::this_thread::sleep_for(20ms);
    } while (Clock::now() < deadline);
    throw std::runtime_error("Cannot connect to Lightpanda Unix socket: " + options.socket);
  }
  // Main thread only: connect (launching the managed browser if needed) before
  // any worker touches the connection, so concurrent checkouts can never race
  // a second spawn. The caller returns it through pool->give_back().
  std::unique_ptr<Connection> checkout(Clock::time_point deadline, SocketDeadline &guard) {
    std::unique_ptr<Connection> connection;
    {
      std::lock_guard lock(pool->mutex);
      if (!pool->free.empty()) {
        connection = std::move(pool->free.back());
        pool->free.pop_back();
      }
    }
    if (!connection) {
      connection = std::make_unique<Connection>();
    }
    connect(*connection, deadline, guard);
    return connection;
  }
};

Browser::Browser(Options o) : impl_(std::make_unique<Impl>(std::move(o))) {}
Browser::~Browser() = default;
std::optional<Clock::time_point> Browser::idle_deadline() const {
  const auto &b = *impl_;
  if (!b.managed() || b.pid <= 0 || !b.idle_since || b.options.idle_timeout.count() == 0) {
    return std::nullopt;
  }
  return *b.idle_since + b.options.idle_timeout;
}
void Browser::stop_if_idle() {
  const auto deadline = idle_deadline();
  if (deadline && Clock::now() >= *deadline) {
    // Called only between requests on the service thread, never during active CDP work.
    impl_->release();
  }
}
void Browser::page_completed() {
  impl_->idle_since = Clock::now(); // Inactivity starts after work completes, not when it starts.
}
void Browser::restart() {
  // Only called after the crawl loop drained every page worker, so no
  // connection is checked out and the pool can be dropped wholesale.
  impl_->release();
}
Json Browser::scrape(const PageRequest &request) {
  auto &b = *impl_;
  b.idle_since.reset();
  const auto deadline = Clock::now() + request.timeout;
  try {
    // httplib's timeout is per read. Continuous control frames or fragments can
    // keep a read busy indefinitely, so enforce the operation deadline separately.
    auto guard = std::make_unique<SocketDeadline>(deadline);
    auto connection = b.checkout(std::min(deadline, Clock::now() + 10s), *guard);
    auto result = run_scrape(*connection, request, deadline);
    // Stop the deadline watcher before the connection returns to the pool: a
    // late shutdown() must not hit a socket a later page reuses.
    guard.reset();
    b.pool->give_back(std::move(connection));
    b.idle_since = Clock::now(); // Inactivity starts after work completes, not when it starts.
    return result;
  } catch (...) {
    // disposeOnDetach releases this operation's context even if its CDP stream
    // is unusable. Sending another command on a failed stream can hang cleanup.
    b.release(); // Close late replies and remove owned resources; external sockets stay intact.
    throw;
  }
}
std::future<Json> Browser::scrape_async(PageRequest request) {
  auto promise = std::make_shared<std::promise<Json>>();
  auto future = promise->get_future();
  try {
    auto &b = *impl_;
    b.idle_since.reset();
    const auto deadline = Clock::now() + request.timeout;
    // The guard starts before checkout so a stalled connect still hits the deadline.
    auto guard = std::make_unique<SocketDeadline>(deadline);
    auto connection = b.checkout(std::min(deadline, Clock::now() + 10s), *guard);
    std::packaged_task<void()> task([pool = b.pool, connection = std::move(connection),
                                     request = std::move(request), deadline,
                                     guard = std::move(guard), promise]() mutable {
      try {
        auto result = run_scrape(*connection, request, deadline);
        // Stop the deadline watcher before the connection returns to the
        // pool: the task outlives this scope, so a live guard could
        // shutdown() a socket reused by a later page.
        guard.reset();
        // Only healthy connections return to the pool; an error destroys
        // this one and the crawl loop reaps the browser after draining.
        pool->give_back(std::move(connection));
        promise->set_value(std::move(result));
      } catch (...) {
        promise->set_exception(std::current_exception());
      }
    });
    b.tasks.submit(std::move(task));
  } catch (...) {
    // Connect/launch failures behave like page errors: the crawl loop records
    // them and reaps the browser without a worker having run.
    promise->set_exception(std::current_exception());
  }
  return future;
}
void Browser::set_parallelism(size_t limit) {
  impl_->tasks.set_limit(limit);
}

} // namespace firepanda
