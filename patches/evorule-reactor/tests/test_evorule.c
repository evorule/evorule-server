/**
 * @file test_evorule.c
 * @brief evorule-reactor C FFI 接口测试程序
 *
 * 编译命令（Windows MSVC）：
 *   cl /I..\include test_evorule.c /link
 * ..\..\target\release\evorule_reactor.dll.lib
 *
 * 编译命令（Windows MinGW）：
 *   gcc -I../include test_evorule.c -L../../target/release -levorule_reactor -o
 * test_evorule.exe
 *
 * 编译命令（Linux/macOS）：
 *   gcc -I../include test_evorule.c -L../../target/release -levorule_reactor -o
 * test_evorule
 *
 * 运行前确保动态库在搜索路径中：
 *   Windows: set PATH=%PATH%;..\..\target\release
 *   Linux: export LD_LIBRARY_PATH=../../target/release:$LD_LIBRARY_PATH
 *   macOS: export DYLD_LIBRARY_PATH=../../target/release:$DYLD_LIBRARY_PATH
 */

#include "../include/evorule.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define TEST_PASS(msg) printf("[PASS] %s\n", msg)
#define TEST_FAIL(msg) printf("[FAIL] %s\n", msg)
#define ASSERT_TRUE(cond, msg)                                                 \
  do {                                                                         \
    if (cond) {                                                                \
      TEST_PASS(msg);                                                          \
    } else {                                                                   \
      TEST_FAIL(msg);                                                          \
      return 1;                                                                \
    }                                                                          \
  } while (0)

int test_version(void) {
  printf("\n=== Test: evorule_version ===\n");

  const char *version = evorule_version();
  ASSERT_TRUE(version != NULL, "version() returns non-null");
  ASSERT_TRUE(strlen(version) > 0, "version() returns non-empty string");

  printf("Library version: %s\n", version);
  return 0;
}

int test_reactor_lifecycle(void) {
  printf("\n=== Test: Reactor Lifecycle ===\n");

  evorule_reactor *reactor = evorule_reactor_new();
  ASSERT_TRUE(reactor != NULL, "reactor_new() returns non-null");

  evorule_reactor_free(reactor);
  TEST_PASS("reactor_free() succeeds");

  return 0;
}

int test_send_command(void) {
  printf("\n=== Test: Send Command ===\n");

  evorule_reactor *reactor = evorule_reactor_new();
  ASSERT_TRUE(reactor != NULL, "reactor_new() returns non-null");

  evorule_error_code_t err = evorule_reactor_send_command(
      reactor,
      "{\"type\": \"increment\", \"params\": {\"attr\": \"x\", \"delta\": 5}}");
  ASSERT_TRUE(err == EVORULE_OK, "send_command() returns EVORULE_OK");

  int queue_size = evorule_reactor_current_queue_size(reactor);
  ASSERT_TRUE(queue_size >= 0, "current_queue_size() returns valid value");
  printf("Queue size after send: %d\n", queue_size);

  evorule_reactor_free(reactor);
  TEST_PASS("reactor_free() after command");

  return 0;
}

int test_null_handling(void) {
  printf("\n=== Test: Null Handling ===\n");

  evorule_error_code_t err = evorule_reactor_send_command(NULL, "test");
  ASSERT_TRUE(err == EVORULE_ERROR_INVALID_ARG,
              "send_command(null) returns INVALID_ARG");

  int val = evorule_reactor_current_queue_size(NULL);
  ASSERT_TRUE(val == -1, "current_queue_size(null) returns -1");

  evorule_reactor_free(NULL);
  TEST_PASS("free(null) does not crash");

  return 0;
}

int test_multiple_commands(void) {
  printf("\n=== Test: Multiple Commands ===\n");

  evorule_reactor *reactor = evorule_reactor_new();
  ASSERT_TRUE(reactor != NULL, "reactor_new() returns non-null");

  const char *commands[] = {
      "{\"type\": \"increment\", \"params\": {\"attr\": \"x\", \"delta\": 1}}",
      "{\"type\": \"increment\", \"params\": {\"attr\": \"x\", \"delta\": 2}}",
      "{\"type\": \"increment\", \"params\": {\"attr\": \"x\", \"delta\": 3}}",
      NULL};

  for (int i = 0; commands[i] != NULL; i++) {
    evorule_error_code_t err =
        evorule_reactor_send_command(reactor, commands[i]);
    ASSERT_TRUE(err == EVORULE_OK, "send_command() succeeds");
  }

  int queue_size = evorule_reactor_current_queue_size(reactor);
  printf("Queue size after 3 commands: %d\n", queue_size);

  evorule_reactor_free(reactor);
  TEST_PASS("reactor_free() after multiple commands");

  return 0;
}

int main(void) {
  printf("========================================\n");
  printf(" evorule C FFI Test Suite\n");
  printf("========================================\n");

  int failed = 0;

  failed += test_version();
  failed += test_reactor_lifecycle();
  failed += test_send_command();
  failed += test_null_handling();
  failed += test_multiple_commands();

  printf("\n========================================\n");
  if (failed == 0) {
    printf(" All tests PASSED\n");
  } else {
    printf(" %d test(s) FAILED\n", failed);
  }
  printf("========================================\n");

  return failed;
}