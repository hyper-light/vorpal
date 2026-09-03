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
- Module item exported Demo.Core
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
- Module item exported demo
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


#[test]
fn c_struct_definition_with_trailing_declarator_mints_the_type() {
  // `struct X { ... } tail;` is first and foremost a TYPE definition — the kernel's
  // `__randomize_layout` / `__packed` idiom parses exactly like a variable declaration
  // with an inline struct type, and the variable item used to swallow the subtree so the
  // struct never existed (file_operations, cpuinfo_x86 at kernel scale). Named+body
  // specifiers win; anonymous bodies and body-less type references keep their variables.
  const RULES: &str = include_str!("../src/default_rules/c.yml");
  common::assert_outline_snapshot(
    SupportLang::C,
    RULES,
    r#"
struct file_operations { int owner; } __randomize_layout;
union addr { int v4; } __packed;
struct { int x; } anonymous_var;
struct forward_only decl_var;
"#,
    r#"
- Struct item exported file_operations
  - Field public owner
- Union item exported addr
  - Field public v4
- Variable item exported anonymous_var
- Variable item exported decl_var
"#,
  );
}

/// The parser-swallow shape (cpython `Objects/object.c` from `_PyObject_GetAttrId`):
/// bare statement-position macros wreck a body, tree-sitter loses the closing brace and
/// parses every later definition INSIDE that body with no top-level ERROR. The recovery
/// walk must lift them all, keep the swallower's locals out, cut the swallower's span
/// back to its real body, and report the recovery.
#[test]
fn c_swallowed_tail_definitions_are_lifted_as_items() {
  use vorpal_language::LanguageExt as _;
  use vorpal_outline::{DEFAULT_OUTLINE_RULES, combined_extractor::CombinedExtractors, extractor::parse_outline_rules};
  let rules = parse_outline_rules::<SupportLang>(DEFAULT_OUTLINE_RULES)
    .expect("rules parse")
    .into_iter()
    .filter(|r| r.common().language == SupportLang::C)
    .collect::<Vec<_>>();
  let combined = CombinedExtractors::try_from(rules, &Default::default()).expect("rules compile");
  let source = r#"#include "Python.h"

static int counter = 0;

int
before_swallow(int x)
{
    return x + counter;
}

PyObject *
_PyObject_GetAttrId(PyObject *v, _Py_Identifier *name)
{
    PyObject *result;
_Py_COMP_DIAG_PUSH
_Py_COMP_DIAG_IGNORE_DEPR_DECLS
    PyObject *oname = _PyUnicode_FromId(name); /* borrowed */
_Py_COMP_DIAG_POP
    if (!oname)
        return NULL;
    result = PyObject_GetAttr(v, oname);
    return result;
}

int
_PyObject_SetAttributeErrorContext(PyObject* v, PyObject* name)
{
    assert(PyErr_Occurred());
    return 0;
}

PyObject *
PyObject_GetAttr(PyObject *v, PyObject *name)
{
    PyTypeObject *tp = Py_TYPE(v);
    return NULL;
}

#define SWALLOWED_MACRO(x) ((x) + 1)

struct swallowed_record {
    int field;
};

typedef struct swallowed_record swallowed_t;

static PyNumberMethods none_as_number = {
    0,
};

PyObject _Py_NoneStruct = _PyObject_HEAD_INIT(&_PyNone_Type);

int
PyCallable_Check(PyObject *x)
{
    int local_in_lifted = 0;
    if (x == NULL)
        return 0;
    return local_in_lifted;
}
"#;
  let grep = SupportLang::C.grep(source);
  assert!(grep.root().has_error(), "the fixture must carry the parse damage it models");
  let mut report = Vec::new();
  let items = combined.extract_with(grep.root(), &mut report).collect::<Vec<_>>();
  let names: Vec<&str> = items.iter().map(|i| i.entry.name.as_ref()).collect();
  assert_eq!(
    names,
    vec![
      "\"Python.h\"",
      "counter",
      "before_swallow",
      "_PyObject_GetAttrId",
      "PyObject_GetAttr",
      "SWALLOWED_MACRO",
      "swallowed_record",
      "swallowed_t",
      "none_as_number",
      "_Py_NoneStruct",
      "PyCallable_Check",
    ],
    "lifted in document order; locals (`result`, `oname`, `local_in_lifted`) and the \
     wreckage blob's `if` never surface"
  );
  // The swallower's span is cut back to its real body: it ends before the floor, which is
  // the first clean nested definition (`PyObject_GetAttr`) — the keyword-named fusion
  // blob (`_Py_COMP_DIAG_POP if (!oname) … { <next function's body> }`) is neither the
  // floor nor an item, so the swallower's span runs through it.
  let swallower = &items[3];
  let floor = source.find("PyObject *\nPyObject_GetAttr(").expect("floor text");
  assert!(
    swallower.entry.range.byte_offset.end <= floor,
    "swallower must not span to EOF: ends at {} (floor {floor})",
    swallower.entry.range.byte_offset.end
  );
  // Zero-based lines: the `PyObject *` return-type line through the closing brace the
  // parser fused into the blob (the real `_PyObject_SetAttributeErrorContext` body's).
  assert_eq!(swallower.entry.range.start.line, 10);
  assert_eq!(swallower.entry.range.end.line, 29);
  // Lifted items carry their real spans.
  let get_attr = &items[4];
  let get_attr_start = source.find("PyObject *\nPyObject_GetAttr(").expect("def text");
  assert_eq!(get_attr.entry.range.byte_offset.start, get_attr_start);
  assert_eq!(items[6].members.len(), 1, "a lifted struct keeps its members");
  assert_eq!(
    report,
    vec![vorpal_outline::model::SwallowRecovery {
      start: swallower.entry.range.byte_offset.start as u32,
      lifted: 7,
    }]
  );
}

/// The kernel shapes: a macro-with-block idiom (`scoped_guard(x) { … }`) fuses the
/// swallower's tail into a function-shaped blob; `for_each_*(x) { }` loops parse as
/// nested function definitions with a parenthesized declarator. Neither may be lifted or
/// become a false floor; the swallower's span ends at the blob (its real closing brace).
#[test]
fn c_swallow_recovery_ignores_macro_block_wreckage() {
  use vorpal_language::LanguageExt as _;
  use vorpal_outline::{DEFAULT_OUTLINE_RULES, combined_extractor::CombinedExtractors, extractor::parse_outline_rules};
  let rules = parse_outline_rules::<SupportLang>(DEFAULT_OUTLINE_RULES)
    .expect("rules parse")
    .into_iter()
    .filter(|r| r.common().language == SupportLang::C)
    .collect::<Vec<_>>();
  let combined = CombinedExtractors::try_from(rules, &Default::default()).expect("rules compile");
  let source = r#"void clock_was_set(unsigned int bases)
{
	cpumask_var_t mask;

	for_each_online_cpu(cpu) {
		struct hrtimer_cpu_base *cpu_base = &per_cpu(hrtimer_bases, cpu);
		cpumask_set_cpu(cpu, mask);
	}
	scoped_guard(cpus_read_lock) {
		int cpu;

		scoped_guard(preempt)
			smp_call_function_many(mask, retrigger_next_event, NULL, 1);
	}
	free_cpumask_var(mask);

out_timerfd:
	timerfd_clock_was_set();
}

static void clock_was_set_work(struct work_struct *work)
{
	clock_was_set(CLOCK_SET_WALL);
}

static DECLARE_WORK(hrtimer_work, clock_was_set_work);

void hrtimer_start_range_ns(struct hrtimer *timer, ktime_t tim)
{
	int local = 0;
}
"#;
  let grep = SupportLang::C.grep(source);
  assert!(grep.root().has_error());
  let mut report = Vec::new();
  let items = combined.extract_with(grep.root(), &mut report).collect::<Vec<_>>();
  let names: Vec<&str> = items.iter().map(|i| i.entry.name.as_ref()).collect();
  // The empty-named variable is `static DECLARE_WORK(…);` — parity with the ordinary
  // top-level traversal, which mints the same item for it (a MISSING identifier).
  assert_eq!(
    names,
    vec!["clock_was_set", "clock_was_set_work", "", "hrtimer_start_range_ns"],
    "no `cpu`, `cpu_base`, `mask`, `scoped_guard`, `for_each_online_cpu`, or `local`"
  );
  assert_eq!(report.len(), 1);
  assert_eq!(report[0].lifted, 3);
  let end = items[0].entry.range.byte_offset.end;
  assert_eq!(
    &source[..end].trim_end().lines().last().unwrap_or(""),
    &"}",
    "the swallower ends at its real closing brace"
  );
}

/// No swallow, no change: the file's last definition carrying an internal error but no
/// nested definition is extracted exactly as before (the diagnosis needs a floor).
#[test]
fn c_last_definition_with_body_damage_is_not_a_swallow() {
  use vorpal_language::LanguageExt as _;
  use vorpal_outline::{DEFAULT_OUTLINE_RULES, combined_extractor::CombinedExtractors, extractor::parse_outline_rules};
  let rules = parse_outline_rules::<SupportLang>(DEFAULT_OUTLINE_RULES)
    .expect("rules parse")
    .into_iter()
    .filter(|r| r.common().language == SupportLang::C)
    .collect::<Vec<_>>();
  let combined = CombinedExtractors::try_from(rules, &Default::default()).expect("rules compile");
  let source = "int a;\nvoid last(void)\n{\n\tint x = ;\n\tstruct local_only { int f; } v;\n}\n";
  let grep = SupportLang::C.grep(source);
  assert!(grep.root().has_error());
  let mut report = Vec::new();
  let items = combined.extract_with(grep.root(), &mut report).collect::<Vec<_>>();
  let names: Vec<&str> = items.iter().map(|i| i.entry.name.as_ref()).collect();
  assert_eq!(names, vec!["a", "last"]);
  assert!(report.is_empty());
  assert_eq!(items[1].entry.range.byte_offset.end, source.trim_end().len());
}
