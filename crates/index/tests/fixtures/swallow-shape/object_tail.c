/* Trimmed from cpython Objects/object.c: the parser-swallow shape. The bare
   statement-position macros in _PyObject_GetAttrId wreck its body; tree-sitter loses
   the closing brace and parses EVERY later definition inside that body — no ERROR
   node at top level, byte-ratio health calls the file clean, and without recovery
   nothing after line 20 exists in the index. */
#include "Python.h"

static int counter = 0;

Py_hash_t
PyObject_Hash(PyObject *v)
{
    return (Py_hash_t)counter;
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
    if (tp->tp_getattro != NULL) {
        return (*tp->tp_getattro)(v, name);
    }
    return NULL;
}

int
PyObject_SetAttr(PyObject *v, PyObject *name, PyObject *value)
{
    PyTypeObject *tp = Py_TYPE(v);
    return tp->tp_setattro(v, name, value);
}

/* Test a value used as condition, e.g., in a while or if statement. */
int
PyObject_IsTrue(PyObject *v)
{
    if (v == Py_True)
        return 1;
    return PyObject_Hash(v) != 0;
}

int
PyObject_Not(PyObject *v)
{
    int res = PyObject_IsTrue(v);
    return res < 0 ? res : res == 0;
}

int
PyCallable_Check(PyObject *x)
{
    if (x == NULL)
        return 0;
    return PyObject_GetAttr(x, NULL) != NULL;
}

static PyNumberMethods none_as_number = {
    0,
};

PyObject _Py_NoneStruct = _PyObject_HEAD_INIT(&_PyNone_Type);

PyObject *
PyObject_GenericGetAttr(PyObject *obj, PyObject *name)
{
    return PyObject_GetAttr(obj, name);
}
