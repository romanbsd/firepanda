#pragma once

#include <nlohmann/json.hpp>
#include <stdexcept>
#include <string_view>

namespace firepanda {
using Json = nlohmann::json;

inline Json parse_json(std::string_view input) {
  // Byte limits alone do not bound recursion while parsing or destroying JSON.
  // Apply the same stack bound to MCP input and messages from an external CDP peer.
  return Json::parse(input.begin(), input.end(), [](int depth, Json::parse_event_t, Json &) {
    if (depth > 64) {
      throw std::invalid_argument("JSON nesting exceeds 64 levels");
    }
    return true;
  });
}
} // namespace firepanda
