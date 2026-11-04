cmake_minimum_required(VERSION 3.24)
project(firepanda VERSION 0.2.0 LANGUAGES CXX)
if(NOT UNIX)
  message(FATAL_ERROR "Firepanda supports macOS and Linux")
endif()
include(FetchContent)
include(CTest)
find_package(Threads REQUIRED)

# Header-only dependencies, pinned and hash checked. TLS and compression belong
# to Lightpanda; the local CDP connection needs neither.
FetchContent_Declare(httplib
  URL https://github.com/yhirose/cpp-httplib/archive/453b1b8613a591a16d0291721a7ebdd64d4b01e9.tar.gz
  URL_HASH SHA256=b16beb3739ec4bbb5d245cf6be73bc020980ea4ae70cab96e99a1977948bb917)
set(HTTPLIB_REQUIRE_OPENSSL OFF CACHE BOOL "" FORCE)
set(HTTPLIB_USE_OPENSSL_IF_AVAILABLE OFF CACHE BOOL "" FORCE)
set(HTTPLIB_USE_ZLIB_IF_AVAILABLE OFF CACHE BOOL "" FORCE)
set(HTTPLIB_USE_BROTLI_IF_AVAILABLE OFF CACHE BOOL "" FORCE)
set(HTTPLIB_USE_ZSTD_IF_AVAILABLE OFF CACHE BOOL "" FORCE)
set(HTTPLIB_INSTALL OFF CACHE BOOL "" FORCE)
FetchContent_Declare(json
  URL https://github.com/nlohmann/json/releases/download/v3.12.0/json.tar.xz
  URL_HASH SHA256=42f6e95cad6ec532fd372391373363b62a14af6d771056dbfc86160e6dfff7aa)
set(JSON_BuildTests OFF CACHE BOOL "" FORCE)
set(JSON_Install OFF CACHE BOOL "" FORCE)
FetchContent_MakeAvailable(httplib json)

file(READ "${CMAKE_CURRENT_SOURCE_DIR}/native/extract.js" FIREPANDA_EXTRACTION_JS)
configure_file(native/extraction.hpp.in generated/extraction.hpp @ONLY)
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS native/extract.js)
add_executable(firepanda native/main.cpp native/browser.cpp native/tools.cpp)
target_compile_features(firepanda PRIVATE cxx_std_20)
target_include_directories(firepanda PRIVATE "${CMAKE_CURRENT_BINARY_DIR}/generated")
target_link_libraries(firepanda PRIVATE httplib::httplib nlohmann_json::nlohmann_json Threads::Threads)
target_compile_options(firepanda PRIVATE -Wall -Wextra -Wpedantic)
install(TARGETS firepanda RUNTIME DESTINATION bin)

option(FIREPANDA_SANITIZERS "Enable address/undefined-behavior sanitizers (development only)" OFF)
if(FIREPANDA_SANITIZERS)
  if(NOT CMAKE_CXX_COMPILER_ID MATCHES "Clang|GNU")
    message(FATAL_ERROR "Sanitizers require Clang or GCC")
  endif()
  target_compile_options(firepanda PRIVATE -fsanitize=address,undefined -fno-sanitize-recover=all -fno-omit-frame-pointer)
  target_link_options(firepanda PRIVATE -fsanitize=address,undefined)
endif()

find_program(CLANG_FORMAT NAMES clang-format HINTS /opt/homebrew/opt/llvm/bin)
if(CLANG_FORMAT)
  set(FIREPANDA_CPP native/main.cpp native/browser.cpp native/browser.hpp native/tools.cpp native/json.hpp)
  add_custom_target(format COMMAND "${CLANG_FORMAT}" -i ${FIREPANDA_CPP}
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}" VERBATIM)
  add_custom_target(format-check COMMAND "${CLANG_FORMAT}" --dry-run --Werror ${FIREPANDA_CPP}
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}" VERBATIM)
endif()

if(BUILD_TESTING)
  find_package(Python3 COMPONENTS Interpreter REQUIRED)
  add_test(NAME protocol COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_SOURCE_DIR}/tests/protocol.py" $<TARGET_FILE:firepanda>)
  set_tests_properties(protocol PROPERTIES TIMEOUT 30)
  add_test(NAME cdp_transport COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_SOURCE_DIR}/tests/transport.py" $<TARGET_FILE:firepanda>)
  set_tests_properties(cdp_transport PROPERTIES TIMEOUT 45)
  set(FIREPANDA_TEST_LIGHTPANDA "" CACHE FILEPATH "Native Lightpanda binary for integration tests")
  if(FIREPANDA_TEST_LIGHTPANDA)
    add_test(NAME native_browser COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_SOURCE_DIR}/tests/integration.py" $<TARGET_FILE:firepanda> "${FIREPANDA_TEST_LIGHTPANDA}")
    set_tests_properties(native_browser PROPERTIES TIMEOUT 180)
    add_test(NAME memory_stress COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_SOURCE_DIR}/tests/memory.py" $<TARGET_FILE:firepanda> "${FIREPANDA_TEST_LIGHTPANDA}")
    set_tests_properties(memory_stress PROPERTIES TIMEOUT 120)
    if(APPLE AND NOT FIREPANDA_SANITIZERS)
      add_custom_target(memory-check
        COMMAND "${Python3_EXECUTABLE}" "${CMAKE_CURRENT_SOURCE_DIR}/tests/memory.py" $<TARGET_FILE:firepanda> "${FIREPANDA_TEST_LIGHTPANDA}"
          --leaks --report "${CMAKE_CURRENT_BINARY_DIR}/evidence/macos-leaks.txt"
        DEPENDS firepanda USES_TERMINAL VERBATIM)
    endif()
  endif()
endif()
