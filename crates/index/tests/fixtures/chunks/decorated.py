import os

def plain(path):
    return os.stat(path)


@property
def wrapped(self):
    return os.stat(self.path)


class Holder:
    @staticmethod
    def make(path):
        return os.stat(path)

    def read(self):
        return os.stat(self.path)


if os.environ.get("DEBUG"):
    def debug_stat(path):
        return os.stat(path)


def last(path):
    if path:
        return os.stat(path)
    return None
