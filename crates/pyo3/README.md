# vorpal python binding

[![PyPI](https://img.shields.io/pypi/v/vorpal-py.svg?logo=PyPI)](https://pypi.org/project/vorpal-py/)
[![Website](https://img.shields.io/badge/vorpal-Vorpal_Website-red?logoColor=red)](https://vorpal.github.io/)

<p align=center>
  <img src="https://vorpal.github.io/logo.svg" alt="vorpal"/>
</p>

## vorpal

`vorpal` is a tool for code structural search, lint, and rewriting. 

This crate intends to build a native python binding of vorpal and provide a python API for programmatic usage.

## Installation

```bash
pip install vorpal-py
```

## Usage

You can take our tests as examples. For example, [test_simple.py](./tests/test_simple.py) shows how to use vorpal to search for a pattern in a file.

Please see the [API usage guide](https://vorpal.github.io/guide/api-usage.html) and [API reference](https://vorpal.github.io/reference/api.html) for more details.

Other resources include [vorpal's official site](https://vorpal.github.io/) and [repository](https://github.com/hyper-light/vorpal).

## Development

### Setup virtualenv

```shell
python -m venv venv
```

### Activate venv

```shell
source venv/bin/activate
```

### Install `maturin`

```shell
pip install maturin[patchelf]
```

### MacOS: Install `patchelf` and `maturin`

```shell
brew install patchelf
pip install maturin
```

### Build bindings

```shell
maturin develop
```

### Run tests

```shell
pytest
```

All tests files are under [tests](./tests) directory.

## License

This project is licensed under the MIT license.
