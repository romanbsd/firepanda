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
constexpr size_t max_crawl_bytes = 4 * 1024 * 1024;
constexpr std::array<std::string_view, 5> formats{"markdown", "text", "html", "rawHtml", "links"};

enum class Tool { scrape, crawl, map };

// One row per tool: dispatch and tools/list both read this table.
struct ToolDef {
  Tool id;
  const char *name;
  const char *description;
};
constexpr std::array tool_table{
    ToolDef{Tool::scrape, "scrape",
            "Render a page with native Lightpanda and extract content. Markdown supports common "
            "article elements."},
    ToolDef{Tool::crawl, "crawl",
            "Breadth-first crawl of same-origin pages. Returns pages and per-page errors; bounded "
            "by depth, page count, 120 seconds, and result size."},
    ToolDef{Tool::map, "map",
            "Discover links with a bounded same-origin crawl. External links are listed but never "
            "followed. Not a sitemap or search engine."},
};

enum class OptionKind { integer, string, boolean, url, formats };

// One row per accepted argument. The advertised schema, validation bounds,
// defaults, and per-tool applicability all derive from these entries.
struct OptionDef {
  const char *name;
  OptionKind kind;
  const char *description;
  int fallback;        // integer/boolean default
  int minimum;         // integer minimum
  int maximum;         // integer maximum or string byte limit
  bool discovery_only; // accepted by crawl/map; rejected by scrape
};
constexpr std::array options{
    OptionDef{"url", OptionKind::url, "Absolute HTTP(S) URL without credentials", 0, 0, 0, false},
    OptionDef{"formats", OptionKind::formats, "Output formats to extract", 0, 0, 0, false},
    OptionDef{"onlyMainContent", OptionKind::boolean,
              "Remove common page chrome and prefer main/article content", 1, 0, 0, false},
    OptionDef{"timeout", OptionKind::integer,
              "Total per-page budget in milliseconds, including navigation and extraction", 30000,
              100, 120000, false},
    OptionDef{"waitFor", OptionKind::integer, "Extra wait after page load, within timeout", 0, 0,
              30000, false},
    OptionDef{"waitForSelector", OptionKind::string,
              "Wait for a CSS selector to match an element (presence, not visibility), within "
              "timeout.",
              0, 0, 2048, false},
    OptionDef{"waitForText", OptionKind::string,
              "Wait for literal case-sensitive body text outside scripts/styles/templates, within "
              "timeout.",
              0, 0, 4096, false},
    OptionDef{"limit", OptionKind::integer, "Maximum pages to attempt", 10, 1, 100, true},
    OptionDef{"maxDepth", OptionKind::integer, "Maximum link depth from the start URL", 1, 0, 5,
              true},
    OptionDef{"maxConcurrency", OptionKind::integer,
              "Pages fetched in flight at once during this crawl", 4, 1, 16, true},
};

const OptionDef *find_option(std::string_view name) {
  const auto found = std::find_if(options.begin(), options.end(),
                                  [&](const OptionDef &option) { return name == option.name; });
  return found == options.end() ? nullptr : &*found;
}
const OptionDef &known_option(std::string_view name) {
  // Options are registered statically; a miss is a programming error.
  if (const auto *option = find_option(name)) {
    return *option;
  }
  throw std::logic_error("Unregistered option: " + std::string(name));
}

int parse_integer(const Json &args, std::string_view name) {
  const auto &option = known_option(name);
  const auto it = args.find(option.name);
  if (it == args.end()) {
    return option.fallback;
  }
  if (!it->is_number_integer()) {
    throw std::invalid_argument(std::string(name) + " must be an integer");
  }
  // Compare as JSON before narrowing, including unsigned integers above INT64_MAX.
  if (*it < option.minimum || *it > option.maximum) {
    throw std::invalid_argument(std::string(name) + " is outside the supported range");
  }
  return it->get<int>();
}

std::string parse_string(const Json &args, std::string_view name) {
  const auto &option = known_option(name);
  const auto it = args.find(option.name);
  if (it == args.end()) {
    return {};
  }
  if (!it->is_string() || it->get_ref<const std::string &>().empty() ||
      it->get_ref<const std::string &>().size() > static_cast<size_t>(option.maximum)) {
    throw std::invalid_argument(std::string(name) +
                                " must be a nonempty string within its byte limit");
  }
  return it->get<std::string>();
}

bool parse_boolean(const Json &args, std::string_view name) {
  const auto &option = known_option(name);
  const auto it = args.find(option.name);
  if (it == args.end()) {
    return option.fallback != 0;
  }
  if (!it->is_boolean()) {
    throw std::invalid_argument(std::string(name) + " must be boolean");
  }
  return it->get<bool>();
}

Json schema_of(const OptionDef &option) {
  switch (option.kind) {
  case OptionKind::integer:
    return {{"type", "integer"},
            {"minimum", option.minimum},
            {"maximum", option.maximum},
            {"default", option.fallback},
            {"description", option.description}};
  case OptionKind::string:
    // JSON Schema lengths count characters; validation also enforces UTF-8 bytes.
    return {{"type", "string"},
            {"minLength", 1},
            {"maxLength", option.maximum},
            {"description", std::string(option.description) + " Maximum " +
                                std::to_string(option.maximum) + " UTF-8 bytes."}};
  case OptionKind::boolean:
    return {{"type", "boolean"},
            {"default", option.fallback != 0},
            {"description", option.description}};
  case OptionKind::url:
    return {{"type", "string"}, {"description", option.description}};
  case OptionKind::formats:
    return {{"type", "array"},
            {"minItems", 1},
            {"items", {{"type", "string"}, {"enum", formats}}},
            {"default", {"markdown"}},
            {"description", option.description}};
  }
  throw std::logic_error("Unhandled option kind");
}

Tool parse_tool(std::string_view name) {
  for (const auto &entry : tool_table) {
    if (name == entry.name) {
      return entry.id;
    }
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
  for (const auto &[key, value] : args.items()) {
    (void)value;
    const auto *known = find_option(key);
    if (!known || (known->discovery_only && tool == Tool::scrape)) {
      throw std::invalid_argument("Unknown argument: " + key);
    }
  }
  if (!args.contains("url") || !args["url"].is_string()) {
    throw std::invalid_argument("url must be a string");
  }
  PageRequest request;
  request.url = normalize_url(args["url"].get_ref<const std::string &>());
  request.only_main_content = parse_boolean(args, "onlyMainContent");
  request.timeout = std::chrono::milliseconds(parse_integer(args, "timeout"));
  request.wait_for = std::chrono::milliseconds(parse_integer(args, "waitFor"));
  request.wait_for_selector = parse_string(args, "waitForSelector");
  request.wait_for_text = parse_string(args, "waitForText");
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
  for (const auto &entry : tool_table) {
    Json properties = Json::object();
    for (const auto &option : options) {
      if (option.discovery_only && entry.id == Tool::scrape) {
        continue;
      }
      properties[option.name] = schema_of(option);
    }
    list.push_back(
        {{"name", entry.name},
         {"description", entry.description},
         {"inputSchema",
          {{"type", "object"},
           {"properties", std::move(properties)},
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
  const int limit = parse_integer(args, "limit"), max_depth = parse_integer(args, "maxDepth");
  const int concurrency = parse_integer(args, "maxConcurrency");
  browser.set_parallelism(static_cast<size_t>(concurrency));
  const std::string start_origin(origin(request.url));
  const auto deadline = Clock::now() + 120s;
  const auto page_timeout = request.timeout;
  // Each page derives its formats from this immutable base instead of mutating
  // one shared vector with paired push/pop calls.
  const std::vector<std::string> page_formats =
      tool == Tool::map ? std::vector<std::string>{"links"} : request.formats;
  const bool return_links = has_format(page_formats, "links");
  std::unordered_set<std::string> queued{request.url}, discovered, visited;
  // Set keys own frontier URLs for the whole crawl. Rehash preserves pointers
  // (not iterators), and keys are never erased, so the queue needs no second copy.
  std::deque<std::pair<const std::string *, int>> queue{{&*queued.begin(), 0}};
  Json pages = Json::array(), links = Json::array(), errors = Json::array(),
       warnings = Json::array();
  int attempted = 0;
  size_t bytes = 0;
  bool truncated = false;
  // A bound was reached: stop dispatching. Pages already in flight are drained.
  bool exhausted = false;
  // The serialization budget overflowed: later pages are dropped even though
  // they were fetched, matching the sequential crawl that stops at the overflow.
  bool budget_reached = false;

  // One dispatched page; its result arrives through the future.
  struct Inflight {
    std::future<Json> page;
    const std::string *url;
    int depth;
  };
  std::deque<Inflight> inflight;
  // Wait for any page still in flight on an early exit so browser teardown can
  // never race a page worker. Pool futures do not block on destruction, but a
  // running task must still finish before release() drops its connection.
  struct Drain {
    std::deque<Inflight> &pending;
    ~Drain() {
      for (auto &item : pending) {
        if (item.page.valid()) {
          item.page.wait();
        }
      }
    }
  } drain{inflight};

  // Fold one successful page into the result exactly where the sequential crawl
  // would have: consumption runs in dispatch order, so pages, links, and errors
  // keep their breadth-first order even though fetches overlap.
  auto record = [&](int depth, Json page) {
    if (budget_reached) {
      return; // Sequential stops at the first overflow and never reaches these.
    }
    const auto canonical =
        normalize_url(page.at("metadata").at("url").get_ref<const std::string &>());
    // A redirect may reach a URL already queued through a direct link. Track
    // final destinations so that URL is neither rendered nor returned twice.
    if (!visited.insert(canonical).second) {
      return;
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
            const auto [entry, inserted] = queued.insert(std::move(candidate));
            if (inserted) {
              queue.emplace_back(&*entry, depth + 1);
            }
          }
        }
      }
    }
    if (tool == Tool::crawl) {
      if (!return_links) {
        page.erase("links");
        page.erase("linksTruncated");
      }
      const auto size = serialized_size_within(page, max_crawl_bytes - bytes);
      if (!size) {
        truncated = true;
        budget_reached = true;
        exhausted = true;
        return;
      }
      bytes += *size;
      pages.push_back(std::move(page));
    }
  };

  while (!stop_requested && (!inflight.empty() || (!queue.empty() && !exhausted))) {
    // Fill the window in frontier order. Connections are checked out on this
    // thread, so the managed browser is spawned exactly once.
    while (!stop_requested && !exhausted && inflight.size() < static_cast<size_t>(concurrency) &&
           !queue.empty()) {
      const auto [pending, depth] = queue.front();
      queue.pop_front();
      const auto &url = *pending;
      if (visited.contains(url)) {
        continue;
      }
      const auto remaining =
          std::chrono::duration_cast<std::chrono::milliseconds>(deadline - Clock::now());
      if (attempted == limit || remaining < 100ms) {
        truncated = true;
        exhausted = true;
        break;
      }
      ++attempted;
      request.url = url;
      request.timeout = std::min(page_timeout, remaining);
      // Leaf pages need no frontier links unless the caller requested them.
      request.formats = page_formats;
      if ((return_links || depth < max_depth) && !has_format(request.formats, "links")) {
        request.formats.push_back("links");
      }
      inflight.push_back({browser.scrape_async(request), pending, depth});
    }
    if (inflight.empty()) {
      break;
    }
    // Consume the oldest dispatch first: results keep sequential BFS order.
    Inflight item = std::move(inflight.front());
    inflight.pop_front();
    const std::string url = *item.url;
    Json page;
    try {
      page = item.page.get();
    } catch (const std::exception &e) {
      errors.push_back({{"url", url}, {"error", e.what()}});
      // A failed fetch invalidates its connection: pause dispatch, drain the
      // siblings while keeping their results, then reap and relaunch the
      // browser before anything else is fetched.
      while (!inflight.empty()) {
        Inflight sibling = std::move(inflight.front());
        inflight.pop_front();
        try {
          Json sibling_page = sibling.page.get();
          record(sibling.depth, std::move(sibling_page));
        } catch (const std::exception &sibling_error) {
          errors.push_back({{"url", *sibling.url}, {"error", sibling_error.what()}});
        }
      }
      browser.restart();
      continue;
    }
    browser.page_completed(); // Idle time counts from the fetch, not from recording.
    try {
      record(item.depth, std::move(page));
    } catch (const std::exception &e) {
      // Recording failures come from a healthy page; the browser stays up.
      errors.push_back({{"url", url}, {"error", e.what()}});
    }
  }
  Json result{{"attempted", attempted}, {"truncated", truncated || !queue.empty()}};
  // Transfer the accumulated arrays into the result.
  result["errors"] = std::move(errors);
  if (!warnings.empty()) {
    result["warnings"] = std::move(warnings);
  }
  result[tool == Tool::map ? "links" : "pages"] =
      tool == Tool::map ? std::move(links) : std::move(pages);
  return result;
}
} // namespace firepanda
