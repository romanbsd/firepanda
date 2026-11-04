#include "browser.hpp"
#include <algorithm>
#include <cerrno>
#include <csignal>
#include <cstring>
#include <iostream>
#include <optional>
#include <pthread.h>
#include <sys/select.h>
#include <unistd.h>

namespace {
using firepanda::Json;
void interrupt(int) {
  firepanda::stop_requested = 1;
}

bool wait_for_input() {
  sigset_t stopping, previous;
  sigemptyset(&stopping);
  sigaddset(&stopping, SIGTERM);
  sigaddset(&stopping, SIGINT);
  if (const int error = pthread_sigmask(SIG_BLOCK, &stopping, &previous)) {
    throw std::runtime_error("Cannot mask shutdown signals: " + std::string(std::strerror(error)));
  }
  fd_set readable;
  FD_ZERO(&readable);
  FD_SET(STDIN_FILENO, &readable);
  // pselect atomically unmasks signals while waiting. Otherwise SIGTERM between
  // checking stop_requested and entering read() could leave an idle server stuck.
  const int ready = firepanda::stop_requested ? 0
                                              : pselect(STDIN_FILENO + 1, &readable, nullptr,
                                                        nullptr, nullptr, &previous);
  const int wait_error = errno;
  if (const int error = pthread_sigmask(SIG_SETMASK, &previous, nullptr)) {
    throw std::runtime_error("Cannot restore shutdown signals: " +
                             std::string(std::strerror(error)));
  }
  if (ready < 0 && wait_error != EINTR) {
    throw std::runtime_error("Cannot wait for stdin: " + std::string(std::strerror(wait_error)));
  }
  return ready > 0 && !firepanda::stop_requested;
}
constexpr const char *version_key = "io.modelcontextprotocol/protocolVersion";
const Json versions =
    Json::array({"2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"});
const Json identity{{"name", "firepanda"}, {"version", "0.2.0"}};
Json error(Json id, int code, const std::string &message) {
  return {
      {"jsonrpc", "2.0"}, {"id", std::move(id)}, {"error", {{"code", code}, {"message", message}}}};
}
class Server {
  firepanda::Browser browser_;
  bool initialized_ = false;

public:
  explicit Server(firepanda::Options options) : browser_(std::move(options)) {}
  std::optional<Json> handle(const Json &r) {
    Json id = r.is_object() ? r.value("id", Json(nullptr)) : Json(nullptr);
    if (!r.is_object() || r.value("jsonrpc", Json()) != "2.0" || !r.contains("method") ||
        !r["method"].is_string() ||
        (r.contains("id") && !(id.is_string() || id.is_number_integer())) ||
        (r.contains("params") && !r["params"].is_object())) {
      return error(nullptr, -32600, "Invalid JSON-RPC request");
    }
    if (!r.contains("id")) {
      return std::nullopt;
    }
    const std::string method = r.at("method");
    static const Json empty = Json::object();
    const auto &p = r.contains("params") ? r.at("params") : empty;
    const auto &meta = p.contains("_meta") ? p.at("_meta") : empty;
    if (!meta.is_object()) {
      return error(id, -32602, "_meta must be an object");
    }
    const auto version = meta.value(version_key, Json());
    const bool modern = version == "2026-07-28";
    if (!version.is_null() && version != "2026-07-28") {
      auto e = error(id, -32022, "Unsupported protocol version");
      e["error"]["data"] = {{"supported", versions}, {"requested", version}};
      return e;
    }
    Json result;
    try {
      if (method == "server/discover") {
        result = {{"supportedVersions", versions}, {"capabilities", {{"tools", Json::object()}}}};
      } else if (method == "initialize") {
        if (!p.contains("protocolVersion") || !p["protocolVersion"].is_string() ||
            !p.contains("capabilities") || !p["capabilities"].is_object() ||
            !p.contains("clientInfo") || !p["clientInfo"].is_object()) {
          return error(id, -32602,
                       "initialize requires protocolVersion, capabilities, and clientInfo");
        }
        auto negotiated = p.at("protocolVersion");
        if (negotiated == "2026-07-28" ||
            std::find(versions.begin(), versions.end(), negotiated) == versions.end()) {
          negotiated = "2025-11-25";
        }
        initialized_ = true;
        result = {{"protocolVersion", negotiated},
                  {"capabilities", {{"tools", Json::object()}}},
                  {"serverInfo", identity}};
      } else {
        if (!modern && !initialized_) {
          return error(id, -32600, "Initialize first or provide modern protocol metadata");
        }
        if (method == "ping") {
          result = Json::object();
        } else if (method == "tools/list") {
          if (p.contains("cursor")) {
            return error(id, -32602, "No pagination cursor is supported");
          }
          result = {{"tools", firepanda::tools()}};
        } else if (method == "tools/call") {
          if (!p.contains("name") || !p["name"].is_string()) {
            return error(id, -32602, "Tool name must be a string");
          }
          const auto name = p["name"].get<std::string>();
          try {
            auto data = firepanda::execute(browser_, name,
                                           p.contains("arguments") ? p.at("arguments") : empty);
            auto text = data.dump(-1, ' ', false, Json::error_handler_t::replace);
            if (text.size() > 8 * 1024 * 1024) {
              throw std::runtime_error(
                  "Tool result exceeds 8 MiB; select fewer output formats or pages");
            }
            // Build large values by assignment: JSON initializer lists copy their elements,
            // even when those elements are written as std::move(...).
            result = Json::object();
            Json content{{"type", "text"}};
            content["text"] = std::move(text);
            result["content"] = Json::array();
            result["content"].push_back(std::move(content));
            result["structuredContent"] = std::move(data);
            result["isError"] = false;
          } catch (const std::invalid_argument &e) {
            return error(id, -32602, e.what());
          } catch (const std::exception &e) {
            result = {{"content", Json::array({{{"type", "text"}, {"text", e.what()}}})},
                      {"isError", true}};
          }
        } else {
          return error(id, -32601, "Method not found");
        }
      }
    } catch (const Json::exception &e) {
      return error(id, -32602, e.what());
    }
    if (modern || method == "server/discover") {
      result["resultType"] = "complete";
      result["_meta"] = {{"io.modelcontextprotocol/serverInfo", identity}};
    }
    Json response{{"jsonrpc", "2.0"}, {"id", id}};
    response["result"] = std::move(result);
    return response;
  }
};
} // namespace

int main(int argc, char **argv) {
  firepanda::Options options;
  for (int i = 1; i < argc; ++i) {
    const std::string arg = argv[i];
    if (arg == "--help") {
      std::cout << "firepanda [--lightpanda /path/to/lightpanda | --socket /absolute/cdp.sock]\n"
                   "MCP over stdio; starts native Lightpanda lazily unless --socket is given.\n";
      return 0;
    }
    if (arg == "--version") {
      std::cout << "firepanda 0.2.0\n";
      return 0;
    }
    if ((arg == "--lightpanda" || arg == "--socket") && i + 1 < argc) {
      if (arg == "--lightpanda") {
        options.lightpanda = argv[++i];
      } else {
        options.socket = argv[++i];
      }
    } else {
      std::cerr << "Unknown or incomplete option: " << arg << '\n';
      return 2;
    }
  }
  if (!options.socket.empty() && (options.socket.front() != '/' || options.socket.size() > 100)) {
    std::cerr << "--socket must be an absolute Unix socket path of at most 100 bytes\n";
    return 2;
  }
  struct sigaction action{};
  action.sa_handler = interrupt;
  sigemptyset(&action.sa_mask);
  sigaction(SIGTERM, &action, nullptr);
  sigaction(SIGINT, &action, nullptr);
  std::signal(SIGPIPE, SIG_IGN);
  try {
    Server server(std::move(options));
    std::string line;
    bool oversized = false;
    // Buffered reads keep stdio cheap while bounding memory before JSON parsing.
    char buffer[8192];
    while (!firepanda::stop_requested) {
      if (!wait_for_input()) {
        continue;
      }
      const auto count = read(STDIN_FILENO, buffer, sizeof(buffer));
      if (count < 0 && errno == EINTR) {
        continue;
      }
      if (count <= 0) {
        break;
      }
      for (ssize_t i = 0; i < count && !firepanda::stop_requested; ++i) {
        const char c = buffer[i];
        if (c != '\n') {
          if (!oversized && line.size() < 1024 * 1024) {
            line += c;
          } else {
            oversized = true;
            line.clear();
          }
          continue;
        }
        std::optional<Json> response;
        if (oversized) {
          response = error(nullptr, -32600, "Request exceeds 1 MiB");
        } else {
          try {
            auto request = firepanda::parse_json(line);
            response = server.handle(request);
          } catch (const Json::parse_error &) {
            response = error(nullptr, -32700, "Parse error");
          } catch (const std::invalid_argument &e) {
            response = error(nullptr, -32600, e.what());
          } catch (const Json::exception &) {
            response = error(nullptr, -32600, "Invalid JSON-RPC request");
          }
        }
        line.clear();
        oversized = false;
        if (response) {
          std::cout << response->dump(-1, ' ', false, Json::error_handler_t::replace) << '\n'
                    << std::flush;
          // Once the client closes its reader, further tool work cannot be delivered.
          // Returning through this scope still tears down our browser and socket directory.
          if (!std::cout) {
            return 1;
          }
        }
      }
    }
  } catch (const std::exception &e) {
    std::cerr << "firepanda: " << e.what() << '\n';
    return 1;
  }
}
