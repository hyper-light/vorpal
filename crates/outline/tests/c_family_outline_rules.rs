use vorpal_language::SupportLang;

#[allow(dead_code)]
mod common;

#[test]
fn csharp_rules_parse_and_extract_dotnet_shapes() {
  const RULES: &str = include_str!("../src/default_rules/csharp.yml");
  common::assert_outline_snapshot(
    SupportLang::CSharp,
    RULES,
    r#"
using System;
namespace Demo.Core;
public interface IService { void Run(); }
public class Parser { private int count; public Parser(int count) { this.count = count; } public string Parse(string input) { return input; } }
public enum Mode { Fast, Slow }
"#,
    r#"
- Module import private System
- Interface item exported IService
  - Method public Run
- Class item exported Parser
  - Field private count
  - Constructor public Parser
  - Method public Parse
- Enum item exported Mode
  - EnumMember public Fast
  - EnumMember public Slow
"#,
  );
}

#[test]
fn c_rules_parse_and_extract_native_shapes() {
  const RULES: &str = include_str!("../src/default_rules/c.yml");
  common::assert_outline_snapshot(
    SupportLang::C,
    RULES,
    r#"
#include <stdio.h>
typedef struct Config { int value; } Config;
enum Mode { Fast, Slow };
int count;
int helper(int value) { return value; }
"#,
    r#"
- Module import private <stdio.h>
- Struct item exported Config
  - Field public value
- Enum item exported Mode
  - EnumMember public Fast
  - EnumMember public Slow
- Variable item exported count
- Function item exported helper
"#,
  );
}

#[test]
fn cpp_rules_parse_and_extract_native_shapes() {
  const RULES: &str = include_str!("../src/default_rules/cpp.yml");
  common::assert_outline_snapshot(
    SupportLang::Cpp,
    RULES,
    r#"
#include <vector>
namespace demo {
class Parser { public: Parser(); int parse(const char* input); private: int count; };
struct Config { int value; };
enum Mode { Fast, Slow };
int helper(int value) { return value; }
}
"#,
    r#"
- Module import private <vector>
- Class item exported Parser
  - Constructor private Parser
  - Method private parse
  - Field private count
- Struct item exported Config
  - Field private value
- Enum item exported Mode
  - EnumMember public Fast
  - EnumMember public Slow
- Function item exported helper
"#,
  );
}

#[test]
fn c_declarator_shapes_extract_clean_names() {
  const RULES: &str = include_str!("../src/default_rules/c.yml");
  // Pointer/array/function-pointer declarators must never leak into names (`*allocation`),
  // pointer-returning function definitions must still be functions, initializers must not
  // ride along in variable names, prototypes must not become variables, and `struct` type
  // *references* must not mint phantom struct definitions.
  common::assert_outline_snapshot(
    SupportLang::C,
    RULES,
    r#"
struct pool {
  int plain;
  void *allocation;
  int buf[4];
  unsigned bits : 3;
  int (*cb)(int);
  struct pool *next;
};
int plain_fn(int a) { return a; }
void *ptr_fn(int a) { return 0; }
static struct pool *sptr_fn(void) { return 0; }
int global_plain;
int global_init = 3;
char *global_ptr;
int global_arr[8];
void *proto_fn(int);
"#,
    r#"
- Struct item exported pool
  - Field public plain
  - Field public allocation
  - Field public buf
  - Field public bits
  - Field public cb
  - Field public next
- Function item exported plain_fn
- Function item exported ptr_fn
- Function item private sptr_fn
- Variable item exported global_plain
- Variable item exported global_init
- Variable item exported global_ptr
- Variable item exported global_arr
"#,
  );
}

#[test]
fn cpp_declarator_shapes_extract_clean_names() {
  const RULES: &str = include_str!("../src/default_rules/cpp.yml");
  // Same declarator discipline for C++, plus the classification split: a pointer-returning
  // method declaration is a *method* (not a `*ptr_method(int x)` field), and a
  // function-pointer member is a *field* named `cb` (not a `(*cb)` method).
  common::assert_outline_snapshot(
    SupportLang::Cpp,
    RULES,
    r#"
class Widget {
public:
  int plain;
  char *cursor;
  int *ptr_method(int x);
  int plain_method(int x);
  int (*cb)(int);
};
void *free_ptr_fn() { return 0; }
int free_fn() { return 1; }
"#,
    r#"
- Class item exported Widget
  - Field private plain
  - Field private cursor
  - Method private ptr_method
  - Method private plain_method
  - Field private cb
- Function item exported free_ptr_fn
- Function item exported free_fn
"#,
  );
}
