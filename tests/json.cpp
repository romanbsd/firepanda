#include "json.hpp"

#include <chrono>
#include <iostream>
#include <limits>
#include <stdexcept>
#include <string_view>

using firepanda::Json;
using firepanda::serialized_size_within;

void require(bool condition) {
  if (!condition) {
    throw std::runtime_error("Serialized size mismatch");
  }
}

void check(const Json &value) {
  const auto expected = value.dump().size();
  require(serialized_size_within(value, expected) == expected);
  require(serialized_size_within(value, expected + 1) == expected);
  require(!serialized_size_within(value, expected - 1));
  require(!serialized_size_within(value, 0));
  require(serialized_size_within(value, std::numeric_limits<size_t>::max()) == expected);
}

int main(int argc, char **argv) {
  for (const auto &value :
       {Json(), Json(true), Json(false), Json(-42), Json(std::numeric_limits<uint64_t>::max()),
        Json(1.234e-50), Json(std::numeric_limits<double>::infinity()), Json(""),
        Json("quotes\" slash\\ newline\n\t\x01 UTF-8: é🐼"), Json::array(), Json::object(),
        Json{{"nested", {1, "two", nullptr, {true, false}}}}}) {
    check(value);
  }
  check(Json{{"text", std::string(256 * 1024, '\n')}});
  check(Json{{"text", std::string(4 * 1024 * 1024, 'x')}});
  // Invalid UTF-8 must raise the same strict serializer error as dump().
  bool invalid = false;
  try {
    (void)serialized_size_within(Json(std::string("\xff")), 100);
  } catch (const Json::type_error &) {
    invalid = true;
  }
  require(invalid);

  if (argc == 2 && std::string_view(argv[1]) == "--benchmark") {
    using Clock = std::chrono::steady_clock;
    for (const size_t length : {256 * 1024, 9 * 1024 * 1024}) {
      const Json page{{"text", std::string(length, 'x')}};
      constexpr size_t limit = 4 * 1024 * 1024;
      constexpr int iterations = 100;
      size_t total = 0;
      auto start = Clock::now();
      for (int i = 0; i < iterations; ++i) {
        total += page.dump().size() <= limit;
      }
      const auto dump = Clock::now() - start;
      start = Clock::now();
      for (int i = 0; i < iterations; ++i) {
        total += serialized_size_within(page, limit).has_value();
      }
      const auto count = Clock::now() - start;
      std::cout << length << " bytes, " << iterations
                << " iterations: dump=" << std::chrono::duration<double, std::milli>(dump).count()
                << "ms count=" << std::chrono::duration<double, std::milli>(count).count()
                << "ms accepted=" << total << '\n';
    }
  }
}
