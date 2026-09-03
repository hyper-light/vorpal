# Evidence for `xtask/labels/cpython.json`

Corpus: CPython at `b86a41cbf63` (main). Paths are relative to the checkout; `file:line`
is the definition, the quote is verbatim from the docstring / `Doc/c-api` entry / source
comment that proves the grade-3 symbol is what the query means. Grading is from the
source only — never from what search returns. Paraphrase queries share no token with the
identifier (or its file name); descriptive queries share at most one. `path` is recorded
wherever a name is defined in more than one file (the harness matches it with
`ends_with`); for Python methods the indexed name is the bare method name.

Known caveats carried from the original set (queries kept verbatim): `gc_collect_main`
and `PyGC_Collect` are defined in both `Python/gc.c` and `Python/gc_free_threading.c`;
`list_append` exists as the clinic wrapper (`Objects/clinic/listobject.c.h`) and a
`Modules/_testlimitedcapi/list.c` helper — the labels carry no `path`, so either
definition scores.

## original set (graded 2026-08-30)

- **short-keyword** "garbage collect run" → `gc_collect_main` (grade 3): Python/gc.c:1423 — comment at :1420 "/* This is the main function.  Read this to understand how the * collection process works. */". [grade 2 `PyGC_Collect` — the public entry that calls it]
- **descriptive** "bytecode evaluation loop" → `_PyEval_EvalFrameDefault` (grade 3): Python/ceval.c:1229 — "PyObject* _Py_HOT_FUNCTION DONT_SLP_VECTORIZE _PyEval_EvalFrameDefault(PyThreadState *tstate, _PyInterpreterFrame *frame, int throwflag)" — the interpreter's main loop (`Python/ceval.c`).
- **descriptive** "parse function arguments tuple" → `PyArg_ParseTuple` (grade 3): Python/getargs.c:103 — Doc/c-api/arg.rst:445 "Parse the parameters of a function that takes only positional parameters into local variables." [grade 2 `vgetargs1` — the worker it forwards to]
- **descriptive** "append item to list" → `PyList_Append` (grade 3): Objects/listobject.c:539 — Doc/c-api/list.rst:145 "Append the object *item* at the end of list *list*." [grade 2 `list_append` — the `list.append` method]
- **descriptive** "dictionary insert entry" → `insertdict` (grade 3): Objects/dictobject.c:2019 — comment at :2013 "Internal routine to insert a new item into the table. Used both by the internal resize routine and by the public insert routine."
- **short-keyword** "import find spec" → `_find_spec` (grade 3): Lib/importlib/_bootstrap.py:1192 — docstring :1193 "Find a module's spec." [grade 2 `find_spec` — the finder-protocol method]

## note on dropped candidates (2026-09-03)

`PyObject_GenericGetAttr` (Objects/object.c:2010) and `PyCallable_Check`
(Objects/object.c:2178) are defined in source but ABSENT from the index at this
commit (object.c is only partially extracted — see the report); they were dropped
rather than labelled, because the existence gate would fail the run.


## exact (added 2026-09-03)

- **exact** "PyUnicode_FromFormat" → `PyUnicode_FromFormat` (grade 3): Objects/unicodeobject.c:3127 — "PyUnicode_FromFormat(const char *format, ...)" [Doc/c-api/unicode.rst:440 "Take a C :c:func:`printf`\ -style *format* string and a variable number of arguments, calculate the size of the resulting Python Unicode string"]
- **exact** "PyLong_AsLongAndOverflow" → `PyLong_AsLongAndOverflow` (grade 3): Objects/longobject.c:593 — "PyLong_AsLongAndOverflow(PyObject *vv, int *overflow)" [Doc/c-api/long.rst:215 "Return a C :c:expr:`long` representation of *obj*."]
- **exact** "PyErr_WarnEx" → `PyErr_WarnEx` (grade 3): Python/_warnings.c:1402 — "PyErr_WarnEx(PyObject *category, const char *text, Py_ssize_t stack_level)" [Doc/c-api/exceptions.rst:377 "Issue a warning message."]
- **exact** "PyModule_AddObjectRef" → `PyModule_AddObjectRef` (grade 3): Python/modsupport.c:602 — "PyModule_AddObjectRef(PyObject *mod, const char *name, PyObject *value)" [Doc/c-api/module.rst:926 "Add an object to *module* as *name*."]
- **exact** "cached_property" → `cached_property` (grade 3): Lib/functools.py:1142 — "class cached_property:" [Doc/library/functools.rst:62 "Transform a method of a class into a property whose value is computed once and then cached as a normal attribute for the life of the instance."]
- **exact** "_find_and_load" → `_find_and_load` (grade 3): Lib/importlib/_bootstrap.py:1327 — "def _find_and_load(name, import_, *, lazy_submodule=False):" / :1328 "Find and load the module."
- **exact** "PathFinder" → `PathFinder` (grade 3): Lib/importlib/_bootstrap_external.py:1176 — "class PathFinder:" / :1178 "Meta path finder for sys.path and package __path__ attributes."
- **exact** "getmembers" → `getmembers` (grade 3, path Lib/inspect.py because Lib/tarfile.py:2182 also defines a `getmembers` method): Lib/inspect.py:524 — "def getmembers(object, predicate=None):" / :525 "Return all members of an object as (name, value) pairs sorted by name."

## short-keyword (added 2026-09-03)

- **short-keyword** "unicode concat" → `PyUnicode_Concat` (grade 3): Objects/unicodeobject.c:11680 — "PyUnicode_Concat(PyObject *left, PyObject *right)" [Doc/c-api/unicode.rst:1523 "Concat two strings giving a new Unicode string."]
- **short-keyword** "integer from string" → `PyLong_FromString` (grade 3): Objects/longobject.c:3052 — "PyLong_FromString(const char *str, char **pend, int base)" [Doc/c-api/long.rst:108 "Return a new :c:type:`PyLongObject` based on the string value in *str*, which is interpreted according to the radix in *base*"]
- **short-keyword** "allocate new tuple" → `PyTuple_New` (grade 3): Objects/tupleobject.c:75 — "PyTuple_New(Py_ssize_t size)" [Doc/c-api/tuple.rst:36 "Return a new tuple object of size *len*,"]. PyTuple_Pack grade 2: Doc/c-api/tuple.rst:55 "Return a new tuple object of size *n*" (allocates + fills).
- **short-keyword** "error set string" → `PyErr_SetString` (grade 3): Python/errors.c:303 — "PyErr_SetString(PyObject *exception, const char *string)" [Doc/c-api/exceptions.rst:132 "This is the most common way to set the error indicator."]. PyErr_Format grade 2 (same op with a format string, exceptions.rst:147).
- **short-keyword** "merge mapping into dict" → `PyDict_Merge` (grade 3): Objects/dictobject.c:4427 — "PyDict_Merge(PyObject *a, PyObject *b, int override)" [Doc/c-api/dict.rst:458 "Iterate over mapping object *b* adding key-value pairs to dictionary *a*."]. PyDict_Update grade 2: Doc/c-api/dict.rst:475 "This is the same as ``PyDict_Merge(a, b, 1)`` in C".
- **short-keyword** "list sort in place" → `PyList_Sort` (grade 3): Objects/listobject.c:3211 — "PyList_Sort(PyObject *v)" [Doc/c-api/list.rst:206 "Sort the items of *list* in place."]. list_sort_impl grade 2: Objects/listobject.c:2944 is the actual timsort implementation PyList_Sort wraps.
- **short-keyword** "object repr" → `PyObject_Repr` (grade 3): Objects/object.c:759 — "PyObject_Repr(PyObject *v)" [Doc/c-api/object.rst:362 "Compute a string representation of object *o*." / :364 "This is the equivalent of the Python expression ``repr(o)``."]. PyObject_Str grade 1 (sibling str()).
- **short-keyword** "type ready" → `PyType_Ready` (grade 3): Objects/typeobject.c:9457 — "PyType_Ready(PyTypeObject *type)" [Doc/c-api/type.rst:226 "Finalize a type object.  This should be called on all type objects to finish their initialization."]
- **short-keyword** "least recently used cache" → `lru_cache` (grade 3): Lib/functools.py:560 — "def lru_cache(maxsize=128, typed=False):" / :561 "Least-recently-used cache decorator.". `cache` grade 2 (path Lib/functools.py; Lib/test/_test_multiprocessing.py also defines `cache`): Lib/functools.py:754 "return lru_cache(maxsize=None)(user_function)".
- **short-keyword** "weak key dict" → `WeakKeyDictionary` (grade 3): Lib/weakref.py:298 — "class WeakKeyDictionary(_collections_abc.MutableMapping):" / :299 "Mapping class that references keys weakly."
- **short-keyword** "coroutine threadsafe submit" → `run_coroutine_threadsafe` (grade 3): Lib/asyncio/tasks.py:1010 — "def run_coroutine_threadsafe(coro, loop):" / :1011 "Submit a coroutine object to a given event loop."

## subset (added 2026-09-03)

- **subset** "AsUTF8" → `PyUnicode_AsUTF8` (grade 3): Objects/unicodeobject.c:4132 — "PyUnicode_AsUTF8(PyObject *unicode)" [Doc/c-api/unicode.rst:1186 "As :c:func:`PyUnicode_AsUTF8AndSize`, but does not store the size."]. PyUnicode_AsUTF8AndSize grade 2: Objects/unicodeobject.c:4108, unicode.rst:1158 "Return a pointer to the UTF-8 encoding of the Unicode object".
- **subset** "intern" → `PyUnicode_InternInPlace` (grade 3): Objects/unicodeobject.c:14805 — "PyUnicode_InternInPlace(PyObject **p)" [Doc/c-api/unicode.rst:1732 "Intern the argument :c:expr:`*p_unicode` in place."]. PyUnicode_InternFromString grade 2: Objects/unicodeobject.c:14821.
- **subset** "richcompare" → `PyObject_RichCompare` (grade 3): Objects/object.c:1099 — "PyObject_RichCompare(PyObject *v, PyObject *w, int op)" [Doc/c-api/object.rst:331 "Compare the values of *o1* and *o2* using the operation specified by *opid*,"]. PyObject_RichCompareBool grade 2: Objects/object.c:1121, object.rst:342 "like :c:func:`PyObject_RichCompare`, but returns ``-1`` on error, ``0`` if the result is false, ``1`` otherwise."
- **subset** "unraisable" → `PyErr_WriteUnraisable` (grade 3): Python/errors.c:1792 — "PyErr_WriteUnraisable(PyObject *obj)" [Doc/c-api/exceptions.rst:80 "Call :func:`sys.unraisablehook` using the current exception and *obj* argument."]. PyErr_FormatUnraisable grade 2: Python/errors.c:1772, exceptions.rst:104 "Similar to :c:func:`PyErr_WriteUnraisable`, but the *format* and subsequent parameters help format the warning message".
- **subset** "generic alias" → `Py_GenericAlias` (grade 3): Objects/genericaliasobject.c:1067 — "Py_GenericAlias(PyObject *origin, PyObject *args)" [Doc/c-api/typehints.rst:14 "Create a :ref:`GenericAlias <types-genericalias>` object."]
- **subset** "from file location" → `spec_from_file_location` (grade 3): Lib/importlib/_bootstrap_external.py:548 — "def spec_from_file_location(name, location=None, *, loader=None," / :550 "Return a module spec based on a file location."

## descriptive (added 2026-09-03)

- **descriptive** "call str on a value" → `PyObject_Str` (grade 3): Objects/object.c:800 — "PyObject_Str(PyObject *v)" [Doc/c-api/object.rst:391 "This is the equivalent of the Python expression ``str(o)``.  Called by the :func:`str` built-in function"]
- **descriptive** "resize list storage capacity" → `list_resize` (grade 3): Objects/listobject.c:94 — "/* Ensure ob_item has room for at least newsize elements, and set" / :98 "The number of allocated elements may grow, shrink, or stay the same." (def at :104)
- **descriptive** "dict grow hash table" → `dictresize` (grade 3): Objects/dictobject.c:2178 — "Restructure the table by allocating a new table and reinserting all" (def at :2192 "dictresize(PyDictObject *mp,")
- **descriptive** "raise exception with format string" → `PyErr_Format` (grade 3): Python/errors.c:1248 — "PyErr_Format(PyObject *exception, const char *format, ...)" [Doc/c-api/exceptions.rst:147 "This function sets the error indicator and returns ``NULL``." / :148 "The *format* and subsequent parameters help format the error message"]. PyErr_SetString grade 1 (no formatting).
- **descriptive** "hash a str object" → `unicode_hash` (grade 3): Objects/unicodeobject.c:12048 — "/* Believe it or not, this produces the same value for ASCII strings as bytes_hash(). */" (def at :12051 "unicode_hash(PyObject *self)")
- **descriptive** "store value under string key" → `PyDict_SetItemString` (grade 3): Objects/dictobject.c:5578 — "PyDict_SetItemString(PyObject *v, const char *key, PyObject *item)" [Doc/c-api/dict.rst:121 "This is the same as :c:func:`PyDict_SetItem`, but *key* is specified as a :c:expr:`const char*` UTF-8 encoded bytes string"]. PyDict_SetItem grade 2 (the PyObject-key variant it wraps).
- **descriptive** "schedule callback soon on event loop" → `call_soon` (grade 3, path Lib/asyncio/base_events.py; also defined as an abstract stub in Lib/asyncio/events.py:308 and in tests): Lib/asyncio/base_events.py:823 — "def call_soon(self, callback, *args, context=None):" / :824 "Arrange for a callback to be called as soon as possible."
- **descriptive** "parse json text into python object" → `loads` (grade 3, path Lib/json/__init__.py; `loads` is also defined in plistlib, tomllib, xmlrpc, pickle tests): Lib/json/__init__.py:313 — "def loads(s, *, cls=None, object_hook=None, parse_float=None," / :316 "Deserialize ``s`` (a ``str``, ``bytes`` or ``bytearray`` instance containing a JSON document) to a Python object."
- **descriptive** "resolve symlinks in path" → `resolve` (grade 3, path Lib/pathlib/__init__.py; `resolve` also defined in pydoc, logging/config, importlib.resources, Tools): Lib/pathlib/__init__.py:1133 — "def resolve(self, strict=False):" / :1135 "Make the path absolute, resolving all symlinks on the way and also"

## paraphrase (added 2026-09-03)

- **paraphrase** "release the interpreter lock" → `drop_gil` (grade 3): Python/ceval_gil.c:217 — "drop_gil(PyInterpreterState *interp, PyThreadState *tstate, int final_release)" / :220 "/* If final_release is true, the caller is indicating that we're releasing" / :221 "the GIL for the last time in this thread.". PyEval_ReleaseThread grade 2: Python/ceval_gil.c:613, public API that calls drop_gil (Doc/c-api/threads.rst:829 "Detach the :term:`attached thread state`.").
- **paraphrase** "C3 linearization of base classes" → `mro_implementation` (grade 3): Objects/typeobject.c:3450 — "mro_implementation(PyTypeObject *type)"; the C3 section comment at :3149 "Method resolution order algorithm C3 described in" / :3150 "\"A Monotonic Superclass Linearization for Dylan\"," heads the block this implements. mro_implementation_unlocked grade 2 (:3362, the body it locks around); mro_internal grade 1 (:3586, caller that also handles a user-defined `mro()`).
- **paraphrase** "run handlers for delivered interrupts" → `PyErr_CheckSignals` (grade 3): Modules/signalmodule.c:1764 — "PyErr_CheckSignals(void)" [Doc/c-api/exceptions.rst:678 "Handle external interruptions, such as signals or activating a debugger," / :679 "whose processing has been delayed until it is safe" / :684 "This function executes the corresponding Python signal handler"]
- **paraphrase** "binary search for where a key belongs in a sorted run" → `gallop_left` (grade 3): Objects/listobject.c:2046 — "Locate the proper position of key in a sorted vector; if the vector contains" / :2061 "key belongs at index k; or, IOW, the first k elements of a should precede" (def at :2067). gallop_right grade 2: Objects/listobject.c:2142 "Exactly like gallop_left(), except that if key already exists in a[0:n]," (def at :2156).
- **paraphrase** "memoize call results" → `cache` (grade 3, path Lib/functools.py; Lib/test/_test_multiprocessing.py also defines `cache`): Lib/functools.py:752 — "def cache(user_function, /):" / :753 "'Simple lightweight unbounded cache.  Sometimes called \"memoize\".'". lru_cache grade 2: Lib/functools.py:560, the bounded variant `cache` delegates to (:754 "return lru_cache(maxsize=None)(user_function)").
- **paraphrase** "clone object recursively" → `deepcopy` (grade 3, path Lib/copy.py; Modules/_elementtree.c:924 and Tools/ftscalingbench define unrelated `deepcopy`): Lib/copy.py:110 — "def deepcopy(x, memo=None):" / :111 "Deep copy operation on arbitrary Python objects."
- **paraphrase** "ignore given errors inside with statement" → `suppress` (grade 3, path Lib/contextlib.py; Lib/test/test_importlib/metadata/_context.py also defines `suppress`): Lib/contextlib.py:495 — "class suppress(AbstractContextManager):" / :496 "Context manager to suppress specified exceptions" / :498 "After the exception is suppressed, execution proceeds with the next" / :499 "statement following the with statement."
- **paraphrase** "turn an error into printable lines" → `format_exception` (grade 3): Lib/traceback.py:188 — "def format_exception(exc, /, value=_sentinel, tb=_sentinel, limit=None, \\" / :190 "Format a stack trace and the exception information." / :193 "to print_exception().  The return value is a list of strings, each" / :194 "ending in a newline". format_exc grade 2: Lib/traceback.py:254 (same for the current exception).
- **paraphrase** "recursively list every subfolder" → `walk` (grade 3, path Lib/os.py; `walk` also defined in ast.py, pathlib, email/iterators.py, tests): Lib/os.py:315 — "def walk(top, topdown=True, onerror=None, followlinks=False):" / :316 "Directory tree generator." / :318 "For each directory in the directory tree rooted at top (including top"

## conjunctive (added 2026-09-03)

- **conjunctive** "list slice assignment" → `list_ass_slice` (grade 3): Objects/listobject.c:1041 — "list_ass_slice(PyListObject *a, Py_ssize_t ilow, Py_ssize_t ihigh, PyObject *v)"; concept proof on the worker it wraps at :943 "/* a[ilow:ihigh] = v if v != NULL." / :944 "del a[ilow:ihigh] if v == NULL.". list_ass_slice_lock_held grade 2 (:950, carries that comment and does the work); list_ass_subscript grade 1 (:3896, the mp_ass_subscript entry that dispatches slice objects to it).
- **conjunctive** "utf-8 decode incremental consumed" → `PyUnicode_DecodeUTF8Stateful` (grade 3): Objects/unicodeobject.c:5388 — "PyUnicode_DecodeUTF8Stateful(const char *s," [Doc/c-api/unicode.rst:1140 "If *consumed* is ``NULL``, behave like :c:func:`PyUnicode_DecodeUTF8`. If" / :1141 "*consumed* is not ``NULL``, trailing incomplete UTF-8 byte sequences will not be" / :1142 "treated as an error."]. PyUnicode_DecodeUTF8 grade 2 (non-incremental form, same file).
- **conjunctive** "rotating log file by size" → `RotatingFileHandler` (grade 3): Lib/logging/handlers.py:125 — "class RotatingFileHandler(BaseRotatingHandler):" / :127 "Handler for logging to a set of files, which switches from one file" / :128 "to the next when the current file reaches a certain size.". BaseRotatingHandler grade 2 (:51, shared rotation base); TimedRotatingFileHandler grade 1 (:215, rotates by time not size).
- **conjunctive** "regex compile with cache" → `_compile` (grade 3, path Lib/re/__init__.py; `_compile` also defined in codeop.py, tokenize.py, re/_compiler.py): Lib/re/__init__.py:332 — "def _compile(pattern, flags):" / :333 "# internal: compile pattern" / :342 "# Item in _cache should be moved to the end if found."
- **conjunctive** "asyncio wait with timeout" → `wait_for` (grade 3, path Lib/asyncio/tasks.py; `wait_for` also defined in threading.py, multiprocessing, asyncio/locks.py, tests): Lib/asyncio/tasks.py:440 — "async def wait_for(fut, timeout):" / :441 "Wait for the single Future or coroutine to complete, with timeout."
