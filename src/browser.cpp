#include "browser.hpp"
#include "extraction.hpp"
#include <algorithm>
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <csignal>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <httplib.h>
#include <mutex>
#include <optional>
#include <spawn.h>
#include <string_view>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

extern char **environ;
namespace firepanda {
std::atomic<bool> stop_requested{false};
using Clock = std::chrono::steady_clock;
using namespace std::chrono_literals;
namespace {
class CdpError : public std::runtime_error {
public:
  using std::runtime_error::runtime_error;
};
class SocketDeadline {
  std::mutex mutex_;
  std::condition_variable changed_;
  bool done_ = false;
  int descriptor_ = -1;
  Clock::time_point deadline_;
  std::thread worker_;

public:
  explicit SocketDeadline(Clock::time_point deadline)
      : deadline_(deadline), worker_([this] {
          std::unique_lock lock(mutex_);
          while (!done_) {
            const auto now = Clock::now();
            const bool expired = now >= deadline_ || stop_requested.load(std::memory_order_relaxed);
            // A socket observed before connect() can still report ENOTCONN.
            // Retry until it connects or the owning operation finishes.
            if (expired && descriptor_ >= 0 && shutdown(descriptor_, SHUT_RDWR) == 0) {
              return;
            }
            changed_.wait_until(lock, expired ? now + 25ms : std::min(deadline_, now + 25ms),
                                [this] { return done_; });
          }
        }) {}
  ~SocketDeadline() {
    {
      std::lock_guard lock(mutex_);
      done_ = true;
    }
    changed_.notify_one();
    worker_.join();
    if (descriptor_ >= 0) {
      close(descriptor_);
    }
  }
  SocketDeadline(const SocketDeadline &) = delete;
  SocketDeadline &operator=(const SocketDeadline &) = delete;

  void watch(int descriptor) {
    std::lock_guard lock(mutex_);
    if (Clock::now() >= deadline_ || stop_requested.load(std::memory_order_relaxed)) {
      throw std::runtime_error("Lightpanda operation timed out or service is stopping");
    }
    // Own a duplicate so socket closure/reuse during connection retries cannot
    // make the worker shut down an unrelated descriptor. Never close httplib's fd.
    const int duplicate = fcntl(descriptor, F_DUPFD_CLOEXEC, 0);
    if (duplicate < 0) {
      throw std::runtime_error("Cannot monitor Lightpanda socket deadline");
    }
    if (descriptor_ >= 0) {
      close(descriptor_);
    }
    descriptor_ = duplicate;
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
} // namespace
struct Browser::Impl {
  Options options;
  std::string directory;
  pid_t pid = -1;
  std::unique_ptr<httplib::ws::WebSocketClient> ws;
  int socket_descriptor = -1;
  uint64_t next_id = 0;
  std::string active_session;
  bool page_loaded = false;
  std::optional<Clock::time_point> idle_since;
  std::string main_frame;
  std::optional<int> http_status;
  std::vector<std::pair<std::string, int>> early_document_statuses;
  const bool owns_process;
  explicit Impl(Options o) : options(std::move(o)), owns_process(options.socket.empty()) {}
  ~Impl() {
    release();
  }
  void release() {
    ws.reset();
    socket_descriptor = -1;
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
  void connect(Clock::time_point deadline, SocketDeadline &guard) {
    if (ws && ws->is_open()) {
      guard.watch(socket_descriptor);
      return;
    }
    const bool own = managed();
    if (own) {
      launch();
    }
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
      ws->set_socket_options([this, &guard](int descriptor) {
        httplib::default_socket_options(descriptor);
        try {
          guard.watch(descriptor);
        } catch (...) {
          // The pinned httplib calls this before recording ownership of the raw
          // socket; an exception here must close it or that descriptor is lost.
          close(descriptor);
          throw;
        }
        socket_descriptor = descriptor;
      });
      const auto connected = ws->connect();
      // The callback must not retain a reference to this operation's guard.
      ws->set_socket_options(httplib::default_socket_options);
      if (connected) {
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
Json Browser::scrape(const PageRequest &request) {
  auto &b = *impl_;
  b.idle_since.reset();
  const auto deadline = Clock::now() + request.timeout;
  try {
    // httplib's timeout is per read. Continuous control frames or fragments can
    // keep a read busy indefinitely, so enforce the operation deadline separately.
    SocketDeadline guard(deadline);
    b.connect(std::min(deadline, Clock::now() + 10s), guard);
    b.page_loaded = false;
    const std::string context =
        b.call("Target.createBrowserContext", {{"disposeOnDetach", true}}, "", deadline)
            .at("browserContextId");
    const std::string page =
        b.call("Target.createTarget", {{"url", "about:blank"}, {"browserContextId", context}}, "",
               deadline)
            .at("targetId");
    const std::string session =
        b.call("Target.attachToTarget", {{"targetId", page}, {"flatten", true}}, "", deadline)
            .at("sessionId");
    b.active_session = session;
    b.main_frame.clear();
    b.http_status.reset();
    b.early_document_statuses.clear();
    b.call("Page.enable", Json::object(), session, deadline);
    try {
      b.call("Network.enable", Json::object(), session, deadline);
    } catch (const CdpError &) {
      // Older CDP peers may lack Network; transport/timeouts still fail normally.
    }
    b.page_loaded = false;
    const auto navigation = b.call("Page.navigate", {{"url", request.url}}, session, deadline);
    b.main_frame = std::string(string_member(navigation, "frameId"));
    for (const auto &[frame, status] : b.early_document_statuses) {
      if (!b.main_frame.empty() && frame == b.main_frame) {
        b.http_status = status;
      }
    }
    b.early_document_statuses.clear();
    if (navigation.contains("errorText")) {
      throw std::runtime_error(navigation["errorText"].get<std::string>());
    }
    b.loaded(deadline);
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
    auto value = b.call("Runtime.evaluate",
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
    if (b.http_status) {
      result["metadata"]["httpStatus"] = *b.http_status;
      if (*b.http_status >= 400) {
        if (!result.contains("warnings")) {
          result["warnings"] = Json::array();
        }
        result["warnings"].push_back(
            {{"code", "http_error"},
             {"message", "Document returned HTTP " + std::to_string(*b.http_status)}});
      }
    }
    b.call("Target.disposeBrowserContext", {{"browserContextId", context}}, "", deadline);
    b.idle_since = Clock::now(); // Inactivity starts after work completes, not when it starts.
    return result;
  } catch (...) {
    // disposeOnDetach releases this operation's context even if its CDP stream
    // is unusable. Sending another command on a failed stream can hang cleanup.
    b.release(); // Close late replies and remove owned resources; external sockets stay intact.
    throw;
  }
}

} // namespace firepanda
