"""Async bridge: index + fan-out searches without blocking the event loop.

    pip install vorpal-py
"""
import asyncio
import vorpal_py


async def main() -> None:
    print(await vorpal_py.build(".", ".vorpal/index"))

    # One query...
    print(await vorpal_py.search(".vorpal/index", "http handler", 5))

    # ...or many, resolved concurrently on the Rust side.
    results = await vorpal_py.search_many(
        ".vorpal/index",
        ["database migration", "signal handling", "argument parsing"],
        k=3,
    )
    for query, rendered in zip(["database migration", "signal handling", "argument parsing"], results):
        print(f"== {query}\n{rendered}")

    # Graph + node lookups have async twins too.
    print(await vorpal_py.graph(".vorpal/index", "callers", "main"))

    # The whole optional ranking stack is awaitable as well:
    #   await vorpal_py.search_ranked(".vorpal/index", "retry backoff", k=5)
    #   await vorpal_py.tune(".vorpal/index", [("parse config", "parse_config")], k=10)
    #   await vorpal_py.enable("semantic-f16")   # weights download off the loop

    # And the pinned Index class: sync reads are sub-millisecond, the *_async twins
    # run GIL-free on the worker pool — fan out freely.
    index = vorpal_py.Index.open(".vorpal/index")
    listing, callers = await asyncio.gather(
        index.nodes_async("main"),
        index.related_async("callers", "main"),
    )
    print(listing, callers)


asyncio.run(main())
