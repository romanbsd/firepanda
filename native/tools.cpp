#include "browser.hpp"

#include <algorithm>
#include <arpa/inet.h>
#include <array>
#include <charconv>
#include <deque>
#include <string_view>
#include <unordered_set>

namespace firepanda {
namespace {
using Clock = std::chrono::steady_clock;
using namespace std::chrono_literals;
constexpr size_t max_links = 10000;
constexpr size_t max_crawl_bytes = 4 * 1024 * 1024;
constexpr std::array<std::string_view, 5> formats{"markdown", "text", "html", "rawHtml", "links"};

// Keep defaults and bounds in one place: the advertised schema must match validation.
struct IntegerOption {
  const char *name;
  int fallback;
  int minimum;
  int maximum;

  int parse(const Json &args) const {
    const auto it = args.find(name);
    if (it == args.end()) {
      return fallback;
    }
    if (!it->is_number_integer()) {
      throw std::invalid_argument(std::string(name) + " must be an integer");
    }
    // Compare as JSON before narrowing, including unsigned integers above INT64_MAX.
    if (*it < minimum || *it > maximum) {
      throw std::invalid_argument(std::string(name) + " is outside the supported range");
    }
    return it->get<int>();
  }

  Json schema() const {
    return {{"type", "integer"}, {"minimum", minimum}, {"maximum", maximum}, {"default", fallback}};
  }
};
constexpr IntegerOption timeout_option{"timeout", 30000, 100, 120000};
constexpr IntegerOption wait_option{"waitFor", 0, 0, 30000};
constexpr IntegerOption limit_option{"limit", 10, 1, 100};
constexpr IntegerOption depth_option{"maxDepth", 1, 0, 5};

enum class Tool { scrape, crawl, map };
Tool parse_tool(std::string_view name) {
  if (name == "scrape") {
    return Tool::scrape;
  }
  if (name == "crawl") {
    return Tool::crawl;
  }
  if (name == "map") {
    return Tool::map;
  }
  throw std::invalid_argument("Unknown tool: " + std::string(name));
}

bool has_format(const std::vector<std::string> &selected, std::string_view format) {
  return std::find(selected.begin(), selected.end(), format) != selected.end();
}

std::string lower(std::string_view value) {
  std::string result(value);
  for (char &c : result) {
    if (c >= 'A' && c <= 'Z') {
      c += 'a' - 'A';
    }
  }
  return result;
}

std::string normalize_url(std::string_view input) {
  if (input.size() > 8192 || std::any_of(input.begin(), input.end(), [](unsigned char c) {
        return c <= 32 || c == 127 || c == '\\';
      })) {
    throw std::invalid_argument("Invalid URL characters or length");
  }
  const auto separator = input.find("://");
  if (separator == std::string_view::npos) {
    throw std::invalid_argument("Use an absolute HTTP(S) URL without credentials");
  }
  const auto scheme = lower(input.substr(0, separator));
  if (scheme != "http" && scheme != "https") {
    throw std::invalid_argument("Only HTTP(S) URLs are supported");
  }
  const auto host_start = separator + 3;
  const auto host_end = input.find_first_of("/?#", host_start);
  auto authority = input.substr(
      host_start, host_end == std::string_view::npos ? host_end : host_end - host_start);
  auto host = authority;
  std::string canonical_host;
  std::string_view port;
  if (host.starts_with('[')) {
    const auto close = host.find(']');
    if (close == std::string_view::npos) {
      throw std::invalid_argument("Invalid IPv6 URL host");
    }
    host = authority.substr(0, close + 1);
    in6_addr address{};
    const std::string literal(host.substr(1, host.size() - 2));
    if (inet_pton(AF_INET6, literal.c_str(), &address) != 1) {
      throw std::invalid_argument("Invalid IPv6 URL host");
    }
    char text[INET6_ADDRSTRLEN];
    if (!inet_ntop(AF_INET6, &address, text, sizeof(text))) {
      throw std::invalid_argument("Cannot normalize IPv6 URL host");
    }
    // Equivalent IPv6 spellings must not split the crawl's same-origin frontier.
    canonical_host = "[" + std::string(text) + "]";
  } else {
    host = authority.substr(0, authority.find(':'));
    if (host.empty() || !std::all_of(host.begin(), host.end(), [](char c) {
          return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
                 c == '.' || c == '_' || c == '-';
        })) {
      throw std::invalid_argument("Use an absolute HTTP(S) URL without credentials");
    }
  }
  if (host.size() != authority.size()) {
    if (authority[host.size()] != ':') {
      throw std::invalid_argument("Invalid URL authority");
    }
    port = authority.substr(host.size() + 1);
  }
  std::string normalized = scheme + "://" + (canonical_host.empty() ? lower(host) : canonical_host);
  if (!port.empty() || host.size() != authority.size()) {
    unsigned number = 0;
    const auto [end, error] = std::from_chars(port.data(), port.data() + port.size(), number);
    if (error != std::errc{} || end != port.data() + port.size() || number == 0 || number > 65535 ||
        port.size() > 5) {
      throw std::invalid_argument("Invalid URL port");
    }
    if (!(scheme == "http" && number == 80) && !(scheme == "https" && number == 443)) {
      normalized += ":" + std::to_string(number);
    }
  }
  auto path = host_end == std::string_view::npos ? std::string_view{} : input.substr(host_end);
  path = path.substr(0, path.find('#'));
  if (path.empty() || path.starts_with('?')) {
    normalized += '/';
  }
  normalized += path;
  return normalized;
}

// Call only after normalization; avoid reparsing and allocating for every crawl link.
std::string_view origin(std::string_view normalized) {
  return normalized.substr(0, normalized.find('/', normalized.find("://") + 3));
}

PageRequest parse_request(const Json &args, Tool tool) {
  if (!args.is_object()) {
    throw std::invalid_argument("arguments must be an object");
  }
  static constexpr std::array<std::string_view, 7> common{
      "url", "formats", "onlyMainContent", "timeout", "waitFor", "waitForSelector", "waitForText"};
  for (const auto &[key, value] : args.items()) {
    (void)value;
    if (std::find(common.begin(), common.end(), key) == common.end() &&
        !(tool != Tool::scrape && (key == "limit" || key == "maxDepth"))) {
      throw std::invalid_argument("Unknown argument: " + key);
    }
  }
  if (!args.contains("url") || !args["url"].is_string()) {
    throw std::invalid_argument("url must be a string");
  }
  PageRequest request;
  request.url = normalize_url(args["url"].get_ref<const std::string &>());
  if (args.contains("onlyMainContent")) {
    if (!args["onlyMainContent"].is_boolean()) {
      throw std::invalid_argument("onlyMainContent must be boolean");
    }
    request.only_main_content = args["onlyMainContent"].get<bool>();
  }
  request.timeout = std::chrono::milliseconds(timeout_option.parse(args));
  request.wait_for = std::chrono::milliseconds(wait_option.parse(args));
  const auto condition = [&](const char *name, size_t maximum) {
    const auto it = args.find(name);
    if (it == args.end()) {
      return std::string{};
    }
    if (!it->is_string() || it->get_ref<const std::string &>().empty() ||
        it->get_ref<const std::string &>().size() > maximum) {
      throw std::invalid_argument(std::string(name) +
                                  " must be a nonempty string within its byte limit");
    }
    return it->get<std::string>();
  };
  request.wait_for_selector = condition("waitForSelector", 2048);
  request.wait_for_text = condition("waitForText", 4096);
  if (args.contains("formats")) {
    if (!args["formats"].is_array() || args["formats"].empty()) {
      throw std::invalid_argument("formats must be a nonempty array");
    }
    request.formats.clear();
    for (const auto &value : args["formats"]) {
      if (!value.is_string()) {
        throw std::invalid_argument("Each format must be a string");
      }
      const auto &format = value.get_ref<const std::string &>();
      if (std::find(formats.begin(), formats.end(), format) == formats.end()) {
        throw std::invalid_argument("Unsupported format: " + format);
      }
      if (!has_format(request.formats, format)) {
        request.formats.push_back(format);
      }
    }
  }
  return request;
}

Json make_tools() {
  Json list = Json::array();
  Json properties{
      {"url", {{"type", "string"}, {"description", "Absolute HTTP(S) URL without credentials"}}},
      {"formats",
       {{"type", "array"},
        {"minItems", 1},
        {"items", {{"type", "string"}, {"enum", formats}}},
        {"default", {"markdown"}}}},
      {"onlyMainContent", {{"type", "boolean"}, {"default", true}}},
      {"timeout", timeout_option.schema()},
      {"waitFor", wait_option.schema()}};
  properties["waitForSelector"] = {
      {"type", "string"},
      {"minLength", 1},
      {"maxLength", 2048},
      {"description", "Wait for a CSS selector to match an element (presence, not visibility), "
                      "within timeout. Maximum 2048 UTF-8 bytes."}};
  properties["waitForText"] = {
      {"type", "string"},
      {"minLength", 1},
      {"maxLength", 4096},
      {"description", "Wait for literal case-sensitive body text outside scripts/styles/templates, "
                      "within timeout. Maximum 4096 UTF-8 bytes."}};
  properties["timeout"]["description"] =
      "Total per-page budget in milliseconds, including navigation and extraction";
  properties["waitFor"]["description"] = "Extra wait after page load, within timeout";
  constexpr std::array names{"scrape", "crawl", "map"};
  constexpr std::array descriptions{
      "Render a page with native Lightpanda and extract content. Markdown supports common article "
      "elements.",
      "Breadth-first crawl of same-origin pages. Returns pages and per-page errors; bounded by "
      "depth, page count, 120 seconds, and result size.",
      "Discover links with a bounded same-origin crawl. External links are listed but never "
      "followed. Not a sitemap or search engine."};
  for (size_t i = 0; i < names.size(); ++i) {
    auto props = properties;
    if (i != 0) {
      props["limit"] = limit_option.schema();
      props["maxDepth"] = depth_option.schema();
    }
    list.push_back(
        {{"name", names[i]},
         {"description", descriptions[i]},
         {"inputSchema",
          {{"type", "object"},
           {"properties", std::move(props)},
           {"required", {"url"}},
           {"additionalProperties", false}}},
         {"annotations",
          {{"readOnlyHint", true}, {"destructiveHint", false}, {"openWorldHint", true}}}});
  }
  return list;
}
} // namespace

const Json &tools() {
  // Immutable schemas do not need rebuilding for every tools/list request.
  static const Json list = make_tools();
  return list;
}

Json execute(Browser &browser, const std::string &name, const Json &args) {
  const auto tool = parse_tool(name);
  auto request = parse_request(args, tool);
  if (tool == Tool::scrape) {
    return browser.scrape(request);
  }
  const int limit = limit_option.parse(args), max_depth = depth_option.parse(args);
  const std::string start_origin(origin(request.url));
  const auto deadline = Clock::now() + 120s;
  const auto page_timeout = request.timeout;
  const bool return_links = has_format(request.formats, "links");
  if (tool == Tool::map) {
    request.formats = {"links"};
  }
  std::deque<std::pair<std::string, int>> queue{{request.url, 0}};
  std::unordered_set<std::string> queued{request.url}, discovered, visited;
  Json pages = Json::array(), links = Json::array(), errors = Json::array(),
       warnings = Json::array();
  int attempted = 0;
  size_t bytes = 0;
  bool truncated = false;
  while (!queue.empty() && !stop_requested) {
    auto [url, depth] = std::move(queue.front());
    queue.pop_front();
    if (visited.contains(url)) {
      continue;
    }
    const auto remaining =
        std::chrono::duration_cast<std::chrono::milliseconds>(deadline - Clock::now());
    if (attempted == limit || remaining < 100ms) {
      truncated = true;
      break;
    }
    ++attempted;
    request.url = url;
    request.timeout = std::min(page_timeout, remaining);
    // Leaf pages need no frontier links unless the caller requested them.
    const bool need_links = tool == Tool::map || return_links || depth < max_depth;
    if (need_links && !has_format(request.formats, "links")) {
      request.formats.push_back("links");
    } else if (!need_links && has_format(request.formats, "links")) {
      request.formats.pop_back(); // only the automatically appended links format
    }
    try {
      auto page = browser.scrape(request);
      const auto canonical =
          normalize_url(page.at("metadata").at("url").get_ref<const std::string &>());
      // A redirect may reach a URL already queued through a direct link. Track
      // final destinations so that URL is neither rendered nor returned twice.
      if (!visited.insert(canonical).second) {
        continue;
      }
      // Redirected pages are returned, but an external destination is never expanded.
      const bool expand = origin(canonical) == start_origin;
      if (tool == Tool::map && page.contains("warnings")) {
        warnings.push_back({{"url", canonical}, {"warnings", std::move(page["warnings"])}});
      }
      truncated = truncated || page.value("linksTruncated", false);
      if (const auto found = page.find("links"); found != page.end()) {
        for (const auto &link : *found) {
          std::string candidate;
          try {
            candidate = normalize_url(link.at("url").get_ref<const std::string &>());
          } catch (const std::invalid_argument &) {
            continue;
          }
          // Crawl only needs its frontier. Retaining a second map of every link wastes memory.
          if (tool == Tool::map && !discovered.contains(candidate)) {
            if (discovered.size() == max_links) {
              truncated = true;
            } else {
              discovered.insert(candidate);
              links.push_back({{"url", candidate}, {"title", link.value("title", "")}});
            }
          }
          if (expand && depth < max_depth && origin(candidate) == start_origin &&
              !queued.contains(candidate) && !visited.contains(candidate)) {
            if (queued.size() == max_links) {
              truncated = true;
            } else {
              queued.insert(candidate);
              queue.emplace_back(std::move(candidate), depth + 1);
            }
          }
        }
      }
      if (tool == Tool::crawl) {
        if (!return_links) {
          page.erase("links");
          page.erase("linksTruncated");
        }
        bytes += page.dump().size();
        if (bytes > max_crawl_bytes) {
          truncated = true;
          break;
        }
        pages.push_back(std::move(page));
      }
    } catch (const std::exception &e) {
      errors.push_back({{"url", url}, {"error", e.what()}});
    }
  }
  Json result{{"attempted", attempted}, {"truncated", truncated || !queue.empty()}};
  // Assignment moves the arrays; initializer-list construction would copy them.
  result["errors"] = std::move(errors);
  if (!warnings.empty()) {
    result["warnings"] = std::move(warnings);
  }
  result[tool == Tool::map ? "links" : "pages"] =
      tool == Tool::map ? std::move(links) : std::move(pages);
  return result;
}
} // namespace firepanda
