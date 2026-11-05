#pragma once

#include <nlohmann/json.hpp>
#include <optional>
#include <ostream>
#include <stdexcept>
#include <streambuf>
#include <string_view>

namespace firepanda {
using Json = nlohmann::json;

// Use the library's serializer so escaping, UTF-8 validation and float formatting
// stay identical to dump(), without allocating another copy of the entire page.
inline std::optional<size_t> serialized_size_within(const Json &value, size_t limit) {
  struct LimitExceeded {};
  class Counter final : public std::streambuf {
  public:
    size_t count = 0;
    explicit Counter(size_t limit) : remaining(limit) {}

  private:
    size_t remaining;
    std::streamsize xsputn(const char *, std::streamsize n) override {
      const auto size = static_cast<size_t>(n);
      if (size > remaining) {
        throw LimitExceeded{};
      }
      remaining -= size;
      count += size;
      return n;
    }
    int_type overflow(int_type ch) override {
      if (!traits_type::eq_int_type(ch, traits_type::eof())) {
        xsputn(nullptr, 1);
      }
      return traits_type::not_eof(ch);
    }
  } counter(limit);
  std::ostream output(&counter);
  output.exceptions(std::ios::badbit | std::ios::failbit);
  try {
    output << value;
    return counter.count;
  } catch (const LimitExceeded &) {
    return std::nullopt;
  }
}

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
